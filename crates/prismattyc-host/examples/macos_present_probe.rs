//! Run the native presenter lifecycle check in a logged-in macOS desktop.
//! `cargo run -p prismattyc-host --example macos_present_probe --locked`
//! Add `iosurface` to run the three-buffer readback and scale checks.

#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/frame_damage.rs"]
mod frame_damage;
#[cfg(target_os = "macos")]
#[path = "../src/mac_present.rs"]
mod mac_present;
#[cfg(target_os = "macos")]
#[path = "../src/macos_window.rs"]
#[allow(dead_code)]
mod macos_window;
#[cfg(target_os = "macos")]
#[path = "../src/pixel_alpha.rs"]
mod pixel_alpha;
#[cfg(target_os = "macos")]
#[path = "../src/present_surface.rs"]
mod present_surface;
#[cfg(target_os = "macos")]
#[path = "../src/present_tiles.rs"]
mod present_tiles;
#[cfg(target_os = "macos")]
#[path = "../src/present_timing.rs"]
mod present_timing;
#[cfg(target_os = "macos")]
#[path = "../src/surface_damage.rs"]
mod surface_damage;

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;

    use frame_damage::{FrameDamage, PixelRect};
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_app_kit::NSView;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGDataProvider, CGImage, CGImageAlphaInfo};
    use winit::{
        application::ApplicationHandler,
        dpi::PhysicalSize,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, EventLoop},
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::{Window, WindowId},
    };

    #[derive(Default)]
    struct Probe {
        window: Option<Arc<Window>>,
        present: Option<mac_present::MacPresent>,
        resized: bool,
        iosurface: bool,
        iosurface_stage: usize,
    }

    impl ApplicationHandler for Probe {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("Prismattyc alpha presenter probe")
                            .with_transparent(true)
                            .with_inner_size(PhysicalSize::new(1540, 1030)),
                    )
                    .unwrap(),
            );
            self.present =
                Some(mac_present::MacPresent::new(window.clone(), self.iosurface).unwrap());
            window.request_redraw();
            self.window = Some(window);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            if !matches!(event, WindowEvent::RedrawRequested) {
                return;
            }
            let window = self.window.as_ref().unwrap();
            let size = window.inner_size();
            let present = self.present.as_mut().unwrap();
            if self.iosurface {
                match self.iosurface_stage {
                    0 => {
                        let width = size.width as usize;
                        let height = size.height as usize;
                        assert_eq!(present.backend(), "iosurface");
                        assert!(!present.prepare(size.width, size.height).unwrap());
                        let mut expected = vec![0x80402010; width * height];
                        expected[0] = 0xff11e795;
                        let mut expected_premultiplied = vec![0x80201008; width * height];
                        expected_premultiplied[0] = 0xff11e795;
                        present.pixels_mut().copy_from_slice(&expected);
                        let timing = present
                            .present_at_scale(FrameDamage::Full, false, 1.0)
                            .unwrap();
                        assert_eq!(timing.backend, "iosurface");
                        assert_eq!(timing.write_us, 0, "timing is disabled");
                        assert_eq!(timing.commit_us, 0, "timing is disabled");
                        assert_eq!(timing.write_bytes, 0, "timing is disabled");
                        assert_eq!(timing.busy_surface_stalls, 0);
                        let readback = present.readback().unwrap();
                        assert_readback(&readback, &expected_premultiplied);
                        if let Some(path) = std::env::var_os("PRISMATTYC_DUMP_PRESENT") {
                            write_present_png(
                                std::path::Path::new(&path),
                                &expected_premultiplied,
                                size.width,
                                size.height,
                            )
                            .unwrap();
                            write_present_png(
                                &std::path::PathBuf::from(path).with_extension("readback.png"),
                                &readback,
                                size.width,
                                size.height,
                            )
                            .unwrap();
                            println!("PASS IOSurface dump and readback PNGs written for SHA-256 comparison");
                        }
                        let mut slots = vec![present.current_surface_slot().unwrap()];

                        for (rect, color, premultiplied_color) in [
                            (PixelRect::new(1, 1, 7, 3), 0x80123456, 0x80091a2b),
                            (PixelRect::new(13, 9, 2, 4), 0x407f31dd, 0x401f0c37),
                            (PixelRect::new(0, 0, 1, 1), 0xffabcdef, 0xffabcdef),
                            (
                                PixelRect::new(width - 2, height - 2, 2, 2),
                                0x00aabbcc,
                                0x00000000,
                            ),
                        ] {
                            assert!(present.prepare(size.width, size.height).unwrap());
                            for y in rect.y..rect.y + rect.height {
                                expected[y * width + rect.x..y * width + rect.x + rect.width]
                                    .fill(color);
                                expected_premultiplied
                                    [y * width + rect.x..y * width + rect.x + rect.width]
                                    .fill(premultiplied_color);
                            }
                            present.pixels_mut().copy_from_slice(&expected);
                            let timing = present
                                .present_at_scale(FrameDamage::Rects(vec![rect]), false, 1.0)
                                .unwrap();
                            assert_eq!(timing.write_us, 0, "timing is disabled");
                            assert_eq!(timing.commit_us, 0, "timing is disabled");
                            assert_eq!(timing.write_bytes, 0, "timing is disabled");
                            assert_readback(present.readback().unwrap(), &expected_premultiplied);
                            slots.push(present.current_surface_slot().unwrap());
                        }
                        assert!(
                            [0, 1, 2].iter().all(|slot| slots.contains(slot)),
                            "all three IOSurface buffers must be used: {slots:?}"
                        );

                        let RawWindowHandle::AppKit(handle) =
                            window.window_handle().unwrap().as_raw()
                        else {
                            panic!("expected AppKit handle");
                        };
                        // SAFETY: the retained winit window owns this main-thread NSView.
                        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
                        let root = view.layer().unwrap();
                        // SAFETY: the main-thread root layer owns its sublayers.
                        let layers = unsafe { root.sublayers() }.unwrap();
                        let layer = layers
                            .iter()
                            .find(|layer| layer.zPosition() == 1.0)
                            .unwrap();
                        assert_eq!(layer.frame(), root.bounds());
                        assert_eq!(layer.contentsScale(), 1.0);
                        // SAFETY: the presenter has assigned a live IOSurface contents object.
                        assert!(unsafe { layer.contents() }.is_some());
                        // SAFETY: the main-thread presenter layer owns its sublayers, if any.
                        assert!(unsafe { layer.sublayers() }.is_none());

                        let mut generation = present.surface_generation().unwrap();
                        for scale in [2.0, 1.0] {
                            assert!(present.prepare(size.width, size.height).unwrap());
                            present.pixels_mut().copy_from_slice(&expected);
                            let timing = present
                                .present_at_scale(
                                    FrameDamage::Rects(vec![PixelRect::new(0, 0, 1, 1)]),
                                    true,
                                    scale,
                                )
                                .unwrap();
                            generation += 1;
                            assert_eq!(present.surface_generation(), Some(generation));
                            assert_eq!(timing.write_bytes, width * height * 4);
                            assert_readback(present.readback().unwrap(), &expected_premultiplied);
                            assert_eq!(layer.contentsScale(), scale);
                        }

                        self.iosurface_stage = 1;
                        let _ = window.request_inner_size(PhysicalSize::new(900, 600));
                        window.request_redraw();
                        println!(
                            "PASS IOSurface partial frames, three buffers, readback, timing-off, and 1x/2x/1x reallocation"
                        );
                        return;
                    }
                    1 | 2 => {
                        let target = if self.iosurface_stage == 1 {
                            PhysicalSize::new(900, 600)
                        } else {
                            PhysicalSize::new(1800, 1200)
                        };
                        if size != target {
                            window.request_redraw();
                            return;
                        }
                        let width = size.width as usize;
                        let height = size.height as usize;
                        let prior_generation = present.surface_generation().unwrap();
                        assert!(!present.prepare(size.width, size.height).unwrap());
                        let expected = vec![0xff23579b; width * height];
                        present.pixels_mut().copy_from_slice(&expected);
                        let timing = present
                            .present_at_scale(FrameDamage::Full, true, 1.0)
                            .unwrap();
                        assert_eq!(present.surface_generation(), Some(prior_generation + 1));
                        assert_eq!(timing.write_bytes, width * height * 4);
                        assert_readback(present.readback().unwrap(), &expected);
                        if self.iosurface_stage == 1 {
                            self.iosurface_stage = 2;
                            let _ = window.request_inner_size(PhysicalSize::new(1800, 1200));
                            window.request_redraw();
                            println!("PASS IOSurface shrink reallocated and copied the full frame");
                            return;
                        }

                        let RawWindowHandle::AppKit(handle) =
                            window.window_handle().unwrap().as_raw()
                        else {
                            panic!("expected AppKit handle");
                        };
                        // SAFETY: the retained winit window owns this main-thread NSView.
                        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
                        let root = view.layer().unwrap();
                        // SAFETY: the main-thread root layer owns its sublayers.
                        let layers = unsafe { root.sublayers() }.unwrap();
                        let layer = layers
                            .iter()
                            .find(|layer| layer.zPosition() == 1.0)
                            .unwrap();
                        drop(self.present.take());
                        assert!(
                            layer.superlayer().is_none(),
                            "drop removes the content layer"
                        );
                        let replacement =
                            mac_present::MacPresent::new(window.clone(), true).unwrap();
                        self.present = Some(replacement);
                        let replacement = self.present.as_mut().unwrap();
                        assert!(!replacement.prepare(size.width, size.height).unwrap());
                        replacement.pixels_mut().copy_from_slice(&expected);
                        replacement
                            .present_at_scale(FrameDamage::Full, false, 1.0)
                            .unwrap();
                        assert_readback(replacement.readback().unwrap(), &expected);
                        println!("PASS IOSurface grow, teardown, and presenter recreation");
                        self.iosurface_stage = 3;
                        event_loop.exit();
                        return;
                    }
                    3 => return,
                    _ => unreachable!(),
                }
            }
            present.prepare(size.width, size.height).unwrap();
            present.pixels_mut().fill(0x80402010);
            present.pixels_mut()[0] = 0xff11e795;
            let full_present = present.present(FrameDamage::Full, false).unwrap();
            assert_eq!(
                full_present.dirty_tiles,
                present_tiles::tiles(size.width as usize, size.height as usize).len()
            );
            assert_eq!(
                full_present.write_bytes,
                size.width as usize * size.height as usize * std::mem::size_of::<u32>()
            );
            assert_eq!(full_present.changed_tiles, None);
            let mut expected_readback =
                vec![0x80201008; size.width as usize * size.height as usize];
            expected_readback[0] = 0xff11e795;
            let readback = present.readback().unwrap();
            assert_pixels_equal(
                &readback,
                &expected_readback,
                "tile readback must match the premultiplied framebuffer",
            );
            if let Some(path) = std::env::var_os("PRISMATTYC_DUMP_PRESENT") {
                let path = std::path::PathBuf::from(path);
                write_present_png(&path, &expected_readback, size.width, size.height).unwrap();
                write_present_png(
                    &path.with_extension("readback.png"),
                    &readback,
                    size.width,
                    size.height,
                )
                .unwrap();
                println!("PASS tile dump and readback PNGs written for SHA-256 comparison");
            }

            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected AppKit handle");
            };
            // SAFETY: the retained winit window owns these main-thread objects.
            let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
            // SAFETY: the view is live on the main thread and responds to `window`.
            let native: *mut AnyObject = unsafe { msg_send![view, window] };
            // SAFETY: the native window is the live NSWindow returned above.
            let opaque: bool = unsafe { msg_send![native, isOpaque] };
            // SAFETY: the native window is the live NSWindow returned above.
            let alpha: f64 = unsafe { msg_send![native, alphaValue] };
            assert!(!opaque);
            assert_eq!(alpha, 1.0, "text must not use whole-window opacity");
            let root = view.layer().unwrap();
            // SAFETY: the main-thread root layer owns its sublayers.
            let layers = unsafe { root.sublayers() }.unwrap();
            let layer = layers
                .iter()
                .find(|layer| layer.zPosition() == 1.0)
                .unwrap();
            assert!(!layer.isOpaque());
            assert_eq!(layer.frame(), root.bounds());
            // SAFETY: the presenter layer owns its tile sublayers.
            let tiles = unsafe { layer.sublayers() }.unwrap();
            let rects = present_tiles::tiles(size.width as usize, size.height as usize);
            assert_eq!(tiles.len(), rects.len());
            let scale = window.scale_factor();
            let mut prior_contents = Vec::new();
            for (tile, rect) in tiles.iter().zip(&rects) {
                let expected = CGRect::new(
                    CGPoint::new(rect.x as f64 / scale, rect.y as f64 / scale),
                    CGSize::new(rect.width as f64 / scale, rect.height as f64 / scale),
                );
                // Optional negative control recreates #8's direct assignment.
                if std::env::var_os("PRISMATTYC_PROBE_OLD_TILE_FRAMES").is_some() {
                    tile.setFrame(expected);
                }
                let root_rect = tile.convertRect_toLayer(tile.bounds(), Some(&root));
                let actual = view.convertRectFromLayer(root_rect);
                for (got, want) in [
                    (actual.origin.x, expected.origin.x),
                    (actual.origin.y, expected.origin.y),
                    (actual.size.width, expected.size.width),
                    (actual.size.height, expected.size.height),
                ] {
                    assert!(
                        (got - want).abs() < 0.01,
                        "tile {rect:?}: view rect {actual:?}, expected {expected:?}"
                    );
                }
                assert_eq!(tile.contentsScale(), scale);
                assert!(!tile.isOpaque());
                // SAFETY: the presenter has assigned a live CGImage contents object.
                let contents = unsafe { tile.contents() }.unwrap();
                // SAFETY: the presenter assigns a CGImage to each tile.
                let image: &CGImage = unsafe { &*((&*contents as *const AnyObject).cast()) };
                assert_eq!(
                    CGImage::alpha_info(Some(image)),
                    CGImageAlphaInfo::PremultipliedFirst
                );
                assert_eq!(CGImage::width(Some(image)), rect.width);
                assert_eq!(CGImage::height(Some(image)), rect.height);
                prior_contents.push(contents);
            }
            // Only the first tile changes. The old image must remain immutable
            // and every untouched tile must retain the same image object.
            assert!(present.prepare(size.width, size.height).unwrap());
            present.pixels_mut()[0] = 0xffabcdef;
            let partial_present = present
                .present(FrameDamage::Rects(vec![PixelRect::new(0, 0, 1, 1)]), false)
                .unwrap();
            assert_eq!(partial_present.dirty_tiles, 1);
            assert_eq!(partial_present.changed_tiles, None);
            expected_readback[0] = 0xffabcdef;
            let readback = present.readback().unwrap();
            assert_pixels_equal(
                &readback,
                &expected_readback,
                "partial tile present must retain the other premultiplied pixels",
            );
            for (index, (tile, prior)) in tiles.iter().zip(&prior_contents).enumerate() {
                // SAFETY: each tile retains the CGImage assigned by the presenter.
                let contents = unsafe { tile.contents() }.unwrap();
                let same = std::ptr::eq(&*contents, &**prior);
                assert_eq!(same, index != 0, "only the damaged tile replaces its image");
            }
            // SAFETY: the retained contents originated from the presenter's CGImage.
            let prior: &CGImage = unsafe { &*((&*prior_contents[0] as *const AnyObject).cast()) };
            let provider = CGImage::data_provider(Some(prior)).unwrap();
            let bytes = CGDataProvider::data(Some(&provider)).unwrap().to_vec();
            assert_eq!(
                &bytes[..8],
                &[0x95, 0xe7, 0x11, 0xff, 0x08, 0x10, 0x20, 0x80]
            );

            let children = view.subviews().len();
            for _ in 0..3 {
                assert!(macos_window::set_window_blur(window, true));
                assert_eq!(view.subviews().len(), children + 1);
                assert!(macos_window::set_window_blur(window, true));
                assert_eq!(view.subviews().len(), children + 1, "no duplicate backdrop");
                assert!(!macos_window::set_window_blur(window, false));
                assert_eq!(view.subviews().len(), children);
            }
            if !self.resized {
                self.resized = true;
                let _ = window.request_inner_size(PhysicalSize::new(1800, 1200));
                window.request_redraw();
                println!("PASS native tile placement, partial images, immutable pixels, and blur toggles");
                return;
            }
            assert_eq!(size, PhysicalSize::new(1800, 1200));
            assert!(macos_window::set_window_blur(window, true));
            drop(self.present.take());
            assert_eq!(view.subviews().len(), children, "drop removes the backdrop");
            assert!(
                layer.superlayer().is_none(),
                "drop removes the content layer"
            );
            // Recreate on the same view to check backdrop bookkeeping cleanup.
            let replacement = mac_present::MacPresent::new(window.clone(), false).unwrap();
            assert!(macos_window::set_window_blur(window, true));
            assert_eq!(view.subviews().len(), children + 1);
            drop(replacement);
            assert_eq!(view.subviews().len(), children);
            println!("PASS resize, presenter teardown, and recreation");
            event_loop.exit();
        }
    }

    let iosurface = std::env::args().any(|arg| arg == "iosurface");
    EventLoop::new()
        .unwrap()
        .run_app(&mut Probe {
            iosurface,
            ..Probe::default()
        })
        .unwrap();
}

#[cfg(target_os = "macos")]
fn assert_readback(readback: impl AsRef<[u32]>, expected_premultiplied: &[u32]) {
    assert_pixels_equal(
        readback.as_ref(),
        expected_premultiplied,
        "present readback must match the premultiplied framebuffer",
    );
}

#[cfg(target_os = "macos")]
fn assert_pixels_equal(actual: &[u32], expected: &[u32], message: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{message}: pixel count differs"
    );
    if let Some((index, (actual, expected))) = actual
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (actual, expected))| actual != expected)
    {
        panic!("{message}: pixel {index} was {actual:#010x}, expected {expected:#010x}");
    }
}

#[cfg(target_os = "macos")]
fn write_present_png(
    path: &std::path::Path,
    pixels: &[u32],
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&[
            (pixel >> 16) as u8,
            (pixel >> 8) as u8,
            *pixel as u8,
            (pixel >> 24) as u8,
        ]);
    }
    writer.write_image_data(&rgba)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Run this probe in a logged-in macOS desktop session.");
    std::process::exit(1);
}
