//! SPIKE (spike/present-cost): per-frame cost of macOS present paths.
//!
//! `present_cost_probe BACKEND WIDTHxHEIGHT SCALE DAMAGE THREAD [FRAMES]`
//!
//! - BACKEND: `tiles` (today: CGImage per 512x128 tile), `ring` (three
//!   IOSurfaces rotated as layer contents), `inplace` (one IOSurface written
//!   in place, then `setContentsChanged`), `metal` (CAMetalLayer: upload
//!   damaged rects to a shared texture, GPU blit to the drawable, present).
//! - WIDTHxHEIGHT: framebuffer pixels. SCALE: layer contents scale (1 or 2).
//!   Layers larger than the window are allowed: upload and commit costs are
//!   real, but the compositor only draws the visible part.
//! - DAMAGE: `full`, `band` (full width, 82 px: two text rows), `cell` (18x41).
//! - THREAD: `main` (frames from the event loop) or `bg` (a render thread).
//!
//! Prints one `key=value` line: per-frame CPU prep and commit times, process
//! CPU per frame, WindowServer CPU per second, GPU time (metal), footprint,
//! and a readback check that the presented pixels equal the premultiplied
//! framebuffer.

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
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{msg_send, sel};
    use objc2_core_foundation::{
        CFData, CFDictionary, CFNumber, CFRetained, CFString, CGPoint, CGRect, CGSize,
    };
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo, CGImageByteOrderInfo,
    };
    use objc2_io_surface::{
        kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat,
        kIOSurfaceWidth, IOSurfaceLockOptions, IOSurfaceRef,
    };
    use objc2_metal::{
        MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
        MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLOrigin, MTLPixelFormat, MTLRegion,
        MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    };
    use objc2_quartz_core::{
        kCAGravityTopLeft, CALayer, CAMetalDrawable, CAMetalLayer, CATransaction,
    };
    use winit::application::ApplicationHandler;
    use winit::dpi::{LogicalPosition, LogicalSize};
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    use crate::frame_damage::PixelRect;
    use crate::pixel_alpha::premultiply_in_place;
    use crate::present_tiles::{copy_tile, tiles};

    const WARMUP: usize = 20;
    const FRAME: Duration = Duration::from_micros(16_667);

    #[derive(Clone, Copy, PartialEq)]
    enum Backend {
        Tiles,
        Ring,
        InPlace,
        Metal,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Damage {
        Full,
        Band,
        Cell,
    }

    #[derive(Clone, Copy)]
    struct Case {
        backend: Backend,
        width: usize,
        height: usize,
        scale: f64,
        damage: Damage,
        background: bool,
        frames: usize,
    }

    fn parse() -> Case {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let usage = "usage: BACKEND WxH SCALE DAMAGE THREAD [FRAMES]";
        let backend = match args.first().map(String::as_str) {
            Some("tiles") => Backend::Tiles,
            Some("ring") => Backend::Ring,
            Some("inplace") => Backend::InPlace,
            Some("metal") => Backend::Metal,
            _ => panic!("{usage}"),
        };
        let (w, h) = args[1].split_once('x').expect(usage);
        let damage = match args[3].as_str() {
            "full" => Damage::Full,
            "band" => Damage::Band,
            "cell" => Damage::Cell,
            _ => panic!("{usage}"),
        };
        Case {
            backend,
            width: w.parse().unwrap(),
            height: h.parse().unwrap(),
            scale: args[2].parse().unwrap(),
            damage,
            background: args[4] == "bg",
            frames: args.get(5).map_or(200, |n| n.parse().unwrap()),
        }
    }

    /// Per-frame costs of one presenter call, in microseconds.
    #[derive(Default, Clone, Copy)]
    struct Cost {
        prep_us: u64,
        commit_us: u64,
        bytes: u64,
    }

    trait Presenter {
        fn present(&mut self, pixels: &[u32], damage: &[PixelRect]) -> Cost;
        /// Compare presented pixels with the premultiplied framebuffer.
        fn verify(&mut self, expected: &[u32]) -> Result<(), String>;
        /// Extra key=value pairs (GPU time, stalls).
        fn extra(&mut self) -> String {
            String::new()
        }
    }

    // ---- today's path: one CGImage per damaged 512x128 tile ---------------

    struct Tiles {
        width: usize,
        rects: Vec<PixelRect>,
        layers: Vec<Retained<CALayer>>,
        space: CFRetained<CGColorSpace>,
        scratch: Vec<u32>,
    }

    impl Tiles {
        fn new(container: &CALayer, case: &Case) -> Self {
            let rects = tiles(case.width, case.height);
            let layers = rects
                .iter()
                .map(|tile| {
                    let layer = sublayer(container, *tile, case.scale);
                    layer.setOpaque(false);
                    layer
                })
                .collect();
            Self {
                width: case.width,
                rects,
                layers,
                space: CGColorSpace::new_device_rgb().unwrap(),
                scratch: Vec::new(),
            }
        }
    }

    impl Presenter for Tiles {
        fn present(&mut self, pixels: &[u32], damage: &[PixelRect]) -> Cost {
            let started = Instant::now();
            let mut images = Vec::new();
            let mut bytes = 0;
            for (index, tile) in self.rects.iter().enumerate() {
                if !damage.iter().any(|rect| intersects(*rect, *tile)) {
                    continue;
                }
                copy_tile(pixels, self.width, *tile, &mut self.scratch);
                premultiply_in_place(&mut self.scratch);
                bytes += self.scratch.len() as u64 * 4;
                images.push((index, cg_image(&self.scratch, tile, &self.space)));
            }
            let prep_us = micros(started);
            let started = Instant::now();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            for (index, image) in images {
                // SAFETY: CALayer retains the immutable CGImage.
                unsafe { self.layers[index].setContents(Some(image.as_ref())) };
            }
            CATransaction::commit();
            flush();
            Cost {
                prep_us,
                commit_us: micros(started),
                bytes,
            }
        }

        fn verify(&mut self, expected: &[u32]) -> Result<(), String> {
            for (layer, tile) in self.layers.iter().zip(&self.rects) {
                let contents = unsafe { layer.contents() }.ok_or("tile without contents")?;
                // SAFETY: this presenter only assigns CGImages.
                let image: &CGImage = unsafe { &*((&*contents as *const AnyObject).cast()) };
                let provider = CGImage::data_provider(Some(image)).ok_or("no provider")?;
                let data = CGDataProvider::data(Some(&provider)).ok_or("no data")?;
                let got = data.to_vec();
                let mut want = Vec::new();
                copy_tile(expected, self.width, *tile, &mut want);
                if got != as_bytes(&want) {
                    return Err(format!("tile {tile:?} differs"));
                }
            }
            Ok(())
        }
    }

    // ---- IOSurface contents ---------------------------------------------

    struct Surfaces {
        layer: Retained<CALayer>,
        surfaces: Vec<CFRetained<IOSurfaceRef>>,
        /// Rects each surface still lacks (applied to the others since).
        stale: Vec<Vec<PixelRect>>,
        current: usize,
        in_place: bool,
        width: usize,
        height: usize,
        all_busy: u64,
    }

    impl Surfaces {
        fn new(container: &CALayer, case: &Case, count: usize) -> Self {
            let full = PixelRect::new(0, 0, case.width, case.height);
            let layer = sublayer(container, full, case.scale);
            layer.setOpaque(false);
            Self {
                layer,
                surfaces: (0..count)
                    .map(|_| surface(case.width, case.height))
                    .collect(),
                stale: vec![vec![full]; count],
                current: count - 1,
                in_place: count == 1,
                width: case.width,
                height: case.height,
                all_busy: 0,
            }
        }

        /// The next surface the window server is not reading, else the oldest.
        fn pick(&mut self) -> usize {
            let count = self.surfaces.len();
            for step in 1..=count {
                let index = (self.current + step) % count;
                if self.in_place || !self.surfaces[index].is_in_use() {
                    return index;
                }
            }
            self.all_busy += 1;
            (self.current + 1) % count
        }
    }

    impl Presenter for Surfaces {
        fn present(&mut self, pixels: &[u32], damage: &[PixelRect]) -> Cost {
            let started = Instant::now();
            let index = self.pick();
            let mut rects = std::mem::take(&mut self.stale[index]);
            rects.extend_from_slice(damage);
            rects.dedup();
            let full = PixelRect::new(0, 0, self.width, self.height);
            if rects.contains(&full) {
                rects = vec![full];
            }
            let surface = &self.surfaces[index];
            let bytes = write_surface(surface, pixels, self.width, &rects);
            for (other, stale) in self.stale.iter_mut().enumerate() {
                if other != index {
                    stale.extend_from_slice(damage);
                    if stale.len() > 64 {
                        stale.clear();
                        stale.push(PixelRect::new(0, 0, self.width, self.height));
                    }
                }
            }
            let prep_us = micros(started);
            let started = Instant::now();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            if self.in_place && self.current == index {
                // Same object: tell Core Animation the bytes changed.
                // Undocumented selector; guarded by respondsToSelector.
                let layer: &CALayer = &self.layer;
                let responds: bool =
                    unsafe { msg_send![layer, respondsToSelector: sel!(setContentsChanged)] };
                assert!(responds, "CALayer lacks setContentsChanged");
                let _: () = unsafe { msg_send![layer, setContentsChanged] };
            } else {
                // SAFETY: IOSurfaceRef is toll-free bridged to IOSurface.
                let object: &AnyObject =
                    unsafe { &*(CFRetained::as_ptr(surface).as_ptr() as *const AnyObject) };
                unsafe { self.layer.setContents(Some(object)) };
            }
            CATransaction::commit();
            flush();
            self.current = index;
            Cost {
                prep_us,
                commit_us: micros(started),
                bytes,
            }
        }

        fn verify(&mut self, expected: &[u32]) -> Result<(), String> {
            let surface = &self.surfaces[self.current];
            lock(surface, true);
            let stride = surface.bytes_per_row() / 4;
            let base = surface.base_address().as_ptr() as *const u32;
            let mut result = Ok(());
            for y in 0..self.height {
                // SAFETY: the surface is locked and has `height` rows of `stride`.
                let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), self.width) };
                if row != &expected[y * self.width..(y + 1) * self.width] {
                    result = Err(format!("surface row {y} differs"));
                    break;
                }
            }
            unlock(surface, true);
            result
        }

        fn extra(&mut self) -> String {
            format!(" all_surfaces_busy={}", self.all_busy)
        }
    }

    fn surface(width: usize, height: usize) -> CFRetained<IOSurfaceRef> {
        let row = (width * 4).next_multiple_of(64);
        let keys: [&CFString; 5] = unsafe {
            [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfaceBytesPerElement,
                kIOSurfaceBytesPerRow,
                kIOSurfacePixelFormat,
            ]
        };
        let values = [
            CFNumber::new_isize(width as isize),
            CFNumber::new_isize(height as isize),
            CFNumber::new_isize(4),
            CFNumber::new_isize(row as isize),
            CFNumber::new_i32(i32::from_be_bytes(*b"BGRA")),
        ];
        let values: Vec<&CFNumber> = values.iter().map(|v| &**v).collect();
        let properties = CFDictionary::from_slices(&keys, &values);
        // SAFETY: valid IOSurface property dictionary.
        unsafe { IOSurfaceRef::new(properties.as_opaque()) }.expect("IOSurfaceCreate")
    }

    fn lock(surface: &IOSurfaceRef, read_only: bool) {
        let options = if read_only {
            IOSurfaceLockOptions::ReadOnly
        } else {
            IOSurfaceLockOptions::empty()
        };
        // SAFETY: a null seed pointer is allowed.
        let status = unsafe { surface.lock(options, std::ptr::null_mut()) };
        assert_eq!(status, 0, "IOSurfaceLock");
    }

    fn unlock(surface: &IOSurfaceRef, read_only: bool) {
        let options = if read_only {
            IOSurfaceLockOptions::ReadOnly
        } else {
            IOSurfaceLockOptions::empty()
        };
        // SAFETY: paired with `lock`.
        unsafe { surface.unlock(options, std::ptr::null_mut()) };
    }

    /// Copy and premultiply `rects` of the straight framebuffer into the
    /// surface. Returns bytes written.
    fn write_surface(
        surface: &IOSurfaceRef,
        pixels: &[u32],
        width: usize,
        rects: &[PixelRect],
    ) -> u64 {
        lock(surface, false);
        let stride = surface.bytes_per_row() / 4;
        let base: NonNull<c_void> = surface.base_address();
        let base = base.as_ptr() as *mut u32;
        let mut bytes = 0;
        for rect in rects {
            for y in rect.y..rect.y + rect.height {
                // SAFETY: locked surface, rect inside the framebuffer bounds.
                let dst = unsafe {
                    std::slice::from_raw_parts_mut(base.add(y * stride + rect.x), rect.width)
                };
                dst.copy_from_slice(&pixels[y * width + rect.x..y * width + rect.x + rect.width]);
                premultiply_in_place(dst);
            }
            bytes += (rect.width * rect.height * 4) as u64;
        }
        unlock(surface, false);
        bytes
    }

    // ---- CAMetalLayer ----------------------------------------------------

    struct Metal {
        layer: Retained<CAMetalLayer>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        canvas: Retained<ProtocolObject<dyn MTLTexture>>,
        width: usize,
        scratch: Vec<u32>,
        buffers: Vec<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
        last_drawable: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
        drawable_wait_us: Vec<u64>,
    }

    impl Metal {
        fn new(container: &CALayer, case: &Case) -> Self {
            let device = MTLCreateSystemDefaultDevice().expect("Metal device");
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            layer.setFramebufferOnly(false);
            layer.setOpaque(false);
            layer.setDrawableSize(CGSize::new(case.width as f64, case.height as f64));
            place(
                &layer,
                PixelRect::new(0, 0, case.width, case.height),
                case.scale,
            );
            container.addSublayer(&layer);
            let descriptor = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    case.width,
                    case.height,
                    false,
                )
            };
            descriptor.setStorageMode(MTLStorageMode::Shared);
            descriptor.setUsage(MTLTextureUsage::ShaderRead);
            let canvas = device
                .newTextureWithDescriptor(&descriptor)
                .expect("texture");
            Self {
                layer,
                queue: device.newCommandQueue().expect("queue"),
                canvas,
                width: case.width,
                scratch: Vec::new(),
                buffers: Vec::new(),
                last_drawable: None,
                drawable_wait_us: Vec::new(),
            }
        }
    }

    impl Presenter for Metal {
        fn present(&mut self, pixels: &[u32], damage: &[PixelRect]) -> Cost {
            let started = Instant::now();
            let mut bytes = 0;
            for rect in damage {
                copy_tile(pixels, self.width, *rect, &mut self.scratch);
                premultiply_in_place(&mut self.scratch);
                let region = MTLRegion {
                    origin: MTLOrigin {
                        x: rect.x,
                        y: rect.y,
                        z: 0,
                    },
                    size: MTLSize {
                        width: rect.width,
                        height: rect.height,
                        depth: 1,
                    },
                };
                // SAFETY: scratch holds rect.width * rect.height pixels.
                unsafe {
                    self.canvas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                        region,
                        0,
                        NonNull::new(self.scratch.as_mut_ptr().cast()).unwrap(),
                        rect.width * 4,
                    )
                };
                bytes += (rect.width * rect.height * 4) as u64;
            }
            let prep_us = micros(started);
            let started = Instant::now();
            let Some(drawable) = self.layer.nextDrawable() else {
                return Cost {
                    prep_us,
                    commit_us: micros(started),
                    bytes,
                };
            };
            self.drawable_wait_us.push(micros(started));
            let buffer = self.queue.commandBuffer().expect("command buffer");
            let blit = buffer.blitCommandEncoder().expect("blit");
            let target = drawable.texture();
            // Drawables are not retained between frames: copy the whole canvas.
            unsafe { blit.copyFromTexture_toTexture(&self.canvas, &target) };
            blit.endEncoding();
            let as_drawable: &ProtocolObject<dyn MTLDrawable> =
                ProtocolObject::from_ref(&*drawable);
            buffer.presentDrawable(as_drawable);
            buffer.commit();
            self.last_drawable = Some(target);
            self.buffers.push(buffer);
            Cost {
                prep_us,
                commit_us: micros(started),
                bytes,
            }
        }

        fn verify(&mut self, expected: &[u32]) -> Result<(), String> {
            if let Some(buffer) = self.buffers.last() {
                buffer.waitUntilCompleted();
            }
            for (name, texture) in [
                ("canvas", Some(&self.canvas)),
                ("drawable", self.last_drawable.as_ref()),
            ] {
                let texture = texture.ok_or("no drawable")?;
                let height = expected.len() / self.width;
                let mut got = vec![0u32; expected.len()];
                let region = MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width: self.width,
                        height,
                        depth: 1,
                    },
                };
                unsafe {
                    texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                        NonNull::new(got.as_mut_ptr().cast()).unwrap(),
                        self.width * 4,
                        region,
                        0,
                    )
                };
                if got != expected {
                    return Err(format!("{name} texture differs"));
                }
            }
            Ok(())
        }

        fn extra(&mut self) -> String {
            let mut gpu: Vec<u64> = self
                .buffers
                .iter()
                .skip(WARMUP)
                .map(|buffer| {
                    buffer.waitUntilCompleted();
                    ((buffer.GPUEndTime() - buffer.GPUStartTime()) * 1e6) as u64
                })
                .collect();
            let wait = std::mem::take(&mut self.drawable_wait_us);
            format!(
                " gpu_us_p50={} gpu_us_p95={} drawable_wait_us_p50={} drawable_wait_us_p95={}",
                pct(&mut gpu, 0.5),
                pct(&mut gpu, 0.95),
                pct(&mut wait.clone(), 0.5),
                pct(&mut wait.clone(), 0.95)
            )
        }
    }

    // ---- shared helpers ---------------------------------------------------

    fn sublayer(container: &CALayer, rect: PixelRect, scale: f64) -> Retained<CALayer> {
        let layer = CALayer::new();
        place(&layer, rect, scale);
        container.addSublayer(&layer);
        layer
    }

    fn place(layer: &CALayer, rect: PixelRect, scale: f64) {
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        layer.setFrame(CGRect::new(
            CGPoint::new(rect.x as f64 / scale, rect.y as f64 / scale),
            CGSize::new(rect.width as f64 / scale, rect.height as f64 / scale),
        ));
        layer.setContentsScale(scale);
    }

    fn intersects(a: PixelRect, b: PixelRect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn as_bytes(pixels: &[u32]) -> &[u8] {
        // SAFETY: u32 has no padding; little-endian BGRA in memory.
        unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast(), std::mem::size_of_val(pixels)) }
    }

    fn cg_image(pixels: &[u32], tile: &PixelRect, space: &CGColorSpace) -> CFRetained<CGImage> {
        let data = CFData::from_bytes(as_bytes(pixels));
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

    /// On main, AppKit may already hold an implicit transaction, so an explicit
    /// commit nests inside it and reaches the render server only when the run
    /// loop turns. `PROBE_FLUSH=0` skips this flush to show the difference.
    fn flush() {
        if std::env::var("PROBE_FLUSH").as_deref() != Ok("0") {
            CATransaction::flush();
        }
    }

    fn micros(started: Instant) -> u64 {
        started.elapsed().as_micros() as u64
    }

    fn pct(values: &mut [u64], q: f64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        values[((values.len() - 1) as f64 * q).round() as usize]
    }

    /// Straight-ARGB framebuffer with a translucent strip so the premultiply
    /// path is exercised by verification.
    struct Scene {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }

    impl Scene {
        fn new(width: usize, height: usize) -> Self {
            let mut pixels = vec![0xff1d2330u32; width * height];
            for row in pixels.chunks_mut(width) {
                row[..64.min(width)].fill(0x80406080);
            }
            Self {
                width,
                height,
                pixels,
            }
        }

        /// Change the damaged area for frame `n` and return it.
        fn step(&mut self, damage: Damage, n: usize) -> PixelRect {
            let rect = match damage {
                Damage::Full => PixelRect::new(0, 0, self.width, self.height),
                Damage::Band => {
                    let y = (n * 41) % (self.height - 82);
                    PixelRect::new(0, y, self.width, 82)
                }
                Damage::Cell => {
                    let x = (n * 18) % (self.width - 18);
                    let y = (n * 41) % (self.height - 41);
                    PixelRect::new(x, y, 18, 41)
                }
            };
            let color =
                0xff000000 | ((n as u32 * 37) % 256) << 16 | ((n as u32 * 11) % 256) << 8 | 0x60;
            for y in rect.y..rect.y + rect.height {
                let row =
                    &mut self.pixels[y * self.width + rect.x..y * self.width + rect.x + rect.width];
                row.fill(color);
                // A thin moving bar keeps neighbouring frames distinct.
                let len = row.len();
                let bar = (n * 7) % len;
                row[bar..(bar + 4).min(len)].fill(0xffffffff);
            }
            rect
        }

        fn premultiplied(&self) -> Vec<u32> {
            let mut out = self.pixels.clone();
            premultiply_in_place(&mut out);
            out
        }
    }

    struct SendBox<T>(T);
    // SAFETY: the presenter's Core Animation and Metal objects are used by
    // exactly one thread at a time; CALayer changes happen inside explicit
    // transactions (see docs/design/render-thread.md).
    unsafe impl<T> Send for SendBox<T> {}

    struct Run {
        case: Case,
        costs: Vec<Cost>,
        cpu_us: u64,
        wall: Duration,
        window_server_ms: f64,
        footprint_mb: f64,
        verify: Result<(), String>,
        extra: String,
    }

    fn run_frames(case: Case, presenter: &mut dyn Presenter) -> Run {
        let mut scene = Scene::new(case.width, case.height);
        let full = PixelRect::new(0, 0, case.width, case.height);
        presenter.present(&scene.pixels, &[full]);
        let mut costs = Vec::with_capacity(case.frames);
        let mut next = Instant::now();
        let mut measured_from = (Instant::now(), cpu_time_us(), window_server_ms());
        for n in 0..case.frames {
            if n == WARMUP {
                measured_from = (Instant::now(), cpu_time_us(), window_server_ms());
            }
            let rect = scene.step(case.damage, n);
            let cost = presenter.present(&scene.pixels, &[rect]);
            if n >= WARMUP {
                costs.push(cost);
            }
            next += FRAME;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            } else {
                next = Instant::now();
            }
        }
        let wall = measured_from.0.elapsed();
        let cpu_us = cpu_time_us() - measured_from.1;
        let window_server_ms = window_server_ms() - measured_from.2;
        // Final full present, then read back.
        presenter.present(&scene.pixels, &[full]);
        std::thread::sleep(Duration::from_millis(50));
        let verify = presenter.verify(&scene.premultiplied());
        let extra = presenter.extra();
        Run {
            case,
            costs,
            cpu_us,
            wall,
            window_server_ms,
            footprint_mb: footprint_bytes() as f64 / 1_048_576.0,
            verify,
            extra,
        }
    }

    fn report(run: Run) {
        let case = run.case;
        let frames = run.costs.len().max(1) as f64;
        let mpx = (case.width * case.height) as f64 / 1e6;
        let mut prep: Vec<u64> = run.costs.iter().map(|c| c.prep_us).collect();
        let mut commit: Vec<u64> = run.costs.iter().map(|c| c.commit_us).collect();
        let mut total: Vec<u64> = run.costs.iter().map(|c| c.prep_us + c.commit_us).collect();
        let bytes = run.costs.iter().map(|c| c.bytes).sum::<u64>() as f64 / frames;
        let backend = match case.backend {
            Backend::Tiles => "tiles",
            Backend::Ring => "ring",
            Backend::InPlace => "inplace",
            Backend::Metal => "metal",
        };
        let damage = match case.damage {
            Damage::Full => "full",
            Damage::Band => "band",
            Damage::Cell => "cell",
        };
        let total_p50 = pct(&mut total, 0.5);
        println!(
            "backend={backend} size={}x{} mpx={mpx:.2} scale={} damage={damage} thread={} frames={} \
             prep_us_p50={} prep_us_p95={} commit_us_p50={} commit_us_p95={} total_us_p50={total_p50} \
             total_us_p95={} total_us_max={} us_per_mpx_p50={:.0} upload_kib={:.0} proc_cpu_us_per_frame={:.0} \
             windowserver_cpu_ms_per_s={:.1} footprint_mib={:.0} verify={}{} qos={}",
            case.width,
            case.height,
            case.scale,
            if case.background { "bg" } else { "main" },
            run.costs.len(),
            pct(&mut prep, 0.5),
            pct(&mut prep, 0.95),
            pct(&mut commit, 0.5),
            pct(&mut commit, 0.95),
            pct(&mut total, 0.95),
            pct(&mut total, 1.0),
            total_p50 as f64 / mpx,
            bytes / 1024.0,
            run.cpu_us as f64 / frames,
            run.window_server_ms / run.wall.as_secs_f64(),
            run.footprint_mb,
            match &run.verify {
                Ok(()) => "pixel-identical".to_string(),
                Err(error) => format!("MISMATCH({})", error.replace(' ', "_")),
            },
            run.extra,
            if !case.background {
                "main"
            } else if std::env::var("PROBE_QOS").as_deref() == Ok("default") {
                "default"
            } else {
                "interactive"
            },
        );
    }

    fn cpu_time_us() -> u64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
        tv(usage.ru_utime) + tv(usage.ru_stime)
    }

    fn footprint_bytes() -> u64 {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let status = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        if status == 0 {
            info.ri_phys_footprint
        } else {
            0
        }
    }

    /// Cumulative WindowServer CPU time in ms, read with `ps` (read-only).
    fn window_server_ms() -> f64 {
        let Ok(out) = std::process::Command::new("/bin/ps")
            .args(["-x", "-o", "cputime=", "-c", "-p"])
            .arg(window_server_pid())
            .output()
        else {
            return 0.0;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut ms = 0.0;
        for (i, part) in text.trim().rsplit(':').enumerate() {
            let value: f64 = part.parse().unwrap_or(0.0);
            ms += value * 1000.0 * 60f64.powi(i as i32);
        }
        ms
    }

    fn window_server_pid() -> String {
        std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "WindowServer"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    }

    // ---- window + event loop ---------------------------------------------

    struct App {
        case: Case,
        window: Option<Window>,
        presenter: Option<Box<dyn Presenter>>,
        done: bool,
    }

    pub fn main() {
        let case = parse();
        // Never steal focus from the person using this Mac.
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        let event_loop = EventLoop::builder()
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_activate_ignoring_other_apps(false)
            .build()
            .unwrap();
        let mut app = App {
            case,
            window: None,
            presenter: None,
            done: false,
        };
        event_loop.run_app(&mut app).unwrap();
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let case = self.case;
            let points = LogicalSize::new(
                case.width as f64 / case.scale,
                case.height as f64 / case.scale,
            );
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("present cost probe")
                        .with_active(false)
                        .with_position(LogicalPosition::new(0.0, 30.0))
                        .with_inner_size(points),
                )
                .unwrap();
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected AppKit");
            };
            // SAFETY: winit keeps the view alive with the window.
            let view: &objc2_app_kit::NSView = unsafe { handle.ns_view.cast().as_ref() };
            view.setWantsLayer(true);
            let root = view.layer().unwrap();
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            let container = CALayer::new();
            container.setAnchorPoint(CGPoint::new(0.0, 0.0));
            container.setGeometryFlipped(true);
            container.setFrame(CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(points.width, points.height),
            ));
            container.setZPosition(1.0);
            root.addSublayer(&container);
            let presenter: Box<dyn Presenter> = match case.backend {
                Backend::Tiles => Box::new(Tiles::new(&container, &case)),
                Backend::Ring => Box::new(Surfaces::new(&container, &case, 3)),
                Backend::InPlace => Box::new(Surfaces::new(&container, &case, 1)),
                Backend::Metal => Box::new(Metal::new(&container, &case)),
            };
            CATransaction::commit();
            if case.background {
                let boxed = SendBox(presenter);
                let proxy_done = std::thread::spawn(move || {
                    let mut boxed = boxed;
                    // PROBE_QOS=default keeps the spawned thread's default QoS.
                    if std::env::var("PROBE_QOS").as_deref() != Ok("default") {
                        // SAFETY: sets this thread's own QoS class.
                        unsafe {
                            libc::pthread_set_qos_class_self_np(
                                libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
                                0,
                            )
                        };
                    }
                    let run = run_frames(case, &mut *boxed.0);
                    report(run);
                    std::process::exit(0);
                });
                drop(proxy_done);
            } else {
                self.presenter = Some(presenter);
            }
            self.window = Some(window);
            event_loop.set_control_flow(ControlFlow::Wait);
        }

        fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
            if self.done {
                return;
            }
            if let Some(mut presenter) = self.presenter.take() {
                // Frames from the main thread, one run inside one callback:
                // the same thread and the same explicit transactions.
                self.done = true;
                let run = run_frames(self.case, &mut *presenter);
                report(run);
                std::process::exit(0);
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            if matches!(event, WindowEvent::CloseRequested) {
                event_loop.exit();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    probe::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
