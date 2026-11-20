// SPDX-License-Identifier: MIT
//! Private direct-child runtime probe. Coordination uses a separate test socket.
use anyhow::{ensure, Context, Result};
use lunatui_core::{ClusterTable, Color, Grid, Rect, Style};
use lunatui_image::{Fit, ImageId, ImageRequest, ImageSink, ImageSource, PixelTarget};
use lunatui_term::{Caps, CellPixels, ColorMode, Encoder, GraphicsProtocol, PixelLayer, PixelMode};
use lunatui_theme::{Theme, Variant};
use lunatui_widgets::{Image, Paragraph, RenderCx, Widget, WidgetState};
use native_client_probe::{
    backing::PreparedImage,
    images::{Images, Key},
    session::{Offer, Session},
};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

struct Raw(libc::termios);
impl Raw {
    fn enter() -> Result<Self> {
        let mut original = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::tcgetattr(0, &mut original) } == 0,
            "terminal attributes"
        );
        let terminal = Self(original);
        terminal.enable()?;
        Ok(terminal)
    }

    fn enable(&self) -> Result<()> {
        let mut raw = self.0;
        unsafe { libc::cfmakeraw(&mut raw) };
        ensure!(
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } == 0,
            "raw terminal"
        );
        Ok(())
    }

    fn suspend(&self, coordination: &mut UnixStream) -> Result<()> {
        ensure!(
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) } == 0,
            "restore terminal before suspension"
        );
        report(
            coordination,
            json!({"event":"suspending","pid":std::process::id()}),
        )?;
        ensure!(
            unsafe { libc::kill(libc::getpid(), libc::SIGTSTP) } == 0,
            "suspend foreground application"
        );
        self.enable()?;
        report(
            coordination,
            json!({"event":"resumed","pid":std::process::id()}),
        )?;
        Ok(())
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

struct ObservedSink<'a> {
    inner: &'a mut dyn ImageSink,
    results: Vec<serde_json::Value>,
}
impl ImageSink for ObservedSink<'_> {
    fn supports_pixels(&self) -> bool {
        self.inner.supports_pixels()
    }
    fn pixel_target(&self, area: Rect) -> Option<PixelTarget> {
        self.inner.pixel_target(area)
    }
    fn submit_raster(&mut self, source: &ImageSource, area: Rect, depth: i32) -> bool {
        self.inner.submit_raster(source, area, depth)
    }
    fn submit(&mut self, request: ImageRequest<'_>) -> bool {
        let accepted = self.inner.submit(request);
        self.results.push(
            json!({"image":request.id().get(),"generation":request.generation(),
            "width":request.source().size().width(),"height":request.source().size().height(),
            "source_bytes":request.source().rgba8_pixels().len(),"accepted":accepted}),
        );
        accepted
    }
}

enum Scene {
    Images,
    Text,
}

enum TerminalMode {
    Primary,
    AlternateMouse,
    AlternateNoMouse,
}

struct Application {
    grid: Grid,
    clusters: ClusterTable,
    theme: Theme,
    image: Option<Image>,
    area: Rect,
    ink: bool,
    extra_images: Vec<(u32, Image, Rect)>,
    scene: Scene,
    terminal_mode: TerminalMode,
}
impl Application {
    fn new() -> Result<Self> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0,
            "terminal geometry"
        );
        ensure!(
            size.ws_col >= 4 && size.ws_row >= 4,
            "probe needs at least four cells per axis"
        );
        let mut pixels = Vec::new();
        for y in 0..4 {
            for x in 0..12 {
                pixels.extend_from_slice(&[
                    if x < 6 { 255 } else { 0 },
                    if y < 2 { 0 } else { 255 },
                    if x < 6 { 0 } else { 255 },
                    255,
                ]);
            }
        }
        let source = ImageSource::rgba8(12, 4, pixels.into_boxed_slice())?;
        Ok(Self {
            grid: Grid::new(size.ws_col, size.ws_row),
            clusters: ClusterTable::new(),
            theme: Theme::builtin(Variant::Dark),
            image: Some(
                Image::new(source)
                    .id(ImageId::new(1).unwrap())
                    .fit(Fit::Cover),
            ),
            area: Rect::new(1, 1, 8.min(size.ws_col - 2), 4.min(size.ws_row - 2)),
            ink: false,
            extra_images: Vec::new(),
            scene: Scene::Images,
            terminal_mode: TerminalMode::Primary,
        })
    }

    fn render(&mut self, sink: &mut dyn ImageSink) {
        self.grid.fill(
            Rect::new(0, 0, self.grid.width(), self.grid.height()),
            Style::new()
                .fg(Color::Rgb(255, 255, 255))
                .bg(Color::Rgb(0, 0, 0)),
            ' ',
            &mut self.clusters,
        );
        let label = if matches!(self.scene, Scene::Text) {
            format!(
                "NATIVE-CLUSTER 界e{}\n\nhttps://example.test/native",
                "\u{301}".repeat(20)
            )
        } else {
            "CLASSIC-BASELINE".to_owned()
        };
        Paragraph::new(&label).render(
            Rect::new(0, 0, self.grid.width(), self.grid.height()),
            &mut RenderCx::new(&mut self.grid, &mut self.clusters, &self.theme),
        );
        if matches!(self.scene, Scene::Text) {
            return;
        }
        if self.ink {
            Paragraph::new("X")
                .style(Style::new().fg(Color::Rgb(255, 255, 255)))
                .render(
                    self.area,
                    &mut RenderCx::new(&mut self.grid, &mut self.clusters, &self.theme),
                );
        }
        if let Some(image) = &self.image {
            image.render(
                self.area,
                &mut RenderCx::new(&mut self.grid, &mut self.clusters, &self.theme)
                    .image_sink(&mut *sink),
            );
        }
        for (_, image, area) in &self.extra_images {
            image.render(
                *area,
                &mut RenderCx::new(&mut self.grid, &mut self.clusters, &self.theme)
                    .image_sink(&mut *sink),
            );
        }
    }

    fn install(&mut self, key: Key, source: ImageSource, area: Rect) -> Result<()> {
        let image = Image::new(source)
            .id(ImageId::new(key.0).context("image identity")?)
            .generation(key.1)
            .fit(Fit::Cover);
        if key.0 == 1 {
            self.image = Some(image);
            self.area = area;
        } else if let Some(entry) = self.extra_images.iter_mut().find(|entry| entry.0 == key.0) {
            *entry = (key.0, image, area);
        } else {
            self.extra_images.push((key.0, image, area));
        }
        Ok(())
    }

    fn fallback_cells(&self) -> usize {
        (0..self.grid.height())
            .flat_map(|y| (0..self.grid.width()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                self.grid.get(x, y).is_some_and(|cell| {
                    cell.glyph().decode() == lunatui_core::DecodedGlyph::Inline('▀')
                })
            })
            .count()
    }

    fn reference(&mut self, cell: [u32; 2], path: &str) -> Result<serde_json::Value> {
        let mut caps = Caps::default();
        caps.protocols.graphics = GraphicsProtocol::Iterm2;
        caps.cell_pixels = CellPixels::new(cell[0].try_into()?, cell[1].try_into()?);
        caps.background_rgb = Some([0, 0, 0]);
        let mut layer = PixelLayer::from_caps(&caps);
        layer.begin_frame();
        let results = {
            let mut observed = ObservedSink {
                inner: &mut layer,
                results: Vec::new(),
            };
            self.render(&mut observed);
            observed.results
        };
        let bytes = layer.finish_frame();
        std::fs::write(path, bytes)?;
        Ok(
            json!({"event":"reference","requests":results,"fallback_cells":self.fallback_cells(),"encoded_bytes":bytes.len()}),
        )
    }

    fn ansi(&mut self) -> Result<()> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0,
            "fallback terminal geometry"
        );
        ensure!(
            size.ws_col > 0 && size.ws_row > 0,
            "fallback empty terminal"
        );
        if (size.ws_col, size.ws_row) != (self.grid.width(), self.grid.height()) {
            self.grid = Grid::new(size.ws_col, size.ws_row);
        }
        // Rerun the widget with the real ANSI sink. Suppressed native cells cannot be reused.
        let mut pixels = PixelLayer::new(PixelMode::Cells);
        self.render(&mut pixels);
        let mut encoder = Encoder::new();
        encoder.set_token_palette(self.theme.palette());
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(lunatui_prismattyc::ansi_fallback_repaint_prefix())?;
        match self.terminal_mode {
            TerminalMode::Primary => {}
            TerminalMode::AlternateMouse => {
                stdout.write_all(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h")?;
            }
            TerminalMode::AlternateNoMouse => stdout.write_all(b"\x1b[?1049h")?,
        }
        stdout.write_all(encoder.encode_full(&self.grid, &self.clusters, ColorMode::TrueColor))?;
        stdout.flush()?;
        Ok(())
    }
}

fn discovery(coordination: &mut UnixStream) -> Result<Offer> {
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&lunatui_prismattyc::discovery_query(77).unwrap())?;
    stdout.flush()?;
    drop(stdout);
    report(coordination, json!({"event":"discovery-written"}))?;
    native_client_probe::session::read_offer(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
        77,
        deadline,
    )
}

struct FileImage {
    key: Key,
    width: u32,
    height: u32,
    area: Rect,
    path: PathBuf,
}

enum PreparationInput {
    Source(Key, ImageSource),
    File(FileImage),
}

struct PreparedSource {
    source: ImageSource,
    backing: PreparedImage,
}

struct Preparation {
    result: mpsc::Receiver<Result<PreparedSource>>,
    notification: UnixStream,
    cancelled: Arc<AtomicBool>,
    key: Key,
    replacement: Option<Rect>,
}
impl Preparation {
    fn start(input: PreparationInput) -> Result<Self> {
        let (key, replacement) = match &input {
            PreparationInput::Source(key, _) => (*key, None),
            PreparationInput::File(spec) => (spec.key, Some(spec.area)),
        };
        let (sender, result) = mpsc::channel();
        let (left, right) = prismattyc_process::fd::socket_pair()?;
        let notification = UnixStream::from(left);
        let mut writer = UnixStream::from(right);
        writer.set_nonblocking(true)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("native-probe-prepare".into())
            .spawn(move || {
                let prepared = (|| -> Result<PreparedSource> {
                    let source = match input {
                        PreparationInput::Source(_, source) => source,
                        PreparationInput::File(spec) => {
                            let file = std::fs::File::open(spec.path)?;
                            ensure!(file.metadata()?.is_file(), "regular RGBA file required");
                            let mut bytes = Vec::new();
                            file.take(lunatui_image::MAX_RGBA8_BYTES as u64 + 4)
                                .read_to_end(&mut bytes)?;
                            ImageSource::rgba8(spec.width, spec.height, bytes.into_boxed_slice())?
                        }
                    };
                    let backing = PreparedImage::prepare_cancellable(
                        source.rgba8_pixels(),
                        &worker_cancelled,
                    )?;
                    Ok(PreparedSource { source, backing })
                })();
                let _ = sender.send(prepared);
                let _ = writer.write_all(&[1]);
            })?;
        Ok(Self {
            result,
            notification,
            cancelled,
            key,
            replacement,
        })
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

fn report(control: &mut UnixStream, mut value: serde_json::Value) -> Result<()> {
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    let clock = libc::CLOCK_MONOTONIC;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(clock, &mut now) } == 0,
        "probe monotonic clock"
    );
    let nanos = u64::try_from(now.tv_sec)?
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(u64::try_from(now.tv_nsec).ok()?))
        .context("probe monotonic clock overflow")?;
    value
        .as_object_mut()
        .context("coordination event object")?
        .insert("monotonic_ns".into(), json!(nanos));
    control.write_all(serde_json::to_string(&value)?.as_bytes())?;
    control.write_all(b"\n")?;
    Ok(())
}

fn native(app: &mut Application, coordination: &mut UnixStream, raw: &Raw) -> Result<()> {
    let inherited = std::env::var("PRISMATTYC_NATIVE_FD")?.parse::<i32>()?;
    ensure!(inherited == 3, "native launch descriptor");
    let flags = unsafe { libc::fcntl(inherited, libc::F_GETFD) };
    ensure!(
        flags >= 0
            && unsafe { libc::fcntl(inherited, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
        "native descriptor close-on-exec"
    );
    let stream = unsafe { UnixStream::from_raw_fd(inherited) };
    report(
        coordination,
        json!({"event":"probe-ready","pid":std::process::id()}),
    )?;
    let mut start = [0; 9];
    coordination.read_exact(&mut start)?;
    ensure!(&start == b"discover\n", "explicit discovery command");
    report(coordination, json!({"event":"discover-start"}))?;
    let offer = discovery(coordination)?;
    let epoch = offer.epoch;
    let mut session =
        Session::activate(stream, offer, &app.grid, &app.clusters, app.theme.palette())?;
    let mut images = Images::new(session.cell);
    report(
        coordination,
        json!({"event":"active","frame":1,"epoch":epoch,"cell":session.cell,"columns":app.grid.width(),"rows":app.grid.height()}),
    )?;
    coordination.set_nonblocking(true)?;
    let mut input = Vec::new();
    let mut preparation: Option<Preparation> = None;
    let mut queued_file: Option<FileImage> = None;
    let mut heartbeat = Instant::now() + Duration::from_millis(100);
    let mut sequence = 1;
    let mut render = true;
    let mut omitting = None;
    loop {
        if render {
            images.begin_frame();
            let results = {
                let mut observed = ObservedSink {
                    inner: &mut images,
                    results: Vec::new(),
                };
                app.render(&mut observed);
                observed.results
            };
            sequence = session.present(&app.grid, &app.clusters, images.placements())?;
            report(
                coordination,
                json!({"event":"frame","frame":sequence,"placements":images.placements().len(),"requests":results,"fallback_cells":app.fallback_cells()}),
            )?;
            render = false;
            omitting = Some(sequence);
        }
        if preparation.is_none() {
            if let Some(spec) = queued_file.take() {
                preparation = Some(Preparation::start(PreparationInput::File(spec))?);
            } else if let Some((key, source)) = images.take_request() {
                preparation = Some(Preparation::start(PreparationInput::Source(key, source))?);
            }
        }
        let now = Instant::now();
        if now >= heartbeat {
            let pong = session.ping()?;
            render |= images.pong(&pong.bytes)?;
            heartbeat =
                Instant::now() + Duration::from_millis(if images.pending() { 5 } else { 100 });
            if render {
                continue;
            }
            if let Some(frame) = omitting {
                if let Some((key, handle)) = images.retirement() {
                    let retired = session.retire(handle, frame)?;
                    if images.retired(key, &retired.bytes)? {
                        report(
                            coordination,
                            json!({"event":"retired","image":key.0,"generation":key.1}),
                        )?;
                    }
                }
            }
        }
        let mut descriptors = [
            libc::pollfd {
                fd: coordination.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: preparation
                    .as_ref()
                    .map_or(-1, |job| job.notification.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let wait = heartbeat
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 100) as i32;
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, wait) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(std::io::Error::last_os_error().into());
        }
        if descriptors[2].revents != 0 {
            let mut keys = [0u8; 4096];
            let n = unsafe { libc::read(0, keys.as_mut_ptr().cast(), keys.len()) };
            ensure!(n > 0, "terminal input");
            report(
                coordination,
                json!({"event":"input","bytes":&keys[..n as usize]}),
            )?;
            if keys[..n as usize].contains(&0x1a) {
                raw.suspend(coordination)?;
                session.ping()?;
            }
        }
        if descriptors[1].revents != 0 {
            let job = preparation.take().unwrap();
            let prepared = match job
                .result
                .recv()
                .context("preparation worker disconnected")?
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    images.decline_preparation(job.key);
                    report(
                        coordination,
                        json!({"event":"source-rejected","image":job.key.0,"generation":job.key.1,"error":error.to_string()}),
                    )?;
                    continue;
                }
            };
            if queued_file
                .as_ref()
                .is_some_and(|latest| latest.key.0 == job.key.0 && latest.key.1 != job.key.1)
            {
                continue;
            }
            if let Some(area) = job.replacement {
                app.install(job.key, prepared.source.clone(), area)?;
            }
            let reply = session.upload(job.key, &prepared.source, prepared.backing)?;
            images.admission(job.key, &reply.bytes)?;
            if images.pending() {
                heartbeat = Instant::now();
            }
            report(
                coordination,
                json!({"event":"admission","image":job.key.0,"generation":job.key.1,"status":u32::from_le_bytes(reply.bytes[40..44].try_into().unwrap()),"handle":u64::from_le_bytes(reply.bytes[32..40].try_into().unwrap()),"source_bytes":prepared.source.rgba8_pixels().len()}),
            )?;
            render = true;
        }
        if descriptors[0].revents != 0 {
            let mut bytes = [0; 1024];
            match coordination.read(&mut bytes) {
                Ok(0) => {
                    session.release()?;
                    return Ok(());
                }
                Ok(n) => input.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e.into()),
            }
            ensure!(input.len() <= 4096, "coordination command bound");
            while let Some(end) = input.iter().position(|&byte| byte == b'\n') {
                let command = String::from_utf8(input.drain(..=end).collect())?;
                let line = command.trim_end_matches('\n');
                if let Some(path) = line.strip_prefix("reference ") {
                    report(coordination, app.reference(session.cell, path)?)?;
                    continue;
                }
                let parts: Vec<_> = line.splitn(8, ' ').collect();
                match parts.as_slice() {
                    ["text-scene"] => {
                        app.scene = Scene::Text;
                        render = true;
                    }
                    ["load", id, generation, width, height, x, y, path] => {
                        let id = id.parse::<u32>()?;
                        ensure!((1..=256).contains(&id), "image identity range");
                        queued_file = Some(FileImage {
                            key: (id, generation.parse()?),
                            width: width.parse()?,
                            height: height.parse()?,
                            area: Rect::new(x.parse()?, y.parse()?, 8, 4),
                            path: PathBuf::from(path),
                        });
                    }
                    ["heartbeat"] => {
                        let pong = session.ping()?;
                        render |= images.pong(&pong.bytes)?;
                        heartbeat = Instant::now() + Duration::from_millis(100);
                        report(coordination, json!({"event":"heartbeat-complete"}))?;
                    }
                    ["suspend"] => {
                        raw.suspend(coordination)?;
                        session.ping()?;
                    }
                    ["ink", value] => {
                        app.ink = match *value {
                            "on" => true,
                            "off" => false,
                            _ => anyhow::bail!("ink state"),
                        };
                        render = true;
                    }
                    ["state", value] => {
                        let state = match *value {
                            "normal" => WidgetState::Normal,
                            "disabled" => WidgetState::Disabled,
                            _ => anyhow::bail!("image state"),
                        };
                        if let Some(image) = app.image.take() {
                            app.image = Some(image.state(state));
                            render = true;
                        }
                    }
                    ["omit"] => {
                        app.image = None;
                        render = true;
                    }
                    ["move", x, y] => {
                        app.area.x = x.parse()?;
                        app.area.y = y.parse()?;
                        render = true;
                    }
                    ["depth", value] => {
                        if let Some(image) = app.image.take() {
                            app.image = Some(image.depth(value.parse()?));
                            render = true;
                        }
                    }
                    ["fit", value] => {
                        let fit = match *value {
                            "cover" => Fit::Cover,
                            "contain" => Fit::Contain,
                            "stretch" => Fit::Stretch,
                            "native" => Fit::Native,
                            _ => anyhow::bail!("unknown fit"),
                        };
                        if let Some(image) = app.image.take() {
                            app.image = Some(image.fit(fit));
                            render = true;
                        }
                    }
                    ["release-hold"] => {
                        session.release()?;
                        report(coordination, json!({"event":"released"}))?;
                        coordination.set_nonblocking(false)?;
                        let _ = coordination.read(&mut [0]);
                        return Ok(());
                    }
                    ["release"] => {
                        session.release()?;
                        report(coordination, json!({"event":"released"}))?;
                        return Ok(());
                    }
                    ["snapshot"] => report(
                        coordination,
                        json!({"event":"snapshot","frame":sequence,"placements":images.placements().len()}),
                    )?,
                    _ => anyhow::bail!("unknown command"),
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn legacy(app: &mut Application, coordination: &mut UnixStream) -> Result<()> {
    use native_client_probe::legacy::LegacySession;
    let descriptor = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
    ensure!(descriptor >= 0, "legacy inherited control descriptor");
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    report(
        coordination,
        json!({"event":"probe-ready","pid":std::process::id()}),
    )?;
    let mut start = [0; 9];
    coordination.read_exact(&mut start)?;
    ensure!(&start == b"discover\n", "explicit legacy discovery command");
    let deadline = Instant::now() + Duration::from_millis(200);
    std::io::stdout().write_all(&lunatui_prismattyc::discovery_query(77).unwrap())?;
    std::io::stdout().flush()?;
    let reply = native_client_probe::session::read_capability(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(0) },
        deadline,
    )?;
    let mut session = LegacySession::activate(
        stream,
        77,
        &reply,
        &app.grid,
        &app.clusters,
        app.theme.palette(),
    )?;
    report(
        coordination,
        json!({"event":"active","frame":1,"epoch":session.epoch,"minor":0,"columns":app.grid.width(),"rows":app.grid.height(),"fallback_cells":app.fallback_cells()}),
    )?;
    coordination.set_nonblocking(true)?;
    let mut input = Vec::new();
    let mut heartbeat = Instant::now() + Duration::from_millis(100);
    loop {
        if Instant::now() >= heartbeat {
            session.ping()?;
            heartbeat = Instant::now() + Duration::from_millis(100);
        }
        let mut descriptor = libc::pollfd {
            fd: coordination.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = heartbeat
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 100) as i32;
        let n = unsafe { libc::poll(&mut descriptor, 1, wait) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(std::io::Error::last_os_error().into());
        }
        if descriptor.revents == 0 {
            continue;
        }
        let mut chunk = [0; 4096];
        let count = coordination.read(&mut chunk)?;
        ensure!(count != 0, "legacy coordinator disconnected");
        input.extend_from_slice(&chunk[..count]);
        ensure!(input.len() <= 4096, "legacy command bound");
        while let Some(end) = input.iter().position(|&byte| byte == b'\n') {
            let line = String::from_utf8(input.drain(..=end).collect())?;
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["release"] => {
                    session.release()?;
                    report(coordination, json!({"event":"released"}))?;
                    return Ok(());
                }
                ["heartbeat"] => {
                    session.ping()?;
                    report(coordination, json!({"event":"heartbeat-complete"}))?;
                    continue;
                }
                ["omit"] => app.image = None,
                ["move", x, y] => {
                    app.area = Rect::new(x.parse()?, y.parse()?, app.area.width, app.area.height)
                }
                _ => anyhow::bail!("unknown legacy probe command"),
            }
            let mut cells = PixelLayer::new(PixelMode::Cells);
            app.render(&mut cells);
            let frame = session.present(&app.grid, &app.clusters)?;
            report(
                coordination,
                json!({"event":"frame","frame":frame,"placements":0,"fallback_cells":app.fallback_cells()}),
            )?;
        }
    }
}

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.len() == 2 && arguments[1] == "--version" {
        println!(
            "native-client-probe {}",
            option_env!("NATIVE_PROBE_REVISION").unwrap_or("unrecorded")
        );
        return Ok(());
    }
    ensure!(
        arguments.len() == 3
            && matches!(
                arguments[1].as_str(),
                "--direct-local" | "--direct-local-alt" | "--direct-local-alt-no-mouse" | "--legacy-direct-local"
            ),
        "usage: native-client-probe --direct-local|--direct-local-alt|--direct-local-alt-no-mouse|--legacy-direct-local COORDINATION_SOCKET"
    );
    let mut coordination = UnixStream::connect(&arguments[2])?;
    let raw = Raw::enter()?;
    let mut app = Application::new()?;
    app.terminal_mode = match arguments[1].as_str() {
        "--direct-local-alt" => TerminalMode::AlternateMouse,
        "--direct-local-alt-no-mouse" => TerminalMode::AlternateNoMouse,
        _ => TerminalMode::Primary,
    };
    app.ansi()?;
    let result = if arguments[1] == "--legacy-direct-local" {
        #[cfg(target_os = "linux")]
        {
            legacy(&mut app, &mut coordination)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow::anyhow!("legacy native transport requires Linux"))
        }
    } else {
        native(&mut app, &mut coordination, &raw)
    };
    // Every exit rerenders fallback cells and emits a complete ANSI recovery frame.
    app.ansi()?;
    if let Err(error) = result {
        report(
            &mut coordination,
            json!({"event":"fallback","error":error.to_string(),"columns":app.grid.width(),"rows":app.grid.height(),"ended_reason":error.downcast_ref::<native_client_probe::session::SessionEnded>().map(|ended| ended.reason)}),
        )?;
        coordination.set_nonblocking(false)?;
        let _ = coordination.read(&mut [0]);
    }
    Ok(())
}
