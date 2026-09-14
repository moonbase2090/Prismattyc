//! Run the native presenter lifecycle check in a logged-in macOS desktop.
//! `cargo run -p prismattyc-host --example macos_present_probe --locked`

#[cfg(target_os = "macos")]
#[path = "../src/mac_present.rs"]
mod mac_present;
#[cfg(target_os = "macos")]
#[path = "../src/macos_window.rs"]
mod macos_window;

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;

    use objc2::{msg_send, runtime::AnyObject};
    use objc2_app_kit::NSView;
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
                            .with_inner_size(PhysicalSize::new(640, 480)),
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
            present.present().unwrap();

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
            assert_eq!(layer.contentsScale(), window.scale_factor());
            let contents = unsafe { layer.contents() }.unwrap();
            // SAFETY: MacPresent puts an immutable CGImage in this layer.
            let image: &CGImage = unsafe { &*((&*contents as *const AnyObject).cast()) };
            assert_eq!(
                CGImage::alpha_info(Some(image)),
                CGImageAlphaInfo::PremultipliedFirst
            );
            assert_eq!(CGImage::width(Some(image)), size.width as usize);
            assert_eq!(CGImage::height(Some(image)), size.height as usize);
            // Reusing the CPU buffer must not change the compositor's image.
            present.pixels_mut().fill(0);
            let provider = CGImage::data_provider(Some(image)).unwrap();
            let bytes = CGDataProvider::data(Some(&provider)).unwrap().to_vec();
            assert_eq!(
                &bytes[..8],
                &[0x95, 0xe7, 0x11, 0xff, 0x10, 0x20, 0x40, 0x80]
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
                let _ = window.request_inner_size(PhysicalSize::new(800, 600));
                window.request_redraw();
                println!("PASS native alpha image, immutable pixels, and blur toggles");
                return;
            }
            assert_eq!(size, PhysicalSize::new(800, 600));
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
