//! SPIKE (spike/render-thread): can Core Animation present from a non-main
//! thread while the main thread is blocked?
//!
//! `cargo run --release -p prismattyc-host --example render_thread_present_probe --locked -- MODE`
//!
//! MODE `bg`: a render thread fills frames, builds tile CGImages, and commits
//! an explicit CATransaction itself. MODE `main`: the render thread builds
//! the images and sends them to the main thread, which commits (the
//! "bounce back to main" design). In both modes the main thread sleeps for
//! two seconds after the first frames; a capture thread screenshots the
//! window twice during that sleep. Differing captures mean frames reached
//! the screen while main was blocked.

#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/frame_damage.rs"]
mod frame_damage;
#[cfg(target_os = "macos")]
#[path = "../src/pixel_alpha.rs"]
mod pixel_alpha;
#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/present_tiles.rs"]
mod present_tiles;

#[cfg(target_os = "macos")]
mod probe {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo, CGImageByteOrderInfo,
    };
    use objc2_quartz_core::{kCAGravityTopLeft, CALayer, CATransaction};
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalPosition, LogicalSize};
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    use crate::frame_damage::PixelRect;
    use crate::pixel_alpha::premultiply_in_place;
    use crate::present_tiles::{copy_tile, tiles};

    const FRAMES: u64 = 240;
    const BLOCK_AT_FRAME: u64 = 30;
    const BLOCK: Duration = Duration::from_secs(2);

    /// CALayer is documented thread-safe for property changes inside an
    /// explicit transaction; objc2 does not mark it Send, so the probe
    /// asserts that by hand.
    struct SendLayers(Vec<Retained<CALayer>>);
    unsafe impl Send for SendLayers {}
    struct SendImages(Vec<(usize, CFRetained<CGImage>)>);
    unsafe impl Send for SendImages {}

    #[derive(Default)]
    struct Stats {
        images_us: Vec<u64>,
        commit_us: Vec<u64>,
        commits_during_block: u64,
        frames_during_block: u64,
        main_commits_during_block: u64,
    }

    struct App {
        bounce: bool,
        proxy: EventLoopProxy<SendImages>,
        window: Option<Arc<Window>>,
        layers: Vec<Retained<CALayer>>,
        stats: Arc<Mutex<Stats>>,
        blocking: Arc<AtomicBool>,
        frames_done: Arc<AtomicU64>,
        blocked_once: bool,
    }

    pub fn run(bounce: bool) {
        let event_loop = EventLoop::<SendImages>::with_user_event().build().unwrap();
        let mut app = App {
            bounce,
            proxy: event_loop.create_proxy(),
            window: None,
            layers: Vec::new(),
            stats: Arc::default(),
            blocking: Arc::default(),
            frames_done: Arc::default(),
            blocked_once: false,
        };
        event_loop.run_app(&mut app).unwrap();
        report(bounce, &app.stats.lock().unwrap());
    }

    impl ApplicationHandler<SendImages> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("render thread present probe")
                            .with_position(LogicalPosition::new(80.0, 80.0))
                            .with_inner_size(LogicalSize::new(1600.0, 1000.0)),
                    )
                    .unwrap(),
            );
            // Let AppKit apply the size before the tile grid is built.
            std::thread::sleep(Duration::from_millis(300));
            let size = window.inner_size();
            let scale = window.scale_factor();
            let rects = tiles(size.width as usize, size.height as usize);
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected AppKit handle");
            };
            // SAFETY: winit keeps the NSView alive with the window.
            let view: &objc2_app_kit::NSView = unsafe { handle.ns_view.cast().as_ref() };
            view.setWantsLayer(true);
            let root = view.layer().unwrap();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            let container = CALayer::new();
            container.setAnchorPoint(CGPoint::new(0.0, 0.0));
            container.setGeometryFlipped(true);
            container.setFrame(root.bounds());
            container.setZPosition(1.0);
            root.addSublayer(&container);
            for tile in &rects {
                let layer = CALayer::new();
                layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
                layer.setContentsGravity(unsafe { kCAGravityTopLeft });
                layer.setFrame(CGRect::new(
                    CGPoint::new(tile.x as f64 / scale, tile.y as f64 / scale),
                    CGSize::new(tile.width as f64 / scale, tile.height as f64 / scale),
                ));
                layer.setContentsScale(scale);
                container.addSublayer(&layer);
                self.layers.push(layer);
            }
            CATransaction::commit();
            eprintln!(
                "probe: {}x{} px, scale {scale}, {} tiles, mode {}",
                size.width,
                size.height,
                rects.len(),
                if self.bounce { "main (bounce)" } else { "bg" }
            );
            spawn_render_thread(
                size.width as usize,
                size.height as usize,
                rects,
                (!self.bounce).then(|| SendLayers(self.layers.clone())),
                self.proxy.clone(),
                self.stats.clone(),
                self.blocking.clone(),
                self.frames_done.clone(),
            );
            spawn_capture_thread(&window, self.blocking.clone());
            self.window = Some(window);
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, images: SendImages) {
            if images.0.is_empty() {
                event_loop.exit();
                return;
            }
            let started = Instant::now();
            commit(&self.layers, images.0);
            let mut stats = self.stats.lock().unwrap();
            stats.commit_us.push(started.elapsed().as_micros() as u64);
            if self.blocking.load(Ordering::SeqCst) {
                stats.main_commits_during_block += 1;
            }
            drop(stats);
            self.maybe_block();
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.maybe_block();
            // Wake regularly so the block starts mid-run in `bg` mode too.
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(5),
            ));
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            if matches!(event, WindowEvent::CloseRequested) {
                event_loop.exit();
            }
        }
    }

    impl App {
        /// Block the main thread once, as a slow pump or paint would.
        fn maybe_block(&mut self) {
            if self.blocked_once || self.frames_done.load(Ordering::Relaxed) < BLOCK_AT_FRAME {
                return;
            }
            self.blocked_once = true;
            self.blocking.store(true, Ordering::SeqCst);
            std::thread::sleep(BLOCK);
            self.blocking.store(false, Ordering::SeqCst);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_render_thread(
        width: usize,
        height: usize,
        rects: Vec<PixelRect>,
        layers: Option<SendLayers>,
        proxy: EventLoopProxy<SendImages>,
        stats: Arc<Mutex<Stats>>,
        blocking: Arc<AtomicBool>,
        frames_done: Arc<AtomicU64>,
    ) {
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let color_space = CGColorSpace::new_device_rgb().unwrap();
                let mut pixels = vec![0u32; width * height];
                let mut scratch = Vec::new();
                let frame_time = Duration::from_micros(16_667);
                let mut next = Instant::now();
                for frame in 0..FRAMES {
                    fill(&mut pixels, width, frame);
                    let started = Instant::now();
                    let mut images = Vec::with_capacity(rects.len());
                    for (index, tile) in rects.iter().enumerate() {
                        copy_tile(&pixels, width, *tile, &mut scratch);
                        premultiply_in_place(&mut scratch);
                        images.push((index, image(&scratch, tile, &color_space)));
                    }
                    stats
                        .lock()
                        .unwrap()
                        .images_us
                        .push(started.elapsed().as_micros() as u64);
                    if blocking.load(Ordering::SeqCst) {
                        stats.lock().unwrap().frames_during_block += 1;
                    }
                    match &layers {
                        Some(layers) => {
                            let during_block = blocking.load(Ordering::SeqCst);
                            let started = Instant::now();
                            commit(&layers.0, images);
                            let mut stats = stats.lock().unwrap();
                            stats.commit_us.push(started.elapsed().as_micros() as u64);
                            if during_block && blocking.load(Ordering::SeqCst) {
                                stats.commits_during_block += 1;
                            }
                        }
                        None => {
                            let _ = proxy.send_event(SendImages(images));
                        }
                    }
                    frames_done.store(frame + 1, Ordering::Relaxed);
                    next += frame_time;
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
                let _ = proxy.send_event(SendImages(Vec::new()));
            })
            .unwrap();
    }

    /// Two screenshots of the window center while main is blocked.
    fn spawn_capture_thread(window: &Window, blocking: Arc<AtomicBool>) {
        let scale = window.scale_factor();
        let origin = window.inner_position().unwrap().to_logical::<f64>(scale);
        let region = format!(
            "{},{},200,120",
            origin.x as i64 + 700,
            origin.y as i64 + 440
        );
        let dir = std::env::var("PROBE_CAPTURE_DIR").unwrap_or_else(|_| ".".into());
        std::thread::spawn(move || {
            while !blocking.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            for (label, delay) in [("a", 600), ("b", 700)] {
                std::thread::sleep(Duration::from_millis(delay));
                let path = format!("{dir}/capture-{label}.png");
                let ok = std::process::Command::new("/usr/sbin/screencapture")
                    .args(["-x", "-R", &region, &path])
                    .status()
                    .is_ok_and(|status| status.success());
                eprintln!(
                    "probe: capture {label} at block+{}ms main_blocked={} ok={ok}",
                    if label == "a" { 600 } else { 1300 },
                    blocking.load(Ordering::SeqCst)
                );
            }
        });
    }

    fn commit(layers: &[Retained<CALayer>], images: Vec<(usize, CFRetained<CGImage>)>) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (index, image) in images {
            // SAFETY: CALayer retains the immutable CGImage.
            unsafe { layers[index].setContents(Some(image.as_ref())) };
        }
        CATransaction::commit();
    }

    /// A full-frame solid color that changes every frame, plus a moving bar.
    fn fill(pixels: &mut [u32], width: usize, frame: u64) {
        let f = frame as u32;
        let color = 0xff00_0000 | ((f * 37 % 256) << 16) | ((255 - f * 11 % 256) << 8) | 0x40;
        pixels.fill(color);
        let bar = (frame as usize * 16) % width;
        for row in pixels.chunks_mut(width) {
            row[bar..(bar + 32).min(width)].fill(0xffffffff);
        }
    }

    fn image(pixels: &[u32], tile: &PixelRect, space: &CGColorSpace) -> CFRetained<CGImage> {
        // SAFETY: u32 pixels are initialized; CFData copies them.
        let bytes = unsafe {
            std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), std::mem::size_of_val(pixels))
        };
        let data = CFData::from_bytes(bytes);
        let provider = CGDataProvider::with_cf_data(Some(&data)).unwrap();
        let bitmap = CGBitmapInfo(
            CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
        );
        // SAFETY: the provider owns width * height ARGB pixels.
        unsafe {
            CGImage::new(
                tile.width,
                tile.height,
                8,
                32,
                tile.width * 4,
                Some(space),
                bitmap,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
        .unwrap()
    }

    fn report(bounce: bool, stats: &Stats) {
        let pct = |values: &[u64], q: f64| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            sorted
                .get(((sorted.len().max(1) - 1) as f64 * q).round() as usize)
                .copied()
                .unwrap_or(0)
        };
        println!(
            "mode={} frames={} images_us p50={} p95={} max={} commit_us p50={} p95={} max={} bg_commits_during_main_block={} frames_built_during_main_block={} main_commits_during_main_block={}",
            if bounce { "main" } else { "bg" },
            stats.images_us.len(),
            pct(&stats.images_us, 0.5),
            pct(&stats.images_us, 0.95),
            pct(&stats.images_us, 1.0),
            pct(&stats.commit_us, 0.5),
            pct(&stats.commit_us, 0.95),
            pct(&stats.commit_us, 1.0),
            stats.commits_during_block,
            stats.frames_during_block,
            stats.main_commits_during_block,
        );
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let bounce = std::env::args().nth(1).as_deref() == Some("main");
    probe::run(bounce);
}

#[cfg(not(target_os = "macos"))]
fn main() {}
