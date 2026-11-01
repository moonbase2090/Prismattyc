// SPDX-License-Identifier: MIT

//! A small host loop around Lunatui's session, encoder and pixel layer.
//!
//! Lunatui 0.1.18's `run` draws into a grid of fixed size. A monitor in a pane has to follow
//! the pane through splits and resizes, so this loop reads the terminal size every frame and
//! repaints in full when it changes.

use std::io::{self, IsTerminal};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Result};
use lunatui_core::Rect;
use lunatui_image::ImageSink;
use lunatui_term::{
    Event, MouseTracking, PixelLayer, PixelMode, Session, SessionOptions, SyncOutput,
};
use pmux_plugin_api::host::pmux::view::frame;

use super::paint::Painter;
use super::{Client, Reply, Request};

/// Longest wait for input before checking terminal geometry again.
const IDLE_WAIT: Duration = Duration::from_millis(50);

pub struct Probe {
    pub layer: PixelLayer,
    pub background_dark: Option<bool>,
    pending_input: Vec<u8>,
}

/// Asks the terminal what it supports. Without a TTY, or with pixels off, it draws cells.
pub fn probe(pixels: bool) -> Probe {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::fd::AsFd;

        use lunatui_term::{probe_fd, ProbeEnv};

        if let Ok(tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") {
            if let Ok(reply) = probe_fd(tty.as_fd(), &ProbeEnv::from_process()) {
                let mut pending_input = reply.pending_input;
                let mode = pixel_mode(pixels, &reply.caps, || {
                    let (shown, typed) = raw_upload_shown(&tty);
                    pending_input.extend(typed);
                    shown
                });
                let layer = if mode.is_pixels() {
                    PixelLayer::from_caps(&reply.caps)
                } else {
                    PixelLayer::new(PixelMode::Cells)
                };
                return Probe {
                    layer,
                    background_dark: reply.caps.background_dark,
                    pending_input,
                };
            }
        }
    }
    let _ = pixels;
    Probe {
        layer: PixelLayer::new(PixelMode::Cells),
        background_dark: None,
        pending_input: Vec::new(),
    }
}

/// Chooses true pixels only where the uploads will be shown.
///
/// Lunatui 0.1.18 uploads raw RGBA (`f=32`). Prismattyc answers every graphics query with OK,
/// but releases before raw-upload support drop raw uploads, which would leave lanes erased and
/// blank. So inside Prismattyc pixels need `raw_upload_shown`, a real upload that is answered.
pub fn pixel_mode(
    requested: bool,
    caps: &lunatui_term::Caps,
    raw_upload_shown: impl FnOnce() -> bool,
) -> PixelMode {
    let mode = if requested {
        PixelMode::from_caps(caps)
    } else {
        PixelMode::Cells
    };
    if mode.is_pixels() && caps.identity.prismattyc_native && !raw_upload_shown() {
        PixelMode::Cells
    } else {
        mode
    }
}

/// Kitty image number used for the raw-upload probe.
const PROBE_IMAGE: u32 = 4_242;

/// Uploads one 1x1 raw RGBA image with an image number, then asks for device attributes.
/// A terminal that keeps the image answers `OK` before the attributes. The image is deleted
/// again. Returns whether it was answered, and any other input read meanwhile.
#[cfg(unix)]
fn raw_upload_shown(tty: &std::fs::File) -> (bool, Vec<u8>) {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let fd = tty.as_raw_fd();
    // SAFETY: tcgetattr fills the zeroed termios for this open terminal descriptor.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return (false, Vec::new());
    }
    let mut raw = saved;
    // SAFETY: cfmakeraw only edits the struct; tcsetattr applies it to the same descriptor.
    unsafe {
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(fd, libc::TCSANOW, &raw);
    }
    let mut out = tty;
    let query = format!("\x1b_Ga=t,I={PROBE_IMAGE},f=32,s=1,v=1;AAAAAA==\x1b\\\x1b[c");
    let mut read = Vec::new();
    if out
        .write_all(query.as_bytes())
        .and_then(|()| out.flush())
        .is_ok()
    {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut buffer = [0u8; 512];
        while Instant::now() < deadline && !scan_raw_probe(&read).answered {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for an open descriptor, with a bounded timeout.
            if unsafe { libc::poll(&mut poll, 1, 50) } > 0 {
                match (&*tty).read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => read.extend_from_slice(&buffer[..count]),
                }
            }
        }
    }
    let scan = scan_raw_probe(&read);
    if let Some(id) = scan.image_id {
        let _ = out.write_all(format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").as_bytes());
    }
    // SAFETY: restores the attributes saved above on the same descriptor.
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &saved);
    }
    (scan.image_id.is_some(), scan.rest)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RawProbe {
    /// The terminal's id for the probe image, when it answered OK.
    image_id: Option<u32>,
    /// Device attributes arrived, so no later OK is coming.
    answered: bool,
    /// Bytes that belong to neither reply, such as keys typed during the probe.
    rest: Vec<u8>,
}

fn scan_raw_probe(bytes: &[u8]) -> RawProbe {
    let mut probe = RawProbe::default();
    let mut index = 0;
    while index < bytes.len() {
        let tail = &bytes[index..];
        if tail.starts_with(b"\x1b_G") {
            if let Some(end) = find(tail, b"\x1b\\") {
                let body = String::from_utf8_lossy(&tail[3..end]);
                let (control, message) = body.split_once(';').unwrap_or((&body, ""));
                let number = format!("I={PROBE_IMAGE}");
                if control.split(',').any(|field| field == number) && message == "OK" {
                    probe.image_id = control
                        .split(',')
                        .find_map(|field| field.strip_prefix("i="))
                        .and_then(|id| id.parse().ok());
                }
                index += end + 2;
                continue;
            }
        }
        if tail.starts_with(b"\x1b[?") {
            if let Some(end) = tail.iter().position(|b| *b == b'c') {
                probe.answered = true;
                index += end + 1;
                continue;
            }
        }
        probe.rest.push(bytes[index]);
        index += 1;
    }
    probe
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The pane size in cells, or `None` when it cannot be read.
pub fn size() -> Option<(u16, u16)> {
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ writes one winsize into the zeroed struct we pass.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO] {
            // SAFETY: fd is a standard descriptor and size is a valid out pointer.
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col > 0
                && size.ws_row > 0
            {
                return Some((size.ws_col, size.ws_row));
            }
        }
        None
    }
    #[cfg(windows)]
    {
        crossterm::terminal::size()
            .ok()
            .filter(|(w, h)| *w > 0 && *h > 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

struct Input {
    receiver: Option<mpsc::Receiver<io::Result<Vec<Event>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Dropping the receiver also releases a producer blocked on the bound.
        drop(self.receiver.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_input(pending: Vec<u8>) -> io::Result<Input> {
    let (sender, receiver) = mpsc::sync_channel(8);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let thread = std::thread::Builder::new()
        .name("pmux-view-input".into())
        .spawn(move || {
            #[cfg(unix)]
            {
                use std::io::Read;
                let mut parser = lunatui_term::Parser::new();
                let send = |mut events: Vec<Event>| {
                    events.retain(|event| matches!(event, Event::Key(_)));
                    events.is_empty() || sender.send(Ok(events)).is_ok()
                };
                if !send(decode(&mut parser, &pending)) {
                    return;
                }
                let mut input = io::stdin();
                let mut buffer = [0; 4096];
                while !stopped.load(Ordering::Acquire) {
                    let mut descriptor = libc::pollfd {
                        fd: libc::STDIN_FILENO,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: descriptor is valid writable storage for one pollfd.
                    let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
                    if ready == 0 {
                        continue;
                    }
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        let _ = sender.send(Err(error));
                        return;
                    }
                    match input.read(&mut buffer) {
                        Ok(0) => {
                            let _ = send(parser.flush());
                            return;
                        }
                        Ok(length) => {
                            if !send(decode(&mut parser, &buffer[..length])) {
                                return;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    }
                }
            }
            #[cfg(windows)]
            {
                let _ = pending;
                while !stopped.load(Ordering::Acquire) {
                    let event = (|| -> io::Result<Option<Event>> {
                        if !crossterm::event::poll(Duration::from_millis(100))? {
                            return Ok(None);
                        }
                        Ok(lunatui_term::sys::windows::map_event(
                            crossterm::event::read()?,
                        ))
                    })();
                    match event {
                        Ok(Some(event @ Event::Key(_))) => {
                            if sender.send(Ok(vec![event])).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(Input {
        receiver: Some(receiver),
        stop,
        thread: Some(thread),
    })
}

/// Parses one read from the terminal.
///
/// A read that ends in ESC is the Esc key: terminals write each escape sequence in one piece,
/// and no sequence ends with ESC. Without this, Esc waits for the next key.
#[cfg(unix)]
fn decode(parser: &mut lunatui_term::Parser, bytes: &[u8]) -> Vec<Event> {
    let mut events = parser.push(bytes);
    if bytes.last() == Some(&0x1b) {
        events.extend(parser.flush());
    }
    events
}

struct Frontend {
    client: Client,
    handle: String,
    viewport: frame::Viewport,
    painter: Painter,
    next_tick: Instant,
    resize: bool,
}

enum Flow {
    Continue,
    Closed,
}

impl Frontend {
    fn event(&mut self, event: frame::Event, session: &mut Session<io::Stdout>) -> Result<Flow> {
        let tick = matches!(&event, frame::Event::Tick(_));
        let reply = self.client.request(Request::ViewEvent {
            handle: self.handle.clone(),
            event,
        })?;
        let interval = match reply {
            Reply::Frame {
                handle,
                content,
                next_tick_ms,
            } => {
                ensure!(
                    handle == self.handle && (33..=1000).contains(&next_tick_ms),
                    "invalid view frame metadata"
                );
                let frame = super::decode_frame(&content, &self.viewport)?;
                if let Some(output) = self.painter.encode(frame) {
                    if !output.upload.is_empty() {
                        session.present_frame(&output.upload)?;
                    }
                    if !output.paint.is_empty() {
                        session.present_frame(&output.paint)?;
                    }
                } else {
                    ensure!(
                        self.viewport.pixels.is_some(),
                        "view frame cannot be presented in cell mode"
                    );
                    let clear = self.painter.clear();
                    if !clear.is_empty() {
                        session.present_frame(&clear)?;
                    }
                    self.viewport.pixels = None;
                    self.resize = true;
                }
                next_tick_ms
            }
            Reply::ViewClosed { handle } => {
                ensure!(handle == self.handle, "invalid closed-view handle");
                return Ok(Flow::Closed);
            }
            Reply::Idle | Reply::Retry { .. } => 250,
            _ => bail!("unexpected component view reply"),
        };
        let deadline = Instant::now() + Duration::from_millis(u64::from(interval));
        self.next_tick = if tick {
            deadline
        } else {
            self.next_tick.min(deadline)
        };
        Ok(Flow::Continue)
    }

    fn drive(
        &mut self,
        input: &mpsc::Receiver<io::Result<Vec<Event>>>,
        session: &mut Session<io::Stdout>,
    ) -> Result<()> {
        let epoch = Instant::now();
        let mut keys = std::collections::VecDeque::new();
        loop {
            let viewport = self::viewport(self.viewport.pixels.clone());
            if viewport != self.viewport {
                self.viewport = viewport;
                self.resize = true;
            }
            if self.resize {
                self.resize = false;
                if matches!(
                    self.event(frame::Event::Resize(self.viewport.clone()), session)?,
                    Flow::Closed
                ) {
                    break;
                }
                if self.resize {
                    continue;
                }
            }
            if Instant::now() >= self.next_tick {
                if matches!(
                    self.event(frame::Event::Tick(clock(epoch)?), session)?,
                    Flow::Closed
                ) {
                    break;
                }
                if self.resize {
                    continue;
                }
            }
            if let Some(key) = keys.pop_front() {
                if matches!(self.event(frame::Event::Key(key), session)?, Flow::Closed) {
                    break;
                }
                continue;
            }
            let wait = self
                .next_tick
                .saturating_duration_since(Instant::now())
                .min(IDLE_WAIT);
            match input.recv_timeout(wait) {
                Ok(Ok(events)) => {
                    if events.iter().any(|event| matches!(event, Event::Key(key) if key.kind != lunatui_term::KeyKind::Release && key.modifiers.is_control() && matches!(key.code, lunatui_term::KeyCode::Char('c'|'C')))) { break; }
                    keys.extend(events.iter().filter_map(|event| match event {
                        Event::Key(key) => super::input::key(key),
                        _ => None,
                    }));
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

/// Drive a component view while the native frontend owns terminal state.
pub(super) fn run(
    mut client: Client,
    id: String,
    contribution: String,
    mut preferences: frame::Preferences,
    pixels: bool,
    endpoints: Option<Vec<super::ViewEndpoint>>,
) -> Result<()> {
    ensure!(
        io::stdout().is_terminal(),
        "the live view needs an interactive terminal; use --snapshot"
    );
    let probe = probe(pixels);
    preferences.dark_background = probe.background_dark;
    let cell_pixels = probe
        .layer
        .pixel_target(Rect::new(0, 0, 1, 1))
        .and_then(|target| {
            Some(frame::PixelSize {
                width: target.size().width().try_into().ok()?,
                height: target.size().height().try_into().ok()?,
            })
        });
    let viewport = viewport(cell_pixels);
    let Reply::ViewOpened { handle } = client.request(Request::OpenView {
        endpoints,
        id,
        contribution,
        viewport: viewport.clone(),
        preferences,
    })?
    else {
        bail!("expected a view handle");
    };
    let mut view = Frontend {
        client,
        handle,
        viewport,
        painter: Painter::new(probe.layer),
        next_tick: Instant::now(),
        resize: false,
    };
    let pending_input = probe.pending_input;
    let result = (|| -> Result<()> {
        let mut session = Session::enter_with_options(
            io::stdout(),
            SessionOptions {
                sync_output: SyncOutput::Enabled,
                mouse_tracking: MouseTracking::Disabled,
            },
        )?;
        let input = spawn_input(pending_input)?;
        let result = view.drive(
            input.receiver.as_ref().expect("live input receiver"),
            &mut session,
        );
        let clear = view.painter.clear();
        let cleanup = if clear.is_empty() {
            Ok(())
        } else {
            session.present_frame(&clear).map_err(anyhow::Error::from)
        };
        result.and(cleanup)
    })();
    let _ = view.client.request(Request::CloseView {
        handle: view.handle.clone(),
    });
    result
}

fn viewport(pixels: Option<frame::PixelSize>) -> frame::Viewport {
    let (columns, rows) = size().unwrap_or((80, 24));
    let columns = columns.clamp(1, 500);
    let rows = rows
        .clamp(1, 200)
        .min((pmux_plugin_api::MAX_VIEW_CELLS / usize::from(columns)) as u16);
    frame::Viewport {
        columns,
        rows,
        pixels,
    }
}

fn clock(epoch: Instant) -> Result<frame::Clock> {
    Ok(frame::Clock {
        wall_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
        monotonic_ms: mono_ms(epoch.elapsed()),
    })
}

fn mono_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunatui_term::{KeyCode, Parser};

    fn codes(events: &[Event]) -> Vec<KeyCode> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Key(key) => Some(key.code),
                _ => None,
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_lone_escape_is_the_esc_key_without_waiting_for_more_input() {
        let mut parser = Parser::new();
        assert_eq!(codes(&decode(&mut parser, b"\x1b")), vec![KeyCode::Esc]);
        assert_eq!(codes(&decode(&mut parser, b"f")), vec![KeyCode::Char('f')]);
    }

    #[cfg(unix)]
    #[test]
    fn whole_escape_sequences_still_parse_as_their_keys() {
        let mut parser = Parser::new();
        assert_eq!(
            codes(&decode(&mut parser, b"\x1b[A\x1b[B")),
            vec![KeyCode::Up, KeyCode::Down]
        );
    }
    use lunatui_term::{Caps, GraphicsProtocol, ProbeEnv};

    fn kitty(term_program: &str) -> Caps {
        let env = ProbeEnv {
            term_program: term_program.to_owned(),
            ..ProbeEnv::default()
        };
        let mut caps = Caps::from_env(&env);
        caps.protocols.graphics = GraphicsProtocol::Kitty;
        caps
    }

    #[test]
    fn kitty_terminals_get_pixels_unless_turned_off_without_a_raw_probe() {
        let never = || -> bool { panic!("only Prismattyc needs the raw-upload probe") };
        assert_eq!(pixel_mode(true, &kitty("kitty"), never), PixelMode::Kitty);
        assert_eq!(pixel_mode(false, &kitty("kitty"), never), PixelMode::Cells);
    }

    #[test]
    fn prismattyc_gets_pixels_only_when_a_raw_upload_is_answered() {
        let caps = kitty("prismattyc");
        assert!(caps.identity.prismattyc_native);
        assert_eq!(pixel_mode(true, &caps, || true), PixelMode::Kitty);
        assert_eq!(pixel_mode(true, &caps, || false), PixelMode::Cells);
        assert_eq!(pixel_mode(false, &caps, || true), PixelMode::Cells);
    }

    #[test]
    fn a_terminal_without_graphics_is_never_probed() {
        let env = ProbeEnv {
            term_program: "prismattyc".to_owned(),
            ..ProbeEnv::default()
        };
        let caps = Caps::from_env(&env);
        assert_eq!(
            pixel_mode(true, &caps, || panic!("probed")),
            PixelMode::Cells
        );
    }

    #[test]
    fn an_ok_for_the_probe_image_before_device_attributes_means_raw_uploads_are_shown() {
        let probe = scan_raw_probe(b"\x1b_Gi=7,I=4242;OK\x1b\\\x1b[?62;22c");
        assert_eq!(probe.image_id, Some(7));
        assert!(probe.answered);
        assert!(probe.rest.is_empty());
    }

    #[test]
    fn device_attributes_alone_mean_the_upload_was_dropped() {
        let probe = scan_raw_probe(b"\x1b[?62;22c");
        assert_eq!(probe.image_id, None);
        assert!(probe.answered);
    }

    #[test]
    fn errors_and_other_images_do_not_count_and_typed_keys_are_kept() {
        let probe =
            scan_raw_probe(b"j\x1b_Gi=7,I=4242;ENODATA:bad\x1b\\\x1b_Gi=8,I=1;OK\x1b\\k\x1b[?62c");
        assert_eq!(probe.image_id, None);
        assert!(probe.answered);
        assert_eq!(probe.rest, b"jk");
        let pending = scan_raw_probe(b"\x1b_Gi=7,I=4242;OK\x1b\\");
        assert_eq!(pending.image_id, Some(7));
        assert!(
            !pending.answered,
            "keep reading until the attributes arrive"
        );
    }
}
