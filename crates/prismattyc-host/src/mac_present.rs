//! Alpha-capable presentation above the AppKit blur backdrop.
//!
//! The rasterizer supplies straight ARGB. Each full frame is premultiplied
//! before Core Graphics copies it into an immutable image. The compositor
//! can keep that image after the next frame reuses the CPU buffer.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use objc2::{rc::Retained, MainThreadMarker};
use objc2_app_kit::NSView;
use objc2_core_foundation::{CFData, CFRetained, CGPoint};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_quartz_core::{kCAGravityTopLeft, CALayer, CATransaction};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub struct MacPresent {
    layer: Retained<CALayer>,
    root_layer: Retained<CALayer>,
    color_space: CFRetained<CGColorSpace>,
    pixels: Vec<u32>,
    width: usize,
    height: usize,
    // Retain the window until the layer and blur view have been removed.
    window: Arc<Window>,
    // AppKit operations, including Drop, stay on the main thread.
    _main_thread: MainThreadMarker,
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
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
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
        CATransaction::commit();
        Ok(Self {
            layer,
            root_layer,
            color_space,
            pixels: Vec::new(),
            width: 0,
            height: 0,
            window,
            _main_thread: main_thread,
        })
    }

    pub fn prepare(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width as usize;
        let height = height as usize;
        let len = width
            .checked_mul(height)
            .context("Mac framebuffer size overflow")?;
        if len == 0 {
            bail!("Mac framebuffer must be nonempty");
        }
        self.pixels.resize(len, 0);
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    pub fn present(&mut self) -> Result<()> {
        let image = alpha_image(&self.pixels, self.width, self.height, &self.color_space)?;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // Read current point bounds and scale for every frame. This includes
        // resize and monitor changes without rounding fractional AppKit bounds.
        self.layer.setFrame(self.root_layer.bounds());
        self.layer.setContentsScale(self.window.scale_factor());
        // SAFETY: CALayer accepts a CGImage and retains its immutable data.
        unsafe { self.layer.setContents(Some(image.as_ref())) };
        CATransaction::commit();
        Ok(())
    }
}

impl Drop for MacPresent {
    fn drop(&mut self) {
        crate::macos_window::set_window_blur(&self.window, false);
        self.layer.removeFromSuperlayer();
    }
}

fn alpha_image(
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
