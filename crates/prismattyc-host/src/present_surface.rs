//! SPIKE (spike/present-cost): IOSurface-backed Core Animation present.
//!
//! One content layer shows one of three IOSurfaces. Each frame copies and
//! premultiplies only the damaged rectangles (plus rectangles the chosen
//! surface missed while the others were shown) into a surface the window
//! server is not reading, then swaps it in. The commit carries a surface
//! reference instead of image bytes. `PRISMATTYC_SPIKE_PRESENT=iosurface`.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CGRect};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
    kIOSurfaceWidth, IOSurfaceLockOptions, IOSurfaceRef,
};
use objc2_quartz_core::{kCAGravityTopLeft, CALayer, CATransaction};

use crate::frame_damage::{FrameDamage, PixelRect};
use crate::pixel_alpha::premultiply_in_place;

const SURFACES: usize = 3;
/// Past this many pending rectangles a surface just takes a full copy.
const MAX_STALE_RECTS: usize = 256;

pub struct SurfaceRing {
    layer: Retained<CALayer>,
    surfaces: Vec<CFRetained<IOSurfaceRef>>,
    /// Rectangles each surface lacks because they changed while it was idle.
    stale: Vec<Vec<PixelRect>>,
    current: Option<usize>,
    width: usize,
    height: usize,
}

impl SurfaceRing {
    pub fn new(parent: &CALayer) -> Self {
        let layer = CALayer::new();
        layer.setOpaque(false);
        layer.setAnchorPoint(objc2_core_foundation::CGPoint::new(0.0, 0.0));
        layer.setGeometryFlipped(true);
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        parent.addSublayer(&layer);
        Self {
            layer,
            surfaces: Vec::new(),
            stale: Vec::new(),
            current: None,
            width: 0,
            height: 0,
        }
    }

    /// Reallocate for a new framebuffer size; call inside a transaction.
    pub fn resize(&mut self, width: usize, height: usize, frame: CGRect, scale: f64) -> Result<()> {
        self.layer.setFrame(frame);
        self.layer.setContentsScale(scale);
        if (width, height) != (self.width, self.height) {
            self.surfaces = (0..SURFACES)
                .map(|_| surface(width, height))
                .collect::<Result<_>>()?;
            let full = PixelRect::new(0, 0, width, height);
            self.stale = vec![vec![full]; SURFACES];
            self.current = None;
            self.width = width;
            self.height = height;
        }
        Ok(())
    }

    pub fn present(&mut self, pixels: &[u32], damage: &FrameDamage) -> Result<()> {
        let full = PixelRect::new(0, 0, self.width, self.height);
        let damage: Vec<PixelRect> = match damage {
            FrameDamage::Full => vec![full],
            FrameDamage::Rects(rects) => rects.iter().filter_map(|r| clip(*r, full)).collect(),
        };
        let started = Instant::now();
        let index = self.pick();
        let mut rects = std::mem::take(&mut self.stale[index]);
        rects.extend_from_slice(&damage);
        if rects.contains(&full) || rects.len() > MAX_STALE_RECTS {
            rects = vec![full];
        }
        let bytes = write(&self.surfaces[index], pixels, self.width, &rects);
        for (other, stale) in self.stale.iter_mut().enumerate() {
            if other != index {
                stale.extend_from_slice(&damage);
            }
        }
        crate::spike_timing::record("present.surface_write", started.elapsed());
        crate::spike_timing::value("present.surface_write_kib", bytes / 1024);
        let started = Instant::now();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // SAFETY: IOSurfaceRef is toll-free bridged to IOSurface, which
        // CALayer accepts as contents and retains.
        let object =
            unsafe { &*(CFRetained::as_ptr(&self.surfaces[index]).as_ptr() as *const AnyObject) };
        unsafe { self.layer.setContents(Some(object)) };
        CATransaction::commit();
        crate::spike_timing::record("present.surface_commit", started.elapsed());
        self.current = Some(index);
        Ok(())
    }

    /// The next surface the window server is not using, else the oldest.
    fn pick(&self) -> usize {
        let start = self.current.map_or(0, |current| current + 1);
        for step in 0..SURFACES {
            let index = (start + step) % SURFACES;
            if !self.surfaces[index].is_in_use() {
                return index;
            }
        }
        crate::spike_timing::count("present.surface", "all_busy");
        start % SURFACES
    }

    /// Pixels of the shown surface that differ from the premultiplied
    /// framebuffer. With `dump`, also write the readback as PNG next to it.
    pub fn verify(&self, pixels: &[u32], dump: Option<&Path>) -> usize {
        let Some(index) = self.current else { return 0 };
        let surface = &self.surfaces[index];
        lock(surface, IOSurfaceLockOptions::ReadOnly);
        let stride = surface.bytes_per_row() / 4;
        let base = surface.base_address().as_ptr() as *const u32;
        let mut readback = Vec::with_capacity(self.width * self.height);
        for y in 0..self.height {
            // SAFETY: locked surface with `height` rows of `stride` pixels.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), self.width) };
            readback.extend_from_slice(row);
        }
        unlock(surface, IOSurfaceLockOptions::ReadOnly);
        let mut expected = pixels.to_vec();
        premultiply_in_place(&mut expected);
        let mismatches = readback
            .iter()
            .zip(&expected)
            .filter(|(a, b)| a != b)
            .count();
        if let Some(path) = dump {
            // The dump is rewritten every frame; keep this frame's copy beside
            // the readback so the pair can be compared byte for byte.
            let _ = std::fs::copy(path, path.with_extension("atverify.png"));
            let _ = crate::write_present_png(
                &path.with_extension("readback.png"),
                &readback,
                self.width as u32,
                self.height as u32,
            );
        }
        mismatches
    }
}

fn clip(rect: PixelRect, bounds: PixelRect) -> Option<PixelRect> {
    let x2 = (rect.x + rect.width).min(bounds.width);
    let y2 = (rect.y + rect.height).min(bounds.height);
    (rect.x < x2 && rect.y < y2).then(|| PixelRect::new(rect.x, rect.y, x2 - rect.x, y2 - rect.y))
}

fn surface(width: usize, height: usize) -> Result<CFRetained<IOSurfaceRef>> {
    let row = (width * 4).next_multiple_of(64);
    // SAFETY: framework constants are valid for the process lifetime.
    let keys: [&CFString; 5] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfaceBytesPerRow,
            kIOSurfacePixelFormat,
        ]
    };
    let numbers = [
        CFNumber::new_isize(width as isize),
        CFNumber::new_isize(height as isize),
        CFNumber::new_isize(4),
        CFNumber::new_isize(row as isize),
        CFNumber::new_i32(i32::from_be_bytes(*b"BGRA")),
    ];
    let values: Vec<&CFNumber> = numbers.iter().map(|number| &**number).collect();
    let properties = CFDictionary::from_slices(&keys, &values);
    // SAFETY: a valid IOSurface property dictionary.
    unsafe { IOSurfaceRef::new(properties.as_opaque()) }.context("IOSurfaceCreate")
}

fn lock(surface: &IOSurfaceRef, options: IOSurfaceLockOptions) {
    // SAFETY: a null seed pointer is allowed.
    unsafe { surface.lock(options, std::ptr::null_mut()) };
}

fn unlock(surface: &IOSurfaceRef, options: IOSurfaceLockOptions) {
    // SAFETY: paired with `lock`.
    unsafe { surface.unlock(options, std::ptr::null_mut()) };
}

/// Copy and premultiply `rects` of the straight framebuffer into `surface`.
fn write(surface: &IOSurfaceRef, pixels: &[u32], width: usize, rects: &[PixelRect]) -> u64 {
    lock(surface, IOSurfaceLockOptions::empty());
    let stride = surface.bytes_per_row() / 4;
    let base = surface.base_address().as_ptr() as *mut u32;
    let mut bytes = 0;
    for rect in rects {
        for y in rect.y..rect.y + rect.height {
            let src = &pixels[y * width + rect.x..y * width + rect.x + rect.width];
            // SAFETY: locked surface; rect is clipped to the framebuffer.
            let dst = unsafe {
                std::slice::from_raw_parts_mut(base.add(y * stride + rect.x), rect.width)
            };
            dst.copy_from_slice(src);
            premultiply_in_place(dst);
        }
        bytes += (rect.width * rect.height * 4) as u64;
    }
    unlock(surface, IOSurfaceLockOptions::empty());
    bytes
}
