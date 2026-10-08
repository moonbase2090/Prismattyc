//! IOSurface-backed contents for the macOS Core Animation presenter.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CGRect};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth, IOSurfaceLockOptions, IOSurfaceRef,
};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::frame_damage::FrameDamage;
use crate::pixel_alpha::premultiply_in_place;
use crate::present_timing::PresentTiming;
use crate::surface_damage::{SurfaceDamageHistory, SURFACE_COUNT};

const MAX_FREE_SURFACE_WAIT: Duration = Duration::from_millis(8);
const SURFACE_POLL_INTERVAL: Duration = Duration::from_micros(100);

// SAFETY: these signatures match IOSurface.framework's documented C API.
unsafe extern "C" {
    #[link_name = "IOSurfaceLock"]
    fn iosurface_lock(surface: &IOSurfaceRef, options: IOSurfaceLockOptions, seed: *mut u32)
        -> i32;
    #[link_name = "IOSurfaceUnlock"]
    fn iosurface_unlock(
        surface: &IOSurfaceRef,
        options: IOSurfaceLockOptions,
        seed: *mut u32,
    ) -> i32;
}

/// Owns one retained reference to each surface. Core Animation retains the
/// surface assigned to `CALayer.contents`; this ring keeps the three CPU
/// buffers alive for writes and readback.
pub(crate) struct SurfaceRing {
    surfaces: [CFRetained<IOSurfaceRef>; SURFACE_COUNT],
    damage: SurfaceDamageHistory,
    width: usize,
    height: usize,
    scale: f64,
    generation: u64,
}

impl SurfaceRing {
    pub(crate) fn new(width: usize, height: usize, scale: f64) -> Result<Self> {
        ensure!(
            scale.is_finite() && scale > 0.0,
            "IOSurface scale must be positive"
        );
        let surfaces = make_surfaces(width, height)?;
        Ok(Self {
            surfaces,
            damage: SurfaceDamageHistory::new(width, height),
            width,
            height,
            scale,
            generation: 1,
        })
    }

    /// Reallocate atomically when pixel geometry or display scale changes.
    /// Returns true when every surface was replaced and a full copy is needed.
    pub(crate) fn resize_if_needed(
        &mut self,
        width: usize,
        height: usize,
        scale: f64,
    ) -> Result<bool> {
        ensure!(
            scale.is_finite() && scale > 0.0,
            "IOSurface scale must be positive"
        );
        if (width, height, scale.to_bits()) == (self.width, self.height, self.scale.to_bits()) {
            return Ok(false);
        }
        let surfaces = make_surfaces(width, height)?;
        self.surfaces = surfaces;
        self.damage = SurfaceDamageHistory::new(width, height);
        self.width = width;
        self.height = height;
        self.scale = scale;
        self.generation = self.generation.saturating_add(1);
        Ok(true)
    }

    pub(crate) fn present(
        &mut self,
        layer: &CALayer,
        frame: CGRect,
        scale: f64,
        pixels: &[u32],
        damage: FrameDamage,
        measure: bool,
    ) -> Result<PresentTiming> {
        let expected_len = self
            .width
            .checked_mul(self.height)
            .context("IOSurface framebuffer dimensions overflow")?;
        ensure!(
            pixels.len() == expected_len,
            "IOSurface framebuffer length mismatch"
        );

        let write_started = measure.then(Instant::now);
        let (index, busy_surface_stalls) = self.free_surface(measure)?;
        let plan = self.damage.plan(index, &damage);
        let write_bytes = if measure {
            plan.iter()
                .map(|rect| rect.width.saturating_mul(rect.height).saturating_mul(4))
                .fold(0usize, usize::saturating_add)
        } else {
            0
        };
        write_rects(
            &self.surfaces[index],
            pixels,
            self.width,
            self.height,
            &plan,
        )?;
        let write_us = write_started.map_or(0, elapsed_us);

        let commit_started = measure.then(Instant::now);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setFrame(frame);
        layer.setContentsScale(scale);
        // SAFETY: IOSurface is a documented Core Foundation object bridged to
        // Objective-C. CALayer retains its contents before the ring can drop it.
        let object = unsafe {
            &*(CFRetained::as_ptr(&self.surfaces[index])
                .as_ptr()
                .cast::<AnyObject>())
        };
        // SAFETY: the object above is a live IOSurface retained by this ring;
        // CALayer.contents accepts the bridged IOSurface object.
        unsafe { layer.setContents(Some(object)) };
        CATransaction::commit();
        let commit_us = commit_started.map_or(0, elapsed_us);

        self.damage.committed(index, &damage);
        Ok(PresentTiming {
            backend: "iosurface",
            write_us,
            commit_us,
            dirty_tiles: 0,
            changed_tiles: None,
            write_bytes,
            busy_surface_stalls,
        })
    }

    pub(crate) fn readback(&self) -> Result<Vec<u32>> {
        let index = self
            .damage
            .current_index()
            .context("IOSurface has no presented frame")?;
        let surface = &self.surfaces[index];
        let mut lock = SurfaceLock::new(surface, IOSurfaceLockOptions::ReadOnly)?;
        let stride_bytes = surface.bytes_per_row();
        ensure!(stride_bytes % std::mem::size_of::<u32>() == 0);
        let stride = stride_bytes / std::mem::size_of::<u32>();
        ensure!(stride >= self.width, "IOSurface row stride is too short");
        let surface_words = stride
            .checked_mul(self.height)
            .context("IOSurface readback allocation size overflow")?;
        let base = surface.base_address().cast::<u32>().as_ptr();
        let mut pixels = Vec::with_capacity(self.width * self.height);
        for y in 0..self.height {
            let offset = y
                .checked_mul(stride)
                .context("IOSurface readback offset overflow")?;
            let end = offset
                .checked_add(self.width)
                .context("IOSurface readback end overflow")?;
            ensure!(
                end <= surface_words,
                "IOSurface readback row exceeds allocation"
            );
            // SAFETY: the surface is locked read-only, `offset` is within the
            // checked row stride, and `width` is within the allocated surface.
            // IOSurface base addresses are aligned for their 32-bit elements.
            let row = unsafe { std::slice::from_raw_parts(base.add(offset), self.width) };
            pixels.extend_from_slice(row);
        }
        lock.unlock()?;
        Ok(pixels)
    }

    pub(crate) fn current_slot(&self) -> Option<usize> {
        self.damage.current_index()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn free_surface(&self, measure: bool) -> Result<(usize, u64)> {
        if let Some(index) = self
            .damage
            .next_free(|index| self.surfaces[index].is_in_use())
        {
            return Ok((index, 0));
        }

        let stalled = u64::from(measure);
        let deadline = Instant::now() + MAX_FREE_SURFACE_WAIT;
        while Instant::now() < deadline {
            thread::sleep(SURFACE_POLL_INTERVAL);
            if let Some(index) = self
                .damage
                .next_free(|index| self.surfaces[index].is_in_use())
            {
                return Ok((index, stalled));
            }
        }
        bail!("all IOSurface present buffers remained busy")
    }
}

fn make_surfaces(width: usize, height: usize) -> Result<[CFRetained<IOSurfaceRef>; SURFACE_COUNT]> {
    ensure!(
        width > 0 && height > 0,
        "IOSurface dimensions must be nonzero"
    );
    let first = make_surface(width, height)?;
    let second = make_surface(width, height)?;
    let third = make_surface(width, height)?;
    Ok([first, second, third])
}

fn make_surface(width: usize, height: usize) -> Result<CFRetained<IOSurfaceRef>> {
    let row_bytes = width
        .checked_mul(4)
        .context("IOSurface row size overflow")?;
    let row_bytes = row_bytes
        .checked_add(63)
        .context("IOSurface row alignment overflow")?
        & !63;
    let width_number = CFNumber::new_isize(isize::try_from(width)?);
    let height_number = CFNumber::new_isize(isize::try_from(height)?);
    let element_number = CFNumber::new_isize(4);
    let row_number = CFNumber::new_isize(isize::try_from(row_bytes)?);
    let format_number = CFNumber::new_i32(i32::from_be_bytes(*b"BGRA"));

    // SAFETY: IOSurface exports these immutable CFString keys for the process
    // lifetime; each is a documented property accepted by IOSurfaceCreate.
    let keys: [&CFString; 5] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfaceBytesPerRow,
            kIOSurfacePixelFormat,
        ]
    };
    let values: [&CFNumber; 5] = [
        &width_number,
        &height_number,
        &element_number,
        &row_number,
        &format_number,
    ];
    let properties = CFDictionary::from_slices(&keys, &values);
    // SAFETY: `properties` contains IOSurface's required dimensions, stride,
    // element size, and pixel format values with valid Core Foundation types.
    let surface = unsafe { IOSurfaceRef::new(properties.as_opaque()) }
        .context("IOSurfaceCreate returned null")?;
    ensure!(surface.width() == width && surface.height() == height);
    ensure!(surface.bytes_per_element() == 4);
    ensure!(surface.bytes_per_row() >= row_bytes);
    ensure!(
        surface.pixel_format() == u32::from_be_bytes(*b"BGRA"),
        "IOSurface did not retain the requested BGRA format"
    );
    Ok(surface)
}

fn write_rects(
    surface: &IOSurfaceRef,
    pixels: &[u32],
    width: usize,
    height: usize,
    rects: &[crate::frame_damage::PixelRect],
) -> Result<()> {
    let mut lock = SurfaceLock::new(surface, IOSurfaceLockOptions::empty())?;
    let stride_bytes = surface.bytes_per_row();
    ensure!(stride_bytes % std::mem::size_of::<u32>() == 0);
    let stride = stride_bytes / std::mem::size_of::<u32>();
    ensure!(stride >= width, "IOSurface row stride is too short");
    let surface_words = stride
        .checked_mul(height)
        .context("IOSurface allocation size overflow")?;
    let base = surface.base_address().cast::<u32>().as_ptr();

    for rect in rects {
        let right = rect
            .x
            .checked_add(rect.width)
            .context("IOSurface damage right edge overflow")?;
        let bottom = rect
            .y
            .checked_add(rect.height)
            .context("IOSurface damage bottom edge overflow")?;
        ensure!(
            right <= width && bottom <= height,
            "IOSurface damage is out of bounds"
        );
        for y in rect.y..bottom {
            let src_start = y
                .checked_mul(width)
                .and_then(|row| row.checked_add(rect.x))
                .context("IOSurface source offset overflow")?;
            let src_end = src_start
                .checked_add(rect.width)
                .context("IOSurface source end overflow")?;
            let source = pixels
                .get(src_start..src_end)
                .context("IOSurface source rectangle exceeds framebuffer")?;
            let dst_start = y
                .checked_mul(stride)
                .and_then(|row| row.checked_add(rect.x))
                .context("IOSurface destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(rect.width)
                .context("IOSurface destination end overflow")?;
            ensure!(
                dst_end <= surface_words,
                "IOSurface rectangle exceeds allocation"
            );
            // SAFETY: the IOSurface is write-locked, and checked row offsets
            // bound this slice to the allocated surface and clipped damage.
            let destination =
                unsafe { std::slice::from_raw_parts_mut(base.add(dst_start), rect.width) };
            destination.copy_from_slice(source);
            premultiply_in_place(destination);
        }
    }
    lock.unlock()?;
    Ok(())
}

struct SurfaceLock<'a> {
    surface: &'a IOSurfaceRef,
    options: IOSurfaceLockOptions,
    locked: bool,
}

impl<'a> SurfaceLock<'a> {
    fn new(surface: &'a IOSurfaceRef, options: IOSurfaceLockOptions) -> Result<Self> {
        // SAFETY: the surface is live and the documented API permits a null seed pointer.
        let status = unsafe { iosurface_lock(surface, options, std::ptr::null_mut()) };
        ensure!(status == 0, "IOSurfaceLock failed with status {status}");
        Ok(Self {
            surface,
            options,
            locked: true,
        })
    }

    fn unlock(&mut self) -> Result<()> {
        if !self.locked {
            return Ok(());
        }
        // SAFETY: this guard owns the matching successful lock operation.
        let status = unsafe { iosurface_unlock(self.surface, self.options, std::ptr::null_mut()) };
        self.locked = false;
        ensure!(status == 0, "IOSurfaceUnlock failed with status {status}");
        Ok(())
    }
}

impl Drop for SurfaceLock<'_> {
    fn drop(&mut self) {
        if self.locked {
            // SAFETY: Drop pairs an outstanding lock from `new` exactly once.
            let _ = unsafe { iosurface_unlock(self.surface, self.options, std::ptr::null_mut()) };
            self.locked = false;
        }
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
