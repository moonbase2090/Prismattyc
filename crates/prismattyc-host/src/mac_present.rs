//! Alpha-capable presentation above the AppKit blur backdrop.
//!
//! Keep straight ARGB pixels between frames. Only damaged tiles are copied,
//! premultiplied, and published as immutable Core Graphics images. Core
//! Animation retains the other tile images without a full-frame conversion.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use objc2::{rc::Retained, MainThreadMarker};
use objc2_app_kit::NSView;
use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_quartz_core::{kCAGravityTopLeft, CALayer, CATransaction};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::frame_damage::{FrameDamage, PixelRect};
use crate::pixel_alpha::premultiply_in_place;
use crate::present_tiles::{copy_tile, damaged_tiles, tiles};

pub struct MacPresent {
    layer: Retained<CALayer>,
    root_layer: Retained<CALayer>,
    view: Retained<NSView>,
    color_space: CFRetained<CGColorSpace>,
    pixels: Vec<u32>,
    tile_rects: Vec<PixelRect>,
    tile_layers: Vec<Retained<CALayer>>,
    scratch: Vec<u32>,
    retained: bool,
    rebuild_layers: bool,
    scale: f64,
    width: usize,
    height: usize,
    // Retain the window until the layer and blur view have been removed.
    window: Arc<Window>,
    // AppKit operations, including Drop, stay on the main thread.
    _main_thread: MainThreadMarker,
    /// SPIKE: `PRISMATTYC_SPIKE_PRESENT_THREAD=1` moves image build and
    /// commit to a present thread. Declared last so it joins after use.
    thread: Option<crate::present_thread::PresentThread>,
    /// SPIKE (present-cost): last presented straight pixels for the tile
    /// diff oracle (`PRISMATTYC_SPIKE_TILE_DIFF=1`).
    previous: Option<Vec<u32>>,
    /// SPIKE (present-cost): `PRISMATTYC_SPIKE_PRESENT=iosurface`.
    surface: Option<crate::present_surface::SurfaceRing>,
    frames: u64,
}

impl MacPresent {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let main_thread =
            MainThreadMarker::new().context("Mac presenter requires the main thread")?;
        let handle = window.window_handle()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            bail!("Mac presenter requires an AppKit view");
        };
        // SAFETY: winit supplies a live NSView, held alive by `window`.
        let view = unsafe { Retained::retain(handle.ns_view.cast::<NSView>().as_ptr()) }
            .context("retain AppKit view")?;
        view.setWantsLayer(true);
        let root_layer = view.layer().context("AppKit view has no backing layer")?;
        let color_space = CGColorSpace::new_device_rgb().context("create RGB color space")?;
        let layer = CALayer::new();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setOpaque(false);
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setGeometryFlipped(true);
        // Keep terminal content above backdrop subviews added by hot reload.
        layer.setZPosition(1.0);
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        root_layer.addSublayer(&layer);
        let surface = (std::env::var("PRISMATTYC_SPIKE_PRESENT").as_deref() == Ok("iosurface"))
            .then(|| crate::present_surface::SurfaceRing::new(&layer));
        CATransaction::commit();
        Ok(Self {
            layer,
            root_layer,
            view,
            color_space,
            pixels: Vec::new(),
            tile_rects: Vec::new(),
            tile_layers: Vec::new(),
            scratch: Vec::new(),
            retained: false,
            rebuild_layers: true,
            scale: 0.0,
            width: 0,
            height: 0,
            window,
            _main_thread: main_thread,
            previous: None,
            surface,
            frames: 0,
            thread: std::env::var_os("PRISMATTYC_SPIKE_PRESENT_THREAD")
                .map(|_| crate::present_thread::PresentThread::spawn()),
        })
    }

    /// Return whether partial raster can reuse the last successful frame.
    pub fn prepare(&mut self, width: u32, height: u32) -> Result<bool> {
        let width = width as usize;
        let height = height as usize;
        let len = width
            .checked_mul(height)
            .context("Mac framebuffer size overflow")?;
        if len == 0 {
            bail!("Mac framebuffer must be nonempty");
        }
        let resized = self.width != width || self.height != height;
        let retained = self.retained && !resized;
        // An error before presentation must force a full repaint next time.
        self.retained = false;
        if resized {
            self.tile_rects = tiles(width, height);
            self.rebuild_layers = true;
        }
        self.pixels.resize(len, 0);
        self.width = width;
        self.height = height;
        Ok(retained)
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    pub fn present(&mut self, damage: FrameDamage) -> Result<()> {
        let damage = if self.rebuild_layers {
            FrameDamage::Full
        } else {
            damage
        };
        if self.surface.is_some() {
            return self.present_surface(damage);
        }
        let dirty = damaged_tiles(&self.tile_rects, &damage);
        let dirty = self.spike_tile_diff(dirty);
        if self.thread.is_some() {
            return self.present_threaded(dirty);
        }
        // Prepare every replacement before changing the layer tree. Images own
        // immutable data, so reuse of scratch never races the compositor.
        let images_started = std::time::Instant::now();
        crate::spike_timing::record(
            "present.dirty_tiles",
            std::time::Duration::from_micros(dirty.len() as u64),
        );
        let mut images = Vec::with_capacity(dirty.len());
        for index in dirty {
            let tile = self.tile_rects[index];
            copy_tile(&self.pixels, self.width, tile, &mut self.scratch);
            premultiply_in_place(&mut self.scratch);
            images.push((
                index,
                alpha_image(&self.scratch, tile.width, tile.height, &self.color_space)?,
            ));
        }
        crate::spike_timing::record("present.tile_images", images_started.elapsed());
        let commit_started = std::time::Instant::now();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.layer.setFrame(self.root_layer.bounds());
        let scale = self.window.scale_factor();
        if self.rebuild_layers {
            for layer in self.tile_layers.drain(..) {
                layer.removeFromSuperlayer();
            }
            for _ in &self.tile_rects {
                let layer = CALayer::new();
                layer.setOpaque(false);
                layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
                layer.setGeometryFlipped(true);
                layer.setContentsGravity(unsafe { kCAGravityTopLeft });
                self.layer.addSublayer(&layer);
                self.tile_layers.push(layer);
            }
        }
        // Pixel boundaries divided by the backing scale keep adjacent tiles
        // aligned on Retina displays, including the short right/bottom tiles.
        if self.rebuild_layers || self.scale != scale {
            for (layer, tile) in self.tile_layers.iter().zip(&self.tile_rects) {
                // Framebuffer rows use winit's top-left view coordinates.
                // AppKit backing layers and this container can have different
                // Y axes. Convert through both spaces instead of assigning a
                // view rectangle directly as a sublayer frame.
                let view_rect = CGRect::new(
                    CGPoint::new(tile.x as f64 / scale, tile.y as f64 / scale),
                    CGSize::new(tile.width as f64 / scale, tile.height as f64 / scale),
                );
                let root_rect = self.view.convertRectToLayer(view_rect);
                let frame = self
                    .layer
                    .convertRect_fromLayer(root_rect, Some(&self.root_layer));
                layer.setFrame(frame);
                layer.setContentsScale(scale);
            }
        }
        for (index, image) in images {
            // SAFETY: CALayer accepts a CGImage and retains its immutable data.
            unsafe { self.tile_layers[index].setContents(Some(image.as_ref())) };
        }
        CATransaction::commit();
        spike_flush();
        crate::spike_timing::record("present.ca_commit", commit_started.elapsed());
        self.scale = scale;
        self.rebuild_layers = false;
        self.retained = true;
        Ok(())
    }
}

/// SPIKE (present-cost): `PRISMATTYC_SPIKE_FLUSH=1` flushes after commit so
/// main-thread timing includes work AppKit's implicit transaction would
/// otherwise defer to the run-loop observer.
fn spike_flush() {
    if std::env::var_os("PRISMATTYC_SPIKE_FLUSH").is_some() {
        CATransaction::flush();
    }
}

impl MacPresent {
    /// SPIKE (present-cost): IOSurface ring instead of CGImage tiles.
    fn present_surface(&mut self, damage: FrameDamage) -> Result<()> {
        let scale = self.window.scale_factor();
        let ring = self.surface.as_mut().expect("surface present");
        if self.rebuild_layers || self.scale != scale {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.layer.setFrame(self.root_layer.bounds());
            let view_rect = CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(self.width as f64 / scale, self.height as f64 / scale),
            );
            let root_rect = self.view.convertRectToLayer(view_rect);
            let frame = self
                .layer
                .convertRect_fromLayer(root_rect, Some(&self.root_layer));
            let resized = ring.resize(self.width, self.height, frame, scale);
            CATransaction::commit();
            resized?;
        }
        let started = std::time::Instant::now();
        ring.present(&self.pixels, &damage)?;
        spike_flush();
        crate::spike_timing::record("present.surface_total", started.elapsed());
        self.frames += 1;
        if std::env::var_os("PRISMATTYC_SPIKE_VERIFY").is_some() && self.frames % 30 == 0 {
            let dump = std::env::var_os("PRISMATTYC_DUMP_PRESENT").map(std::path::PathBuf::from);
            let mismatches = ring.verify(&self.pixels, dump.as_deref());
            crate::spike_timing::value("present.verify_mismatched_pixels", mismatches as u64);
        }
        self.scale = scale;
        self.rebuild_layers = false;
        self.retained = true;
        Ok(())
    }

    /// SPIKE (present-cost): count dirty tiles whose pixels really changed
    /// since the last present, optionally present only those, and audit
    /// undamaged tiles for changes the damage missed.
    fn spike_tile_diff(&mut self, dirty: Vec<usize>) -> Vec<usize> {
        if std::env::var_os("PRISMATTYC_SPIKE_TILE_DIFF").is_none() {
            return dirty;
        }
        let len = self.pixels.len();
        let Some(previous) = self.previous.as_mut().filter(|p| p.len() == len) else {
            self.previous = Some(self.pixels.clone());
            return dirty;
        };
        let started = std::time::Instant::now();
        let width = self.width;
        let differs = |tile: PixelRect, a: &[u32], b: &[u32]| {
            (tile.y..tile.y + tile.height).any(|y| {
                let row = y * width + tile.x..y * width + tile.x + tile.width;
                a[row.clone()] != b[row]
            })
        };
        let changed: Vec<usize> = dirty
            .iter()
            .copied()
            .filter(|&index| differs(self.tile_rects[index], &self.pixels, previous))
            .collect();
        crate::spike_timing::record("present.diff", started.elapsed());
        crate::spike_timing::value("present.oracle_dirty_tiles", dirty.len() as u64);
        crate::spike_timing::value("present.oracle_changed_tiles", changed.len() as u64);
        if std::env::var_os("PRISMATTYC_SPIKE_DAMAGE_AUDIT").is_some() {
            let missed = (0..self.tile_rects.len())
                .filter(|index| !dirty.contains(index))
                .filter(|&index| differs(self.tile_rects[index], &self.pixels, previous))
                .count();
            crate::spike_timing::value("present.audit_undamaged_changed_tiles", missed as u64);
        }
        for &index in &dirty {
            let tile = self.tile_rects[index];
            for y in tile.y..tile.y + tile.height {
                let row = y * width + tile.x..y * width + tile.x + tile.width;
                previous[row.clone()].copy_from_slice(&self.pixels[row]);
            }
        }
        if std::env::var_os("PRISMATTYC_SPIKE_SKIP_UNCHANGED").is_some() && !self.rebuild_layers {
            changed
        } else {
            dirty
        }
    }

    /// SPIKE: geometry on main, pixels to the present thread.
    fn present_threaded(&mut self, dirty: Vec<usize>) -> Result<()> {
        let scale = self.window.scale_factor();
        if self.rebuild_layers || self.scale != scale {
            let thread = self.thread.as_ref().expect("threaded present");
            thread.wait_idle();
            let started = std::time::Instant::now();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.layer.setFrame(self.root_layer.bounds());
            if self.rebuild_layers {
                for layer in self.tile_layers.drain(..) {
                    layer.removeFromSuperlayer();
                }
                for _ in &self.tile_rects {
                    let layer = CALayer::new();
                    layer.setOpaque(false);
                    layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
                    layer.setGeometryFlipped(true);
                    layer.setContentsGravity(unsafe { kCAGravityTopLeft });
                    self.layer.addSublayer(&layer);
                    self.tile_layers.push(layer);
                }
            }
            for (layer, tile) in self.tile_layers.iter().zip(&self.tile_rects) {
                let view_rect = CGRect::new(
                    CGPoint::new(tile.x as f64 / scale, tile.y as f64 / scale),
                    CGSize::new(tile.width as f64 / scale, tile.height as f64 / scale),
                );
                let root_rect = self.view.convertRectToLayer(view_rect);
                let frame = self
                    .layer
                    .convertRect_fromLayer(root_rect, Some(&self.root_layer));
                layer.setFrame(frame);
                layer.setContentsScale(scale);
            }
            CATransaction::commit();
            thread.set_layers(self.tile_layers.clone());
            crate::spike_timing::record("present.main_geometry", started.elapsed());
        }
        let started = std::time::Instant::now();
        let tiles = dirty
            .into_iter()
            .map(|index| {
                let tile = self.tile_rects[index];
                let mut pixels = Vec::new();
                copy_tile(&self.pixels, self.width, tile, &mut pixels);
                (
                    index,
                    crate::present_thread::TilePixels {
                        width: tile.width,
                        height: tile.height,
                        pixels,
                    },
                )
            })
            .collect();
        crate::spike_timing::record("present.main_copy", started.elapsed());
        self.thread
            .as_ref()
            .expect("threaded present")
            .submit(tiles);
        self.scale = scale;
        self.rebuild_layers = false;
        self.retained = true;
        Ok(())
    }
}

impl Drop for MacPresent {
    fn drop(&mut self) {
        // Join the present thread before the layers leave the tree.
        self.thread = None;
        crate::macos_window::set_window_blur(&self.window, false);
        self.layer.removeFromSuperlayer();
    }
}

pub(crate) fn alpha_image(
    pixels: &[u32],
    width: usize,
    height: usize,
    color_space: &CGColorSpace,
) -> Result<CFRetained<CGImage>> {
    let byte_len = std::mem::size_of_val(pixels);
    // SAFETY: every byte of a u32 is initialized; the slice remains borrowed
    // until CFData has copied it. Supported Macs are little-endian.
    let bytes = unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), byte_len) };
    let data = CFData::from_bytes(bytes);
    let provider =
        CGDataProvider::with_cf_data(Some(&data)).context("create image data provider")?;
    let bitmap = CGBitmapInfo(
        CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
    );
    // SAFETY: the provider owns width * height initialized ARGB pixels. Null
    // decode selects the normal component range; no borrowed data escapes.
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(color_space),
            bitmap,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .context("create alpha-capable Core Graphics image")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_preserves_alpha_and_owns_its_pixels() {
        let color_space = CGColorSpace::new_device_rgb().unwrap();
        let mut pixels = [0x80402010, 0xffabcdef];
        let image = alpha_image(&pixels, 2, 1, &color_space).unwrap();
        pixels.fill(0);
        assert_eq!(
            CGImage::alpha_info(Some(&image)),
            CGImageAlphaInfo::PremultipliedFirst
        );
        let provider = CGImage::data_provider(Some(&image)).unwrap();
        let data = CGDataProvider::data(Some(&provider)).unwrap();
        assert_eq!(
            data.to_vec(),
            &[0x10, 0x20, 0x40, 0x80, 0xef, 0xcd, 0xab, 0xff]
        );
    }
}
