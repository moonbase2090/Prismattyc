//! Run the native presenter lifecycle check in a logged-in macOS desktop.
//! `cargo run -p prismattyc-host --example macos_present_probe --locked`

#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/frame_damage.rs"]
mod frame_damage;
#[cfg(target_os = "macos")]
#[path = "../src/mac_present.rs"]
mod mac_present;
#[cfg(target_os = "macos")]
#[path = "../src/macos_window.rs"]
mod macos_window;
#[cfg(target_os = "macos")]
#[path = "../src/pixel_alpha.rs"]
mod pixel_alpha;
#[cfg(target_os = "macos")]
#[path = "../src/present_tiles.rs"]
mod present_tiles;

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
            self.present = Some(mac_present::MacPresent::new(window.clone()).unwrap());
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
            present.prepare(size.width, size.height).unwrap();
            present.pixels_mut().fill(0x80402010);
            present.pixels_mut()[0] = 0xff11e795;
            present.present(FrameDamage::Full).unwrap();

            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected AppKit handle");
            };
            // SAFETY: the retained winit window owns these main-thread objects.
            let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
            let native: *mut AnyObject = unsafe { msg_send![view, window] };
            let opaque: bool = unsafe { msg_send![native, isOpaque] };
            let alpha: f64 = unsafe { msg_send![native, alphaValue] };
            assert!(!opaque);
            assert_eq!(alpha, 1.0, "text must not use whole-window opacity");
            let root = view.layer().unwrap();
            let layers = unsafe { root.sublayers() }.unwrap();
            let layer = layers
                .iter()
                .find(|layer| layer.zPosition() == 1.0)
                .unwrap();
            assert!(!layer.isOpaque());
            assert_eq!(layer.frame(), root.bounds());
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
            present
                .present(FrameDamage::Rects(vec![PixelRect::new(0, 0, 1, 1)]))
                .unwrap();
            for (index, (tile, prior)) in tiles.iter().zip(&prior_contents).enumerate() {
                let contents = unsafe { tile.contents() }.unwrap();
                let same = std::ptr::eq(&*contents, &**prior);
                assert_eq!(same, index != 0, "only the damaged tile replaces its image");
            }
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
            let replacement = mac_present::MacPresent::new(window.clone()).unwrap();
            assert!(macos_window::set_window_blur(window, true));
            assert_eq!(view.subviews().len(), children + 1);
            drop(replacement);
            assert_eq!(view.subviews().len(), children);
            println!("PASS resize, presenter teardown, and recreation");
            event_loop.exit();
        }
    }

    EventLoop::new()
        .unwrap()
        .run_app(&mut Probe::default())
        .unwrap();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Run this probe in a logged-in macOS desktop session.");
    std::process::exit(1);
}
