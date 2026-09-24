//! Thin local attach client for the Phase 2B control protocol.
//!
//! Non-TTY (or `--json`) prints the bounded `ReadPane` projection as JSON so
//! scripts stay working. `--styled-json` exposes the bounded styled and
//! rich projection for proof/debug tooling. TTY stdin and stdout enter an interactive raw-mode
//! client that paints the pane, forwards keys via `WritePane`, and detaches
//! on `C-\ d` without killing the server-owned child. PageUp / `C-\ [`
//! enter local copy mode over server-side scrollback. `C-\ a` cycles
//! named arrangements. `/` and `?` search
//! that viewport (`n`/`N` wrap). The mouse wheel enters
//! ordinary scroll mode via SGR mouse reporting. When the child enabled mouse
//! tracking, or when pane history is empty, the wheel is forwarded to the child.
//! An idle observer re-acquires the lease for that forward so
//! alt-screen TUIs with no mux history (Claude Code) still receive the wheel.
//! DECAWM is off while attached so a
//! full-width row does not wrap. Erase-line runs *before* glyphs so
//! it cannot wipe the last column. MailAttention `depth > 0` paints
//! the envelope as one Nerd Font letter cell (U+F0E0) after the
//! guest grid; it does not mutate server cells. With `--experimental-rich`,
//! C-S-G grants `input.rich_focus` on a requested overlay.

use std::{
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{bail, Context, Result};
use prismattyc_mux::local_socket::UnixStream;
use std::io::{IsTerminal, Read};
#[cfg(windows)]
#[path = "pmux-attach/windows_terminal.rs"]
mod windows_terminal;
use prismattyc_core::{
    for_each_display_scalar, line_display_width,
    splash::{INK, SPECTRUM},
};
use prismattyc_mux::{
    attach_pty_fallback, attach_tabs, classify_control_request_id, default_socket_path,
    diagnose_runtime_dir_miss_from_env, expand_empty_bracketed_paste, host_ack_path_from_socket,
    list_spaces, load_space, next_stale_skip, prism_config_path, probe_socket_liveness,
    route_seat_to_host, should_host_route_seat, space_sessions_in_tab_order, spaces_dir,
    wait_host_ack, ArrangementWire, ColorWire, ControlError, ControlErrorCode, ControlIdMatch,
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, Event, OverlayKind,
    OverlayRun, PaneContent, PaneOverlay, PaneStyled, RichInputKind, RichPointerPhase, Snapshot,
    SocketLiveness, SpawnSpec, StyleRun, WorkspaceInverseRun, WorkspaceStyleRun, PROTOCOL_VERSION,
};
use prismattyc_protocol::encode_focus_key;
#[cfg(unix)]
use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    termios::{self, OptionalActions, Termios},
};
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use windows_terminal::PollFlags;

const DETACH_PREFIX: u8 = 0x1c; // C-\
/// History lines per mouse-wheel notch.
const WHEEL_LINES: u32 = 3;
#[cfg(unix)]
const POLL_WAIT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 50_000_000,
};
/// Release the controller lease when attach is idle so InjectMail can AcquireLease
///. Holding the lease for the whole attach made every live pane
/// `deferred_lease`.
const LEASE_IDLE: Duration = Duration::from_millis(750);
/// Viewport toast on TTY attach: "session NAME is attached" (host chrome, not
/// an app overlay). Same linger as the host chord strip. Chip fill is the
/// host focus-border color (`PRISMATTYC_FOCUS_BORDER` / `focus_border`).
const ATTACH_TOAST_LINGER: Duration = Duration::from_millis(3000);
/// How often attach re-reads space file mtimes (PT-205). Not every poll.
const SPACE_STAMP_INTERVAL: Duration = Duration::from_secs(1);
/// Socket read/write budget for one attach control request.
/// Hang-prevention for the TTY poll loop, not an operation SLA.
/// Measured 2026-09-07 on this box: 200 Snapshot round-trips on a local
/// pmuxd unix socket (same `Client::request` path `tests/native/spaces-e2e.sh` uses):
/// p50 = 0.15 ms, p99 = 0.55 ms. 2 s is >3000× that p99 so a loaded host
/// still returns before the attach poll (50 ms) looks wedged, without
/// waiting SubscribePane's 7 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Host default: blue (`FOCUS_BORDER_PALETTE` index 4).
const DEFAULT_TOAST_FOCUS_RGB: [u8; 3] = [0x62, 0xa8, 0xff];

struct Cli {
    socket: PathBuf,
    pane: Option<u64>,
    session: Option<String>,
    session_id: Option<u64>,
    create_session: Option<String>,
    space: Option<String>,
    write: Option<String>,
    watch: bool,
    json: bool,
    styled_json: bool,
    read_only: bool,
    fit: bool,
}

impl Cli {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut socket = None;
        let mut pane = None;
        let mut session = None;
        let mut session_id = None;
        let mut create_session = None;
        let mut space = None;
        let mut write = None;
        let mut watch = false;
        let mut json = false;
        let mut styled_json = false;
        let mut read_only = false;
        let mut fit = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("{}", prismattyc_core::bin_version("pmux-attach"));
                    std::process::exit(0);
                }
                "--socket" => {
                    socket = Some(PathBuf::from(
                        args.next().context("--socket requires PATH")?,
                    ));
                }
                "--pane" => {
                    pane = Some(
                        args.next()
                            .context("--pane requires an opaque numeric pane ID")?
                            .parse::<u64>()
                            .context("--pane requires an opaque numeric pane ID")?,
                    );
                }
                "--session" => {
                    session = Some(args.next().context("--session requires NAME or ID")?);
                }
                "--session-id" => {
                    session_id = Some(
                        args.next()
                            .context("--session-id requires an opaque numeric session ID")?
                            .parse::<u64>()
                            .context("--session-id requires an opaque numeric session ID")?,
                    );
                }
                "--create-session" => {
                    create_session = Some(args.next().context("--create-session requires NAME")?);
                }
                "--space" => {
                    space = Some(args.next().context("--space requires NAME")?);
                }
                "--write" => write = Some(args.next().context("--write requires UTF-8 text")?),
                "--watch" => watch = true,
                "--json" => json = true,
                "--styled-json" => styled_json = true,
                "--read-only" => read_only = true,
                "--fit" => fit = true,
                _ => bail!("unknown argument {arg:?}"),
            }
        }
        if json && styled_json {
            bail!("--json and --styled-json are mutually exclusive");
        }
        if read_only && write.is_some() {
            bail!("--read-only cannot be combined with --write");
        }
        Ok(Self {
            socket: socket.unwrap_or(default_socket_path("default")?),
            pane,
            session,
            session_id,
            create_session,
            space: space.or_else(|| {
                std::env::var("PMUX_SPACE")
                    .ok()
                    .filter(|value| !value.is_empty())
            }),
            write,
            watch,
            json,
            styled_json,
            read_only,
            fit,
        })
    }
}

fn print_help() {
    eprintln!(
        "\
pmux-attach — thin same-user local attach client

USAGE:
    pmux-attach [--socket PATH] [--pane ID] [--session NAME|ID]
                     [--session-id ID] [--create-session NAME]
                     [--space NAME] [--write TEXT] [--watch]
                     [--json|--styled-json] [--read-only] [--fit]

Registers a connection-bound client identity, takes a fresh snapshot, and
reads server-owned pane content. --write first acquires the pane controller
lease.

TTY stdin and stdout enter interactive raw mode (paint + keys + SIGWINCH).
Detach
with C-\\ then d; the pane and child stay alive. PageUp or C-\\ [ enter
scrollback. The mouse wheel scrolls host history when the child has no
mouse mode and history is non-empty; otherwise attach forwards the wheel
to the child, re-acquiring an idle lease first. PageUp
stays live when history is empty so the child still sees it. Esc/q or
wheeling back to the tail returns to live;
plain drag selects while attach is idle. Hold Shift when the
child has application mouse and this attach holds the lease. MailAttention lights a
one-cell Nerd envelope (U+F0E0) at the upper-left. With the server
`--experimental-rich` flag, C-S-G grants input.rich_focus on a requested
overlay; Esc or C-S-G revokes. Flag-off leaves C-S-G for the child.
--json, or a
non-TTY stdin, prints the JSON ReadPane dump used by. --watch keeps
polling that projection (JSON) or stays attached (TTY). --styled-json selects
the richer ReadPaneStyled dump for local proof/debug tooling without changing
the stable --json shape.
--read-only never acquires the controller lease. Keys are dropped. Scroll
and copy still work. The attach chrome shows [ro].
--fit (or C-\\ z) resizes the pane to this terminal even when another
viewer is attached. Default [mux] remote_size=latest also fits on attach
and SIGWINCH. Detach restores the last host size.

A leftover same-uid socket (dead server) is diagnosed as stale; start
pmuxd to replace it. --create-session / --session implement
§5.6.1 step 8 on the control plane. --space NAME (or $PMUX_SPACE)
scopes C-\\ n / C-\\ p / C-\\ 1-9 to that space's sessions. Without a
space, those chords cycle every non-default session. C-\\ 1-9 is reserved
and is not forwarded to the child.
"
    );
}

fn diagnose_socket(path: &std::path::Path) -> Result<()> {
    match probe_socket_liveness(path) {
        SocketLiveness::Live => Ok(()),
        SocketLiveness::Missing => {
            let mut message = format!(
                "no control socket at {} — start pmuxd (or ./scripts/pmux-daemon.sh start)",
                path.display()
            );
            if let Some(miss) = diagnose_runtime_dir_miss_from_env(path) {
                message = format!("{message}\n{miss}");
            }
            bail!("{message}")
        }
        SocketLiveness::Stale => {
            let mut message = format!(
                "stale control socket at {} (no listener; leftover from a dead server). \
                 Start pmuxd to replace the same-uid leftover, or remove the file.",
                path.display()
            );
            if let Some(miss) = diagnose_runtime_dir_miss_from_env(path) {
                message = format!("{message}\n{miss}");
            }
            bail!("{message}")
        }
        SocketLiveness::Foreign => bail!(
            "refusing {} — not a same-uid Prismattyc control socket",
            path.display()
        ),
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_request_id: u64,
}

impl Client {
    fn connect(path: &PathBuf) -> Result<Self> {
        Self::connect_timeout(path, REQUEST_TIMEOUT)
    }

    fn connect_timeout(path: &PathBuf, read_timeout: Duration) -> Result<Self> {
        let stream =
            UnixStream::connect(path).with_context(|| format!("connect {}", path.display()))?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_request_id: 1,
        })
    }

    fn register(&mut self) -> Result<u64> {
        match self.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })? {
            ControlResponseData::ClientRegistered { client_id } => Ok(client_id),
            _ => bail!("server returned an unexpected registration response"),
        }
    }

    fn send(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("request ID space exhausted")?;
        let request = make(request_id);
        serde_json::to_writer(&mut self.writer, &request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(request_id)
    }

    fn read_response(&mut self) -> Result<ControlResponse> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("server closed the attach connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// Read until `request_id` arrives. Skip lower ids (orphaned replies
    /// after a timed-out `request`). Fail on a higher id. Bound the skips
    /// so a silent stream cannot spin (PT-286).
    fn read_matching(&mut self, request_id: u64) -> Result<ControlResponse> {
        let mut skipped = 0u32;
        loop {
            let response = self.read_response()?;
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => return Ok(response),
                ControlIdMatch::Stale => {
                    skipped = match next_stale_skip(skipped) {
                        Some(n) => n,
                        None => bail!(
                            "gave up after stale control responses before {request_id} (last was {})",
                            response.request_id
                        ),
                    };
                }
                ControlIdMatch::Ahead => bail!(
                    "response request ID {} is ahead of awaited {request_id}",
                    response.request_id
                ),
            }
        }
    }

    fn request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> Result<ControlResponseData> {
        let request_id = self.send(make)?;
        let response = self.read_matching(request_id)?;
        match response.body {
            ControlResponseBody::Ok { response } => Ok(response),
            ControlResponseBody::Error { error } => Err(anyhow::Error::new(error)),
        }
    }
}

#[cfg(test)]
static REQUEST_ERRS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Log line for a failed attach control request (PT-288).
/// `session` is `-` when snapshot has not named the session yet (startup
/// SwitchSession before `pane_session_name` runs).
fn request_err_line(
    op: &str,
    pane_id: u64,
    session: Option<&str>,
    err: &dyn std::fmt::Display,
) -> String {
    format!(
        "pmux-attach: {op} failed pane={pane_id} session={}: {err}",
        session.unwrap_or("-")
    )
}

/// Keep the `Ok` payload. Log `Err` and return `None` so callers do not
/// treat a timed-out request as success (PT-288).
fn keep_request_ok<T>(
    op: &str,
    pane_id: u64,
    session: Option<&str>,
    result: Result<T>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            let line = request_err_line(op, pane_id, session, &err);
            eprintln!("{line}");
            #[cfg(test)]
            if let Ok(mut sink) = REQUEST_ERRS.lock() {
                sink.push(line);
            }
            None
        }
    }
}

fn resize_request(
    request_id: u64,
    window_id: u64,
    size: LocalWinsize,
    client_id: u64,
    fit: bool,
    host: bool,
) -> ControlRequest {
    ControlRequest::Resize {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        cols: size.cols,
        rows: size.rows,
        cell_width_px: Some(size.cell_width_px),
        cell_height_px: Some(size.cell_height_px),
        client_id: Some(client_id),
        fit,
        host,
    }
}

/// Copy local TTY size into paint state. Does not mean the server got Resize.
fn apply_observed_winsize(
    last_size: &mut Option<LocalWinsize>,
    previous: &mut Option<PaintFrame>,
    scroll: &mut ScrollState,
    observed: Option<LocalWinsize>,
) {
    if observed == *last_size {
        return;
    }
    *last_size = observed;
    *previous = None;
    if let Some(size) = observed {
        scroll.rows = size.rows;
        scroll.cols = size.cols;
    }
}

fn resize_needs_send(last_sent: Option<LocalWinsize>, size: LocalWinsize) -> bool {
    last_sent != Some(size)
}

const SUBSCRIBE_TIMEOUT_MS: u32 = 2_000;
const SUBSCRIBE_READ_TIMEOUT: Duration = Duration::from_millis(7_000);
static READ_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

fn poll_fallback_requested() -> bool {
    matches!(
        std::env::var("PRISMATTYC_ATTACH_POLL").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn drain_probe_requested() -> bool {
    matches!(
        std::env::var("PRISMATTYC_ATTACH_DRAIN_PROBE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn snapshot_request(request_id: u64) -> ControlRequest {
    ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }
}

/// PT-286 box probe: orphan two Snapshot replies, then request one more.
/// Drain must skip the stale ids so this request succeeds. Two behind
/// matches the live failure (read 225525 while awaiting 225527).
fn run_drain_probe(client: &mut Client) -> Result<()> {
    client.send(snapshot_request)?;
    client.send(snapshot_request)?;
    let response = client.request(snapshot_request)?;
    let ControlResponseData::Snapshot { .. } = response else {
        bail!("drain-probe: unexpected response {response:?}");
    };
    println!("drain-probe recovered after 2 stale");
    Ok(())
}

fn bump_read_frame_count() {
    READ_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn flush_read_frame_count() {
    let Ok(path) = std::env::var("PRISMATTYC_ATTACH_READ_COUNT") else {
        return;
    };
    let n = READ_FRAME_COUNT.load(Ordering::Relaxed);
    let _ = std::fs::write(path, format!("{n}\n"));
}

fn probe_current_seq(client: &mut Client, client_id: u64, pane_id: u64) -> Result<u64> {
    // from_seq > current is CatchUp::Ahead. Probe with u64::MAX so the
    // server does not ship the ring; StaleSequence.current_sequence is current.
    match client.request(|request_id| ControlRequest::SubscribePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        from_seq: u64::MAX,
        timeout_ms: 0,
    }) {
        Ok(ControlResponseData::PaneSubscribe { through_seq, .. }) => Ok(through_seq),
        Ok(_) => bail!("unexpected subscribe probe"),
        Err(error) => {
            if let Some(seq) = error
                .downcast_ref::<ControlError>()
                .filter(|err| err.code == ControlErrorCode::StaleSequence)
                .and_then(|err| err.current_sequence)
            {
                Ok(seq)
            } else {
                Err(error)
            }
        }
    }
}

struct LogPaintWake {
    ready: UnixStream,
    #[cfg(windows)]
    native_wake: windows_terminal::SocketWake,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LogPaintWake {
    fn start(socket: PathBuf, pane_id: u64) -> Result<Self> {
        let (ready, signal) = UnixStream::pair()?;
        ready.set_nonblocking(true)?;
        #[cfg(windows)]
        let native_wake = windows_terminal::SocketWake::new(&ready)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let thread = thread::spawn(move || subscribe_paint_loop(socket, pane_id, signal, &stop_t));
        Ok(Self {
            #[cfg(windows)]
            native_wake,
            ready,
            stop,
            thread: Some(thread),
        })
    }

    /// `(events, peer_closed)`. `peer_closed` means the reader thread dropped
    /// its end — poll would otherwise HUP-spin.
    fn drain(&self) -> (bool, bool) {
        let mut buf = [0u8; 32];
        let mut events = false;
        let mut closed = false;
        loop {
            match (&self.ready).read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(_) => events = true,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break
                }
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        (events, closed)
    }

    fn is_dead(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Drop the handle without join: the thread is in a blocking
        // SubscribePane read (up to 7 s). Detach so C-\ d stays fast.
        let _ = self.thread.take();
    }
}

impl Drop for LogPaintWake {
    fn drop(&mut self) {
        self.stop();
    }
}

fn bump_wake(signal: &mut UnixStream) {
    let _ = signal.write_all(&[1]);
}

fn connect_registered(socket: &PathBuf) -> Result<(Client, u64)> {
    let mut client = Client::connect_timeout(socket, SUBSCRIBE_READ_TIMEOUT)?;
    let client_id = client.register()?;
    Ok((client, client_id))
}

fn subscribe_paint_loop(socket: PathBuf, pane_id: u64, mut signal: UnixStream, stop: &AtomicBool) {
    let (mut client, mut client_id) = match connect_registered(&socket) {
        Ok(pair) => pair,
        Err(_) => return,
    };
    // Never default to seq 0: that replays the retained ring. If the probe
    // cannot learn current_sequence, exit and let run_interactive poll.
    let mut from_seq = {
        let mut seq = None;
        for _ in 0..5 {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match probe_current_seq(&mut client, client_id, pane_id) {
                Ok(n) => {
                    seq = Some(n);
                    break;
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                    if let Ok((fresh, id)) = connect_registered(&socket) {
                        client = fresh;
                        client_id = id;
                    }
                }
            }
        }
        let Some(seq) = seq else {
            return;
        };
        seq
    };
    while !stop.load(Ordering::Acquire) {
        let request_id = match client.send(|request_id| ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            from_seq,
            timeout_ms: SUBSCRIBE_TIMEOUT_MS,
        }) {
            Ok(id) => id,
            Err(_) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
                match connect_registered(&socket) {
                    Ok((fresh, id)) => {
                        client = fresh;
                        client_id = id;
                    }
                    Err(_) => return,
                }
                continue;
            }
        };
        let mut skipped = 0u32;
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let response = match client.read_response() {
                Ok(response) => response,
                Err(error) => {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(seq) = error
                        .downcast_ref::<ControlError>()
                        .filter(|err| err.code == ControlErrorCode::StaleSequence)
                        .and_then(|err| err.current_sequence)
                    {
                        from_seq = seq;
                        bump_wake(&mut signal);
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                    match connect_registered(&socket) {
                        Ok((fresh, id)) => {
                            client = fresh;
                            client_id = id;
                        }
                        Err(_) => return,
                    }
                    break;
                }
            };
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => {}
                ControlIdMatch::Stale => {
                    skipped = match next_stale_skip(skipped) {
                        Some(n) => n,
                        None => break,
                    };
                    continue;
                }
                ControlIdMatch::Ahead => {
                    thread::sleep(Duration::from_millis(50));
                    match connect_registered(&socket) {
                        Ok((fresh, id)) => {
                            client = fresh;
                            client_id = id;
                        }
                        Err(_) => return,
                    }
                    break;
                }
            }
            match response.body {
                ControlResponseBody::Ok {
                    response:
                        ControlResponseData::PaneSubscribe {
                            through_seq,
                            events,
                            gap,
                            done,
                            ..
                        },
                } => {
                    from_seq = through_seq;
                    if gap || !events.is_empty() {
                        bump_wake(&mut signal);
                    }
                    if done {
                        break;
                    }
                }
                ControlResponseBody::Error { error }
                    if error.code == ControlErrorCode::StaleSequence =>
                {
                    if let Some(seq) = error.current_sequence {
                        from_seq = seq;
                    }
                    bump_wake(&mut signal);
                    break;
                }
                ControlResponseBody::Error { error } if error.code == ControlErrorCode::StaleId => {
                    return;
                }
                _ => return,
            }
        }
    }
}

fn first_pane(snapshot: &Snapshot) -> Option<u64> {
    snapshot
        .sessions
        .first()
        .and_then(|session| session.windows.first())
        .and_then(|window| window.panes.first())
        .map(|pane| pane.id)
}

fn session_pane(snapshot: &Snapshot, key: &str) -> Result<u64> {
    let matches: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.name == key || session.id.to_string() == key)
        .collect();
    match matches.as_slice() {
        [session] => session
            .windows
            .first()
            .and_then(|window| window.panes.first())
            .map(|pane| pane.id)
            .context("selected session has no pane"),
        [] => bail!("no session matching {key:?}"),
        _ => bail!("session name {key:?} is ambiguous; use --session-id"),
    }
}

fn session_pane_by_id(snapshot: &Snapshot, session_id: u64) -> Result<u64> {
    let matches: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.id == session_id)
        .collect();
    match matches.as_slice() {
        [session] => session
            .windows
            .first()
            .and_then(|window| window.panes.first())
            .map(|pane| pane.id)
            .context("selected session has no pane"),
        [] => bail!("no session with id {session_id}"),
        _ => bail!("session id {session_id} is not unique"),
    }
}

fn pane_window(snapshot: &Snapshot, pane_id: u64) -> Option<u64> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .find(|window| window.panes.iter().any(|pane| pane.id == pane_id))
            .map(|window| window.id)
    })
}

fn pane_controller_id(snapshot: &Snapshot, pane_id: u64) -> Option<u64> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .flat_map(|window| window.panes.iter())
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.controller_id)
    })
}

fn pane_guest_status(snapshot: &Snapshot, pane_id: u64) -> Option<String> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .flat_map(|window| window.panes.iter())
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| {
                // The chrome slot shows the guest status; a pane title
                // (PT-128) fills it when no status is set.
                pane.status
                    .clone()
                    .or_else(|| Some(pane.title.clone()).filter(|title| !title.is_empty()))
            })
    })
}

fn pane_sync_input(snapshot: &Snapshot, pane_id: u64) -> Option<bool> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .find(|window| window.panes.iter().any(|pane| pane.id == pane_id))
            .map(|window| window.sync_input)
    })
}

fn session_ended_line(pane_id: u64, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("session ended: {name} (pane {pane_id} child exited)"),
        None => format!("session ended (pane {pane_id} child exited)"),
    }
}

fn pane_session_name(snapshot: &Snapshot, pane_id: u64) -> Option<&str> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .any(|window| window.panes.iter().any(|pane| pane.id == pane_id))
            .then_some(session.name.as_str())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSwitch {
    Next,
    Prev,
    Jump(u8),
}

fn take_snapshot(client: &mut Client) -> Result<Snapshot> {
    let response = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    match response {
        ControlResponseData::Snapshot { snapshot } => Ok(snapshot),
        _ => bail!("server returned an unexpected snapshot response"),
    }
}

/// Sessions to cycle with C-\ n/p/1-9: space tab order when named,
/// else every non-default session in `pmux ls` order.
fn session_has_live_child(snapshot: &Snapshot, name: &str) -> bool {
    snapshot
        .sessions
        .iter()
        .find(|session| session.name == name)
        .is_some_and(|session| {
            session
                .windows
                .iter()
                .flat_map(|window| window.panes.iter())
                .any(|pane| pane.child_pid.is_some())
        })
}

fn space_lists_session(dir: &Path, name: &str, session: &str) -> bool {
    let Ok(space) = load_space(dir, name) else {
        return false;
    };
    space_sessions_in_tab_order(&space)
        .iter()
        .any(|listed| listed == session)
}

/// Disk-backed space name for attach chrome (PT-205).
///
/// `hint` (`--space` / `$PMUX_SPACE`) wins only when that file exists and
/// lists `session`. Otherwise the first `spaces/*.json` that lists it
/// (sorted by name) wins. None means no saved space names this session.
fn resolve_space_label(hint: Option<&str>, session: &str, dir: &Path) -> Option<String> {
    if let Some(hint) = hint.filter(|name| !name.is_empty()) {
        if space_lists_session(dir, hint, session) {
            return Some(hint.to_string());
        }
    }
    let Ok(entries) = list_spaces(dir) else {
        return None;
    };
    entries
        .into_iter()
        .find(|entry| space_lists_session(dir, &entry.name, session))
        .map(|entry| entry.name)
}

fn spaces_dir_stamp(dir: &Path) -> Option<SystemTime> {
    let mut latest = std::fs::metadata(dir).ok()?.modified().ok();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return latest;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(mtime) = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
        else {
            continue;
        };
        latest = Some(latest.map_or(mtime, |current| current.max(mtime)));
    }
    latest
}

/// Nested under `prismattyc-host` (`PRISMATTYC_HOST=1`). The rail and tab
/// title already name the space and session (PT-208).
fn under_host() -> bool {
    matches!(
        std::env::var("PRISMATTYC_HOST").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Identity overlay is space-attach chrome. A plain attach has no row.
/// A hinted attach whose file is gone may show `session X` (PT-205).
/// Under the host, the overlay is omitted (PT-208).
fn space_attach_identity(
    hinted: bool,
    under_host: bool,
    space: Option<&str>,
    session: Option<&str>,
    names: &[String],
) -> Option<String> {
    if under_host || !hinted {
        return None;
    }
    attach_identity_line(space, session, names)
}

fn cycle_session_names(space: Option<&str>, snapshot: &Snapshot) -> Vec<String> {
    if let Some(name) = space {
        return match load_space(&spaces_dir(), name) {
            Ok(saved) => space_sessions_in_tab_order(&saved)
                .into_iter()
                .filter(|session| session_has_live_child(snapshot, session))
                .collect(),
            Err(_) => Vec::new(),
        };
    }
    let non_default: Vec<String> = snapshot
        .sessions
        .iter()
        .filter(|session| session.name != "default")
        .filter(|session| session_has_live_child(snapshot, &session.name))
        .map(|session| session.name.clone())
        .collect();
    if !non_default.is_empty() {
        return non_default;
    }
    snapshot
        .sessions
        .iter()
        .filter(|session| session_has_live_child(snapshot, &session.name))
        .map(|session| session.name.clone())
        .collect()
}

fn attach_identity_line(
    space: Option<&str>,
    session: Option<&str>,
    names: &[String],
) -> Option<String> {
    let session = session?;
    if let Some(idx) = names.iter().position(|name| name == session) {
        let n = names.len();
        let i = idx + 1;
        return Some(match space {
            Some(space) => format!("space {space} · session {session} ({i}/{n})"),
            None => format!("session {session} ({i}/{n})"),
        });
    }
    Some(match space {
        Some(space) => format!("space {space} · session {session}"),
        None => format!("session {session}"),
    })
}

fn target_session_name(
    current: Option<&str>,
    names: &[String],
    switch: SessionSwitch,
) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let current_idx = current.and_then(|name| names.iter().position(|item| item == name));
    let idx = match switch {
        SessionSwitch::Next => current_idx.map(|i| (i + 1) % names.len()).unwrap_or(0),
        SessionSwitch::Prev => current_idx
            .map(|i| (i + names.len() - 1) % names.len())
            .unwrap_or(names.len() - 1),
        SessionSwitch::Jump(n) => {
            let i = usize::from(n);
            if i == 0 || i > names.len() {
                return None;
            }
            i - 1
        }
    };
    let name = names[idx].clone();
    if current == Some(name.as_str()) {
        return None;
    }
    Some(name)
}

fn attach_toast_text(session_name: Option<&str>) -> String {
    match session_name {
        Some(name) if !name.is_empty() => format!(" session {name} is attached "),
        _ => " session is attached ".to_string(),
    }
}

/// Top-right viewport HUD. Fill is the focus-border RGB; ink/dark contrast text.
fn attach_toast_overlay(text: &str, cols: u32) -> PaneOverlay {
    let width = u16::try_from(text.chars().count())
        .unwrap_or(u16::MAX)
        .max(1);
    let col = u16::try_from(cols.saturating_sub(u32::from(width))).unwrap_or(0);
    PaneOverlay {
        id: u32::MAX,
        kind: OverlayKind::Viewport,
        row: 0,
        col,
        rows: 1,
        cols: width,
        text: text.to_string(),
        runs: Vec::new(),
    }
}

fn parse_focus_border_rgb(spec: &str) -> Option<[u8; 3]> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Ok(n) = spec.parse::<usize>() {
        if let Some(rgb) = SPECTRUM.get(n) {
            return Some(*rgb);
        }
        if n == SPECTRUM.len() {
            return Some(INK);
        }
        return None;
    }
    match spec.to_ascii_lowercase().as_str() {
        "coral" => Some(SPECTRUM[0]),
        "amber" => Some(SPECTRUM[1]),
        "yellow" => Some(SPECTRUM[2]),
        "green" => Some(SPECTRUM[3]),
        "blue" => Some(SPECTRUM[4]),
        "indigo" => Some(SPECTRUM[5]),
        "violet" => Some(SPECTRUM[6]),
        "ink" => Some(INK),
        _ => None,
    }
}

fn config_focus_border_spec() -> Option<String> {
    let raw = std::fs::read_to_string(prism_config_path()).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    match parsed.get("focus_border")? {
        toml::Value::String(text) => Some(text.clone()),
        toml::Value::Integer(n) if *n >= 0 => Some(n.to_string()),
        _ => None,
    }
}

fn toast_focus_border_rgb() -> [u8; 3] {
    if let Ok(spec) = std::env::var("PRISMATTYC_FOCUS_BORDER") {
        if let Some(rgb) = parse_focus_border_rgb(&spec) {
            return rgb;
        }
    }
    if cfg!(test) {
        return DEFAULT_TOAST_FOCUS_RGB;
    }
    config_focus_border_spec()
        .and_then(|spec| parse_focus_border_rgb(&spec))
        .unwrap_or(DEFAULT_TOAST_FOCUS_RGB)
}

fn toast_contrast_fg(bg: [u8; 3]) -> [u8; 3] {
    let luma = u32::from(bg[0]) * 299 + u32::from(bg[1]) * 587 + u32::from(bg[2]) * 114;
    if luma >= 140_000 {
        [0x18, 0x18, 0x1c]
    } else {
        INK
    }
}

fn default_create_spawn() -> SpawnSpec {
    let mut command = prismattyc_mux::platform::default_shell_command();
    let program = command.remove(0);
    SpawnSpec {
        program,
        argv: command,
        cwd: std::env::current_dir().ok(),
        env: Default::default(),
    }
}

fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Raw interactive attach needs both a TTY stdin (keys) and a TTY stdout
/// (paint). Piped stdout dumps JSON instead of emitting CSI.
fn attach_wants_interactive(
    json: bool,
    styled_json: bool,
    stdin_tty: bool,
    stdout_tty: bool,
) -> bool {
    !json && !styled_json && stdin_tty && stdout_tty
}

fn attach_is_dump_or_write(cli: &Cli) -> bool {
    cli.json
        || cli.styled_json
        || cli.watch
        || cli.write.is_some()
        || cli.pane.is_some()
        || cli.read_only
        || cli.fit
}

fn should_host_route_cli(cli: &Cli) -> bool {
    should_host_route_seat(
        under_host(),
        attach_pty_fallback(),
        attach_is_dump_or_write(cli),
    )
}

fn pane_session_id(snapshot: &Snapshot, pane_id: u64) -> Option<u64> {
    snapshot.sessions.iter().find_map(|session| {
        session
            .windows
            .iter()
            .any(|window| window.panes.iter().any(|pane| pane.id == pane_id))
            .then_some(session.id)
    })
}

/// Session to host-route after resolve, or `None` to keep nested attach.
fn host_route_session_after_resolve(should_route: bool, session_id: Option<u64>) -> Option<u64> {
    if !should_route {
        return None;
    }
    session_id
}

fn try_host_route_after_resolve(cli: &Cli, snapshot: &Snapshot, pane_id: u64) -> Result<bool> {
    let Some(session_id) = host_route_session_after_resolve(
        should_host_route_cli(cli),
        pane_session_id(snapshot, pane_id),
    ) else {
        return Ok(false);
    };
    let name = pane_session_name(snapshot, pane_id).unwrap_or("session");
    if !route_seat_to_host(&cli.socket, &session_id.to_string(), name)? {
        return Ok(false);
    }
    let ack = host_ack_path_from_socket(&cli.socket);
    let cache = attach_tabs::layout_path_from_socket(&cli.socket);
    let since = std::fs::metadata(&cache)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| SystemTime::now());
    let _ = wait_host_ack(&ack, since, Duration::from_secs(2));
    eprintln!("attached {name} in the host as a log replica");
    Ok(true)
}

fn control_code(err: &anyhow::Error) -> Option<ControlErrorCode> {
    err.downcast_ref::<ControlError>().map(|error| error.code)
}

fn read_pane(client: &mut Client, client_id: u64, pane_id: u64) -> Result<PaneContent> {
    let response = client.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    })?;
    let ControlResponseData::PaneContent { content } = response else {
        bail!("server returned an unexpected pane-content response");
    };
    Ok(content)
}

fn read_frame(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    view_offset: Option<u32>,
) -> Result<PaintFrame> {
    bump_read_frame_count();
    match client.request(|request_id| ControlRequest::ReadPaneStyled {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        view_offset,
    }) {
        Ok(ControlResponseData::PaneStyled { content }) => Ok(PaintFrame::from_styled(content)),
        Ok(_) | Err(_) => Ok(PaintFrame::from_plain(read_pane(
            client, client_id, pane_id,
        )?)),
    }
}

enum LeaseAcquire {
    Acquired,
    Held { holder: u64 },
}

fn try_acquire_lease(client: &mut Client, client_id: u64, pane_id: u64) -> Result<LeaseAcquire> {
    match client.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    }) {
        Ok(_) => Ok(LeaseAcquire::Acquired),
        Err(error) if control_code(&error) == Some(ControlErrorCode::LeaseHeld) => {
            let holder = error
                .downcast_ref::<ControlError>()
                .and_then(|err| err.holder)
                .unwrap_or(0);
            Ok(LeaseAcquire::Held { holder })
        }
        Err(error) => Err(error),
    }
}

fn write_pane(client: &mut Client, client_id: u64, pane_id: u64, data: String) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    loop {
        match client.request(|request_id| ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            data: data.clone(),
        }) {
            Ok(_) => return Ok(()),
            Err(error) if control_code(&error) == Some(ControlErrorCode::Backpressure) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Interactive attach must survive `WritePane`/`RichInput` `NotController`.
/// A `pmux send --force` takeover is the usual cause. Drop the stale
/// controller flag, paint a chip, and keep the TTY session up.
fn absorb_not_controller(
    error: &anyhow::Error,
    controller: &mut bool,
    lease_held: &mut bool,
    pending_utf8: &mut Vec<u8>,
    held_notice: &mut Option<String>,
) -> bool {
    if control_code(error) != Some(ControlErrorCode::NotController) {
        return false;
    }
    *controller = false;
    let holder = error.downcast_ref::<ControlError>().and_then(|e| e.holder);
    *lease_held = holder.is_some();
    pending_utf8.clear();
    *held_notice = Some(match holder {
        Some(id) => format!("input held by client {id} — wait or C-\\ d"),
        None => "lease lost — wait or C-\\ d".into(),
    });
    true
}

fn take_utf8_prefix(buf: &mut Vec<u8>) -> Option<String> {
    match std::str::from_utf8(buf) {
        Ok(text) => {
            let out = text.to_string();
            buf.clear();
            (!out.is_empty()).then_some(out)
        }
        Err(error) => {
            let valid = error.valid_up_to();
            if valid > 0 {
                let out = String::from_utf8(buf.drain(..valid).collect())
                    .expect("prefix marked valid UTF-8");
                Some(out)
            } else if error.error_len().is_some() {
                buf.remove(0);
                None
            } else {
                None
            }
        }
    }
}

struct RawTerminal {
    #[cfg(unix)]
    original: Termios,
    #[cfg(windows)]
    original: windows_terminal::RawConsole,
}

impl RawTerminal {
    #[cfg(windows)]
    fn enter() -> Result<Self> {
        Ok(Self {
            original: windows_terminal::RawConsole::enter()?,
        })
    }
    #[cfg(unix)]
    fn enter() -> Result<Self> {
        let stdin = io::stdin();
        let original = termios::tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw).context("tcsetattr raw")?;
        Ok(Self { original })
    }

    fn restore(&self) {
        #[cfg(unix)]
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.original);
        let mut out = io::stdout();
        let _ = out.write_all(
            b"\x1b[?7700l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?7h\x1b[?1049l",
        );
        let _ = out.flush();
        #[cfg(windows)]
        self.original.restore();
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// `None` means the size is unknown (ioctl error or a 0×N / N×0 / 0×0 PTY).
/// Callers must not `Resize` on `None` — leave the server window alone.
fn normalize_winsize(cols: u16, rows: u16) -> Option<(u32, u32)> {
    if cols == 0 || rows == 0 {
        None
    } else {
        Some((u32::from(cols).max(2), u32::from(rows).max(1)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalWinsize {
    cols: u32,
    rows: u32,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[cfg(any(unix, test))]
fn cell_px_from_window(window_px: u16, cells: u32) -> u32 {
    if window_px == 0 || cells == 0 {
        return 0;
    }
    (u32::from(window_px) / cells).max(1)
}

#[cfg(test)]
thread_local! {
    static INJECTED_WINSIZE: std::cell::Cell<Option<Option<LocalWinsize>>> =
        const { std::cell::Cell::new(None) };
}

/// Test seam for [`local_winsize`]. Drop restores ioctl. `set(None)` is a
/// missing TTY; `set(Some(size))` is a forced size.
#[cfg(test)]
struct InjectedWinsize;

#[cfg(test)]
impl InjectedWinsize {
    fn set(size: Option<LocalWinsize>) -> Self {
        INJECTED_WINSIZE.with(|cell| cell.set(Some(size)));
        Self
    }
}

#[cfg(test)]
impl Drop for InjectedWinsize {
    fn drop(&mut self) {
        INJECTED_WINSIZE.with(|cell| cell.set(None));
    }
}

#[cfg(unix)]
fn local_winsize() -> Option<LocalWinsize> {
    #[cfg(test)]
    if let Some(forced) = INJECTED_WINSIZE.with(|cell| cell.get()) {
        return forced;
    }
    let size = termios::tcgetwinsize(io::stdin()).ok()?;
    let (cols, rows) = normalize_winsize(size.ws_col, size.ws_row)?;
    let cell_width_px = cell_px_from_window(size.ws_xpixel, cols);
    let cell_height_px = cell_px_from_window(size.ws_ypixel, rows);
    Some(LocalWinsize {
        cols,
        rows,
        cell_width_px: if cell_width_px == 0 {
            prismattyc_emulator::NOMINAL_CELL_W_PX
        } else {
            cell_width_px
        },
        cell_height_px: if cell_height_px == 0 {
            prismattyc_emulator::NOMINAL_CELL_H_PX
        } else {
            cell_height_px
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaintFrame {
    content: PaneContent,
    runs: Vec<Vec<StyleRun>>,
    view_offset: Option<u32>,
    max_view_scroll: Option<u32>,
    child_mouse_tracking: Option<bool>,
    child_mouse_sgr: Option<bool>,
    overlays: Vec<PaneOverlay>,
    experimental_rich: bool,
    rich_focus_id: Option<u32>,
    structured_focus: bool,
    workspace_rows: u32,
    semantic_clipboard: Option<String>,
    semantic_clipboard_seq: Option<u64>,
}

impl PaintFrame {
    fn from_styled(styled: PaneStyled) -> Self {
        let guest_runs = if styled.runs.len() == styled.content.lines.len() {
            styled.runs
        } else {
            plain_runs(&styled.content.lines)
        };
        let workspace_rows = u32::try_from(styled.workspace.len()).unwrap_or(u32::MAX);
        let mut runs = selected_workspace_runs(
            &styled.workspace,
            &styled.workspace_inverse,
            &styled.workspace_styles,
        );
        runs.extend(guest_runs);
        let mut content = styled.content;
        let mut lines = styled.workspace;
        lines.append(&mut content.lines);
        content.lines = lines;
        content.rows = content.rows.saturating_add(workspace_rows);
        content.cursor_row = content.cursor_row.saturating_add(workspace_rows);
        Self {
            content,
            view_offset: styled.view_offset,
            max_view_scroll: styled.max_view_scroll,
            child_mouse_tracking: styled.child_mouse_tracking,
            child_mouse_sgr: styled.child_mouse_sgr,
            overlays: styled.overlays,
            experimental_rich: styled.experimental_rich,
            rich_focus_id: styled.rich_focus_id,
            structured_focus: styled.structured_focus,
            workspace_rows,
            runs,
            semantic_clipboard: styled.semantic_clipboard,
            semantic_clipboard_seq: styled.semantic_clipboard_seq,
        }
    }

    fn from_plain(content: PaneContent) -> Self {
        let runs = plain_runs(&content.lines);
        Self {
            content,
            runs,
            view_offset: None,
            max_view_scroll: None,
            child_mouse_tracking: None,
            child_mouse_sgr: None,
            overlays: Vec::new(),
            experimental_rich: false,
            rich_focus_id: None,
            structured_focus: false,
            workspace_rows: 0,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
        }
    }
}

/// Origin so `cursor` stays inside a `term`-row window of `pane` rows.
fn viewport_origin(pane: u32, term: u32, cursor: u32) -> u32 {
    if pane <= term {
        0
    } else {
        let max_origin = pane.saturating_sub(term);
        cursor
            .saturating_sub(term.saturating_sub(1))
            .min(max_origin)
    }
}

fn clip_display_cols(line: &str, origin_col: u32, term_cols: u32) -> String {
    let mut out = String::new();
    let origin = origin_col as usize;
    let end = origin.saturating_add(term_cols as usize);
    for_each_display_scalar(line, |col, ch, width| {
        if width == 0 {
            if col >= origin && col < end {
                out.push(ch);
            }
            return;
        }
        if col + width <= origin || col >= end {
            return;
        }
        if col >= origin {
            out.push(ch);
        }
    });
    out
}

fn clip_style_runs(runs: &[StyleRun], origin_col: u32, term_cols: u32) -> Vec<StyleRun> {
    let origin = origin_col as usize;
    let end = origin.saturating_add(term_cols as usize);
    let mut out = Vec::new();
    let mut abs = 0usize;
    for run in runs {
        let mut text = String::new();
        for_each_display_scalar(&run.text, |_, ch, width| {
            let start = abs;
            if width == 0 {
                if start >= origin && start < end {
                    text.push(ch);
                }
                return;
            }
            abs = abs.saturating_add(width);
            if start + width <= origin || start >= end {
                return;
            }
            if start >= origin {
                text.push(ch);
            }
        });
        if !text.is_empty() {
            let mut clipped = run.clone();
            clipped.text = text;
            out.push(clipped);
        }
    }
    if out.is_empty() {
        out.push(StyleRun::plain(String::new()));
    }
    out
}

/// Viewport overlays stay in visible-terminal coordinates.
/// CellRect overlays follow pane panning. Drop a CellRect that sits fully
/// left of the origin; trim one that straddles the left edge.
fn clip_overlay_into_viewport(
    mut overlay: PaneOverlay,
    origin_row: u32,
    origin_col: u32,
    view_rows: u32,
    view_cols: u32,
) -> Option<PaneOverlay> {
    match overlay.kind {
        OverlayKind::Viewport => Some(overlay),
        OverlayKind::CellRect => {
            overlay.row -= i32::try_from(origin_row).unwrap_or(i32::MAX);
            let view_rows_i = i32::try_from(view_rows).unwrap_or(i32::MAX);
            if overlay.row >= view_rows_i {
                return None;
            }
            if overlay.row.saturating_add(i32::from(overlay.rows)) <= 0 {
                return None;
            }
            let start = u32::from(overlay.col);
            let end = start.saturating_add(u32::from(overlay.cols));
            if end <= origin_col || start >= origin_col.saturating_add(view_cols) {
                return None;
            }
            if start < origin_col {
                skip_overlay_cols(&mut overlay, origin_col - start);
                overlay.col = 0;
            } else {
                overlay.col = u16::try_from(start - origin_col).unwrap_or(u16::MAX);
            }
            if overlay.cols == 0 {
                return None;
            }
            Some(overlay)
        }
    }
}

fn skip_overlay_cols(overlay: &mut PaneOverlay, skip: u32) {
    let skip_u = u16::try_from(skip).unwrap_or(u16::MAX);
    overlay.cols = overlay.cols.saturating_sub(skip_u);
    let skip_n = skip as usize;
    if !overlay.text.is_empty() {
        overlay.text = overlay.text.chars().skip(skip_n).collect();
    }
    if overlay.runs.is_empty() {
        return;
    }
    let mut remaining = skip_n;
    let mut kept = Vec::new();
    for run in overlay.runs.drain(..) {
        let n = run.text.chars().count();
        if remaining >= n {
            remaining -= n;
            continue;
        }
        let text: String = run.text.chars().skip(remaining).collect();
        remaining = 0;
        kept.push(OverlayRun { text, ..run });
    }
    overlay.runs = kept;
}

/// Clip a pane frame to a terminal. Scroll so the cursor cell stays visible.
/// Cursor is rewritten to the on-screen cell and never past the last row/col.
fn clip_frame_to_terminal(mut frame: PaintFrame, term_cols: u32, term_rows: u32) -> PaintFrame {
    let pane_cols = frame.content.cols.max(1);
    let pane_rows = frame.content.rows.max(1);
    let term_cols = term_cols.max(1);
    let term_rows = term_rows.max(1);
    let origin_row = viewport_origin(pane_rows, term_rows, frame.content.cursor_row);
    let origin_col = viewport_origin(pane_cols, term_cols, frame.content.cursor_col);
    let view_rows = pane_rows.min(term_rows);
    let view_cols = pane_cols.min(term_cols);
    if origin_row == 0 && origin_col == 0 && view_rows == pane_rows && view_cols == pane_cols {
        return frame;
    }
    let start = origin_row as usize;
    let end = start.saturating_add(view_rows as usize);
    if start < frame.content.lines.len() {
        frame.content.lines = frame.content.lines[start..end.min(frame.content.lines.len())]
            .iter()
            .map(|line| clip_display_cols(line, origin_col, view_cols))
            .collect();
    } else {
        frame.content.lines.clear();
    }
    if start < frame.runs.len() {
        frame.runs = frame.runs[start..end.min(frame.runs.len())]
            .iter()
            .map(|runs| clip_style_runs(runs, origin_col, view_cols))
            .collect();
    } else {
        frame.runs.clear();
    }
    frame.overlays = frame
        .overlays
        .into_iter()
        .filter_map(|overlay| {
            clip_overlay_into_viewport(overlay, origin_row, origin_col, view_rows, view_cols)
        })
        .collect();
    while frame.content.lines.len() < view_rows as usize {
        frame.content.lines.push(String::new());
        frame.runs.push(vec![StyleRun::plain(String::new())]);
    }
    frame.content.cols = view_cols;
    frame.content.rows = view_rows;
    frame.content.cursor_row = frame
        .content
        .cursor_row
        .saturating_sub(origin_row)
        .min(view_rows.saturating_sub(1));
    frame.content.cursor_col = frame
        .content
        .cursor_col
        .saturating_sub(origin_col)
        .min(view_cols.saturating_sub(1));
    if origin_row < frame.workspace_rows {
        frame.workspace_rows = frame.workspace_rows.saturating_sub(origin_row);
    } else {
        frame.workspace_rows = 0;
    }
    frame
}

fn viewport_hint(pane_cols: u32, pane_rows: u32, term_cols: u32, term_rows: u32) -> Option<String> {
    if pane_cols > term_cols || pane_rows > term_rows {
        Some(format!(
            "pane {pane_cols}×{pane_rows} > {term_cols}×{term_rows} · C-\\ z fit"
        ))
    } else {
        None
    }
}

fn plain_runs(lines: &[String]) -> Vec<Vec<StyleRun>> {
    lines
        .iter()
        .map(|line| vec![StyleRun::plain(line.clone())])
        .collect()
}

fn selected_workspace_runs(
    lines: &[String],
    inverse: &[WorkspaceInverseRun],
    styles: &[WorkspaceStyleRun],
) -> Vec<Vec<StyleRun>> {
    lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let mut runs = Vec::new();
            let mut current = String::new();
            let mut current_inverse = false;
            let mut current_fg = ColorWire::Default;
            for_each_display_scalar(line, |col, ch, width| {
                let col = u32::try_from(col).unwrap_or(u32::MAX);
                let inverse = if width == 0 {
                    current_inverse
                } else {
                    inverse.iter().any(|run| {
                        run.row == row as u32
                            && col >= run.col
                            && col < run.col.saturating_add(run.cols)
                    })
                };
                let fg = if width == 0 {
                    current_fg
                } else {
                    styles
                        .iter()
                        .find(|run| {
                            run.row == row as u32
                                && col >= run.col
                                && col < run.col.saturating_add(run.cols)
                        })
                        .map(|run| run.fg)
                        .unwrap_or_default()
                };
                if !current.is_empty() && (inverse != current_inverse || fg != current_fg) {
                    let mut style = StyleRun::plain(std::mem::take(&mut current));
                    style.inverse = current_inverse;
                    style.fg = current_fg;
                    runs.push(style);
                }
                current_inverse = inverse;
                current_fg = fg;
                current.push(ch);
            });
            if !current.is_empty() {
                let mut style = StyleRun::plain(current);
                style.inverse = current_inverse;
                style.fg = current_fg;
                runs.push(style);
            }
            if runs.is_empty() {
                runs.push(StyleRun::plain(String::new()));
            }
            runs
        })
        .collect()
}

/// `None` means a full repaint (row count changed).
fn dirty_line_indices<T: PartialEq>(prev: &[T], next: &[T]) -> Option<Vec<usize>> {
    if prev.len() != next.len() {
        return None;
    }
    Some(
        prev.iter()
            .zip(next)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect(),
    )
}

fn write_color(out: &mut Vec<u8>, color: ColorWire, foreground: bool) {
    match color {
        ColorWire::Default => {}
        ColorWire::Ansi { n } => {
            let code = match (foreground, n < 8) {
                (true, true) => 30 + n,
                (true, false) => 90 + n.saturating_sub(8),
                (false, true) => 40 + n,
                (false, false) => 100 + n.saturating_sub(8),
            };
            let _ = write!(out, "\x1b[{code}m");
        }
        ColorWire::Indexed { n } => {
            let ground = if foreground { 38 } else { 48 };
            let _ = write!(out, "\x1b[{ground};5;{n}m");
        }
        ColorWire::Rgb { r, g, b } => {
            let ground = if foreground { 38 } else { 48 };
            let _ = write!(out, "\x1b[{ground};2;{r};{g};{b}m");
        }
    }
}

fn write_runs(out: &mut Vec<u8>, runs: &[StyleRun]) {
    for run in runs {
        out.extend_from_slice(b"\x1b[0m");
        if run.bold {
            out.extend_from_slice(b"\x1b[1m");
        }
        if run.italic {
            out.extend_from_slice(b"\x1b[3m");
        }
        if run.underline {
            out.extend_from_slice(b"\x1b[4m");
        }
        if run.inverse {
            out.extend_from_slice(b"\x1b[7m");
        }
        write_color(out, run.fg, true);
        write_color(out, run.bg, false);
        out.extend_from_slice(run.text.as_bytes());
    }
    out.extend_from_slice(b"\x1b[0m");
}

fn line_display_cells(line: &str, cols: u32) -> Vec<String> {
    let mut cells = vec![String::from(" "); usize::try_from(cols).unwrap_or(0)];
    for_each_display_scalar(line, |col, ch, width| {
        if col >= cells.len() {
            return;
        }
        if width == 0 {
            cells[col].push(ch);
        } else {
            cells[col].clear();
            cells[col].push(ch);
            if width > 1 && col.saturating_add(1) < cells.len() {
                cells[col + 1].clear();
            }
        }
    });
    cells
}

fn ordered_copy_points(a: CopyPoint, b: CopyPoint) -> (CopyPoint, CopyPoint) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn copy_cell_selected(scroll: &ScrollState, row: u32, col: u32) -> bool {
    let Some(anchor) = scroll.copy_anchor else {
        return false;
    };
    let (start, end) = ordered_copy_points(
        scroll.copy_view_point(anchor),
        scroll.copy_view_point(scroll.copy_cursor),
    );
    (row > start.row || (row == start.row && col >= start.col))
        && (row < end.row || (row == end.row && col <= end.col))
}

fn copy_selection_text(frame: &PaintFrame, scroll: &ScrollState) -> String {
    let cols = frame.content.cols;
    let rows = frame.content.rows;
    let cursor = scroll.copy_view_point(scroll.copy_cursor);
    let (start, end) = scroll
        .copy_anchor
        .map(|anchor| {
            ordered_copy_points(
                scroll.copy_view_point(anchor),
                scroll.copy_view_point(scroll.copy_cursor),
            )
        })
        .unwrap_or((
            CopyPoint {
                row: cursor.row,
                col: 0,
            },
            CopyPoint {
                row: cursor.row,
                col: cols.saturating_sub(1),
            },
        ));
    let start_row = start.row.min(rows.saturating_sub(1));
    let end_row = end.row.min(rows.saturating_sub(1));
    let mut lines = Vec::new();
    for row in start_row..=end_row {
        let cells = frame
            .content
            .lines
            .get(row as usize)
            .map(|line| line_display_cells(line, cols))
            .unwrap_or_else(|| vec![String::from(" "); usize::try_from(cols).unwrap_or(0)]);
        let first = if row == start_row { start.col } else { 0 };
        let last = if row == end_row {
            end.col.min(cols.saturating_sub(1))
        } else {
            cols.saturating_sub(1)
        };
        if first <= last {
            let text = cells
                .get(first as usize..=last as usize)
                .map(|cells| cells.concat())
                .unwrap_or_default();
            lines.push(text.trim_end_matches(' ').to_string());
        } else {
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn copy_search_matches(scroll: &ScrollState) -> Vec<CopyMatch> {
    let query = scroll.search.needle();
    if query.is_empty() {
        return Vec::new();
    }
    let cols = scroll.cols;
    let last_row = scroll.copy_last_row();
    let offset = scroll.offset;
    let mut out = Vec::new();
    for (view_row, line) in scroll.search.lines.iter().enumerate() {
        let view_row = view_row as u32;
        if view_row > last_row {
            break;
        }
        let hist_row = last_row.saturating_add(offset).saturating_sub(view_row);
        for (byte_start, _) in line.match_indices(query) {
            let byte_end = byte_start.saturating_add(query.len());
            if !line.is_char_boundary(byte_end) {
                continue;
            }
            let col = u32::try_from(line_display_width(&line[..byte_start])).unwrap_or(u32::MAX);
            if col >= cols {
                continue;
            }
            let end = u32::try_from(line_display_width(&line[..byte_end]))
                .unwrap_or(u32::MAX)
                .min(cols);
            let len = end.saturating_sub(col).max(1);
            out.push(CopyMatch {
                point: CopyPoint { row: hist_row, col },
                len,
            });
        }
    }
    out
}

fn copy_search_rank(scroll: &ScrollState) -> Option<(usize, usize)> {
    if scroll.search.needle().is_empty() {
        return None;
    }
    let matches = copy_search_matches(scroll);
    let n = matches.len();
    if n == 0 {
        return None;
    }
    let i = scroll
        .search
        .current
        .and_then(|point| matches.iter().position(|m| m.point == point));
    Some((i.map(|i| i.saturating_add(1)).unwrap_or(0), n))
}

fn append_copy_search(out: &mut Vec<u8>, frame: &PaintFrame, scroll: &ScrollState) {
    if !scroll.copy_mode || scroll.search.needle().is_empty() {
        return;
    }
    let matches = copy_search_matches(scroll);
    let cols = frame.content.cols;
    let visible_rows = frame.content.rows.saturating_sub(1);
    for hit in &matches {
        let view = scroll.copy_view_point(hit.point);
        if view.row >= visible_rows {
            continue;
        }
        let cells = frame
            .content
            .lines
            .get(view.row as usize)
            .map(|line| line_display_cells(line, cols))
            .unwrap_or_else(|| vec![String::from(" "); usize::try_from(cols).unwrap_or(0)]);
        let inverse = scroll.search.current == Some(hit.point);
        for dcol in 0..hit.len {
            let col = view.col.saturating_add(dcol);
            if col >= cols {
                break;
            }
            if scroll.copy_selecting && copy_cell_selected(scroll, view.row, col) {
                continue;
            }
            let text = cells.get(col as usize).map(String::as_str).unwrap_or(" ");
            if inverse {
                let _ = write!(out, "\x1b[{};{}H\x1b[7m", view.row + 1, col + 1);
            } else {
                let _ = write!(out, "\x1b[{};{}H\x1b[4m", view.row + 1, col + 1);
            }
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    }
}

fn append_copy_selection(out: &mut Vec<u8>, frame: &PaintFrame, scroll: &ScrollState) {
    if !scroll.copy_mode || !scroll.copy_selecting || scroll.copy_anchor.is_none() {
        return;
    }
    let cols = frame.content.cols;
    let rows = frame.content.rows;
    let visible_rows = rows.saturating_sub(1);
    for row in 0..visible_rows {
        let cells = frame
            .content
            .lines
            .get(row as usize)
            .map(|line| line_display_cells(line, cols))
            .unwrap_or_else(|| vec![String::from(" "); usize::try_from(cols).unwrap_or(0)]);
        for col in 0..cols {
            if !copy_cell_selected(scroll, row, col) {
                continue;
            }
            let text = cells.get(col as usize).map(String::as_str).unwrap_or(" ");
            let _ = write!(out, "\x1b[{};{}H\x1b[7m", row + 1, col + 1);
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    }
}

fn paint_bytes(prev: Option<&PaintFrame>, next: &PaintFrame, clipboard: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(text) = clipboard {
        if let Some(osc) = prismattyc_core::encode_osc52_clipboard(text) {
            out.extend_from_slice(&osc);
        }
    }
    if let Some(text) = next.semantic_clipboard.as_deref() {
        if let Some(osc) = prismattyc_core::encode_osc52_clipboard(text) {
            out.extend_from_slice(&osc);
        }
    }
    out.extend_from_slice(b"\x1b[?25l");
    let dirty = prev.and_then(|p| dirty_line_indices(&p.runs, &next.runs));
    match dirty {
        None => {
            // DECAWM off: a full-width row must not wrap on the outer
            // terminal. Restored with the alt screen on detach.
            // EL first: after writing `cols` cells the cursor sits on
            // the last column, so a trailing EL would erase it.
            out.extend_from_slice(b"\x1b[?1049h\x1b[?7l\x1b[H");
            for (index, runs) in next.runs.iter().enumerate() {
                out.extend_from_slice(b"\x1b[K");
                write_runs(&mut out, runs);
                if index + 1 < next.runs.len() {
                    out.extend_from_slice(b"\r\n");
                }
            }
        }
        Some(rows) => {
            for index in rows {
                let _ = write!(out, "\x1b[{};1H", index + 1);
                out.extend_from_slice(b"\x1b[K");
                write_runs(&mut out, &next.runs[index]);
            }
        }
    }
    append_caret(&mut out, next);
    out
}

fn truncate_cols(text: &str, max_cols: usize) -> String {
    text.chars().take(max_cols).collect()
}

fn fit_status_line(prefix: &str, status: Option<&str>, cols: usize) -> String {
    let mut line = prefix.to_string();
    if let Some(status) = status {
        if line.is_empty() {
            line = status.to_string();
        } else {
            line.push_str(" │ ");
            line.push_str(status);
        }
    }
    truncate_cols(line.trim_start(), cols)
}

#[derive(Clone, Copy, Default)]
struct AttachChrome<'a> {
    sync_input: bool,
    lease_held: bool,
    read_only: bool,
    pane_status: Option<&'a str>,
    viewport_hint: Option<&'a str>,
    identity: Option<&'a str>,
    /// MailAttention letter occupies row 1 col 1; identity starts after it.
    mail_letter: bool,
    /// Nested under `prismattyc-host`. Scroll chrome matches the host
    /// inverse chip and right-edge bar (PT-306 fallback).
    host_nested: bool,
}

impl AttachChrome<'_> {
    fn chips(self) -> Vec<&'static str> {
        let mut chips = Vec::new();
        if self.read_only {
            chips.push("[ro]");
        }
        if self.lease_held {
            chips.push("[held]");
        }
        if self.sync_input {
            chips.push("[sync]");
        }
        chips
    }

    fn chip_suffix(self) -> String {
        let mut out = String::new();
        for chip in self.chips() {
            out.push(' ');
            out.push_str(chip);
        }
        out
    }
}

/// Live-mode attach chrome. Right-aligns guest status and inverse chips
/// (`[ro]`, `[held]`, `[sync]`) on row 1.
fn append_live_chrome(out: &mut Vec<u8>, cols: u32, chrome: AttachChrome<'_>) {
    if let Some(hint) = chrome.viewport_hint.filter(|text| !text.is_empty()) {
        out.extend_from_slice(b"\x1b[?25l");
        let shown = truncate_cols(hint, cols as usize);
        let _ = write!(out, "\x1b[1;1H\x1b[0m{shown}");
    } else if let Some(identity) = chrome.identity.filter(|text| !text.is_empty()) {
        // Row 1 col 1 is the MailAttention letter. Write a space at col 2
        // so the glyph does not touch the identity text (PT-209).
        let (start_col, gap) = if chrome.mail_letter {
            (MAIL_CELL_COLS as u32 + 1, " ")
        } else {
            (1, "")
        };
        let budget = (cols as usize)
            .saturating_sub(start_col.saturating_sub(1) as usize)
            .saturating_sub(gap.len());
        out.extend_from_slice(b"\x1b[?25l");
        let shown = truncate_cols(identity, budget);
        let _ = write!(out, "\x1b[1;{start_col}H\x1b[0m{gap}{shown}\x1b[K");
    }
    let status = chrome
        .pane_status
        .filter(|text| !text.is_empty())
        .unwrap_or("");
    let chips = chrome.chips();
    if status.is_empty() && chips.is_empty() {
        return;
    }
    let chip_cols: usize = chips.iter().map(|chip| chip.len()).sum::<usize>()
        + chips.len().saturating_sub(1)
        + if !status.is_empty() && !chips.is_empty() {
            1
        } else {
            0
        };
    let budget = (cols as usize).saturating_sub(chip_cols);
    let status_shown = truncate_cols(status, budget);
    let overlay_cols = status_shown.chars().count()
        + chips.iter().map(|chip| chip.len()).sum::<usize>()
        + chips.len().saturating_sub(1)
        + if !status_shown.is_empty() && !chips.is_empty() {
            1
        } else {
            0
        };
    let col = cols.saturating_sub(overlay_cols as u32).max(1);
    out.extend_from_slice(b"\x1b[?25l");
    let _ = write!(out, "\x1b[1;{col}H\x1b[0m{status_shown}");
    let mut wrote = !status_shown.is_empty();
    for chip in chips {
        if wrote {
            out.push(b' ');
        }
        wrote = true;
        out.extend_from_slice(b"\x1b[7m");
        out.extend_from_slice(chip.as_bytes());
        out.extend_from_slice(b"\x1b[0m");
    }
}

fn append_caret(out: &mut Vec<u8>, next: &PaintFrame) {
    let _ = write!(
        out,
        "\x1b[{};{}H",
        next.content.cursor_row.saturating_add(1),
        next.content.cursor_col.saturating_add(1)
    );
    if let Some(shape) = next.content.cursor_shape {
        out.extend_from_slice(shape.decscusr_bytes());
    }
    if next.content.cursor_visible {
        out.extend_from_slice(b"\x1b[?25h");
    }
}

fn append_copy_cursor(out: &mut Vec<u8>, frame: &PaintFrame, scroll: &ScrollState) {
    if !scroll.copy_mode {
        return;
    }
    let row = scroll
        .view_row_for_copy_point(scroll.copy_cursor)
        .min(frame.content.rows.saturating_sub(1));
    let col = scroll
        .copy_cursor
        .col
        .min(frame.content.cols.saturating_sub(1));
    let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
}

/// Mail letter. Amber is allowed (needs-you). Attach has no
/// pixels; the host default face (JetBrains Mono Nerd) already has
/// `nf-fa-envelope` at U+F0E0 and fits it to one cell. Not ✉ / U+1FB00
/// (emoji and sextant fallbacks overflow the cell).
const MAIL_LETTER_RGB: [u8; 3] = [0xff, 0xb4, 0x54];
const MAIL_CELL_COLS: usize = 1;
const MAIL_CELL_ROWS: usize = 1;
const MAIL_LETTER_CELLS: [[char; MAIL_CELL_COLS]; MAIL_CELL_ROWS] = [['\u{F0E0}']];

fn mail_letter_cells() -> [[char; MAIL_CELL_COLS]; MAIL_CELL_ROWS] {
    MAIL_LETTER_CELLS
}

fn pane_mail_depth(snapshot: &Snapshot, pane_id: u64) -> u32 {
    snapshot
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.mail.as_ref())
        .map(|mail| mail.depth)
        .unwrap_or(0)
}

fn pane_attention(snapshot: &Snapshot, pane_id: u64) -> Option<String> {
    snapshot
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.attention.clone())
}

fn valid_attention_message(message: &str) -> bool {
    message.len() <= 512 && !message.chars().any(char::is_control)
}

fn attention_osc_bytes(message: &str) -> Option<Vec<u8>> {
    valid_attention_message(message).then(|| format!("\x1b]9;{message}\x07").into_bytes())
}

fn emit_attention(message: &str) -> io::Result<()> {
    let Some(bytes) = attention_osc_bytes(message) else {
        return Ok(());
    };
    let mut out = io::stdout();
    out.write_all(&bytes)?;
    out.flush()
}
fn apply_mail_event(target_pane: u64, event: &Event, mail_depth: &mut u32) -> bool {
    match event {
        Event::MailAttentionChanged { pane_id, depth, .. }
            if *pane_id == target_pane && *mail_depth != *depth =>
        {
            *mail_depth = *depth;
            true
        }
        _ => false,
    }
}

fn apply_attention_event(target_pane: u64, event: &Event, attention: &mut Option<String>) -> bool {
    match event {
        Event::PaneAttention { pane_id, message } if *pane_id == target_pane => {
            *attention = Some(message.clone());
            true
        }
        Event::PaneAttentionCleared { pane_id }
            if *pane_id == target_pane && attention.is_some() =>
        {
            *attention = None;
            true
        }
        _ => false,
    }
}
fn mail_letter_overlay_bytes() -> Vec<u8> {
    let [fr, fg, fb] = MAIL_LETTER_RGB;
    let cells = mail_letter_cells();
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[?25l");
    for (row, line) in cells.iter().enumerate() {
        let _ = write!(out, "\x1b[{};1H\x1b[0m\x1b[38;2;{fr};{fg};{fb}m", row + 1);
        for ch in line {
            let mut utf8 = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
        }
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

/// OSC 2 title the host reads per pane (PT-148): the guest status or pane
/// title when one is set, else the `pmux: NAME` base; a mail suffix while
/// letters wait. The host drops the base form and keeps the label.
fn mail_title_osc(session_name: Option<&str>, label: Option<&str>, depth: u32) -> Vec<u8> {
    let base = match (label, session_name) {
        (Some(label), _) if !label.trim().is_empty() => label.trim().to_string(),
        (_, Some(name)) if !name.is_empty() => format!("pmux: {name}"),
        _ => "pmux-attach".to_string(),
    };
    if depth == 0 {
        format!("\x1b]2;{base}\x07").into_bytes()
    } else {
        format!("\x1b]2;{base} — {depth} mail\x07").into_bytes()
    }
}

fn append_mail_overlay(out: &mut Vec<u8>, next: &PaintFrame, mail_depth: u32) {
    if mail_depth == 0
        || next.content.cols < MAIL_CELL_COLS as u32
        || next.content.rows < MAIL_CELL_ROWS as u32
    {
        return;
    }
    out.extend_from_slice(&mail_letter_overlay_bytes());
    append_caret(out, next);
}

/// Host-owned attach toast. Paints on alt too: app viewport overlays do not.
fn append_attach_toast(
    out: &mut Vec<u8>,
    next: &PaintFrame,
    toast: Option<&PaneOverlay>,
    terminal_cols: u32,
) {
    let Some(overlay) = toast else {
        return;
    };
    if overlay.rows == 0 || overlay.cols == 0 || terminal_cols == 0 || next.content.rows == 0 {
        return;
    }
    let [br, bg, bb] = toast_focus_border_rgb();
    let [fr, fg, fb] = toast_contrast_fg([br, bg, bb]);
    let cells: Vec<char> = overlay.text.chars().collect();
    let mut cell_i = 0usize;
    for overlay_row in 0..overlay.rows {
        let absolute = overlay.row.saturating_add(i32::from(overlay_row));
        if absolute < 0 {
            cell_i = cell_i.saturating_add(usize::from(overlay.cols));
            continue;
        }
        let row = absolute as u32;
        if row >= next.content.rows {
            break;
        }
        for col_offset in 0..overlay.cols {
            let ch = cells.get(cell_i).copied().unwrap_or(' ');
            cell_i = cell_i.saturating_add(1);
            let col = u32::from(overlay.col).saturating_add(u32::from(col_offset));
            if col >= terminal_cols {
                continue;
            }
            let _ = write!(
                out,
                "\x1b[{};{}H\x1b[0m\x1b[1m\x1b[38;2;{fr};{fg};{fb}m\x1b[48;2;{br};{bg};{bb}m",
                row.saturating_add(1),
                col.saturating_add(1)
            );
            let mut utf8 = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    append_caret(out, next);
}

fn overlay_cells(overlay: &PaneOverlay) -> Vec<(char, OverlayRun)> {
    if overlay.runs.is_empty() {
        return overlay
            .text
            .chars()
            .map(|ch| {
                (
                    ch,
                    OverlayRun {
                        text: String::new(),
                        fg: None,
                        bg: None,
                        bold: false,
                        italic: false,
                        underline: false,
                        inverse: true,
                    },
                )
            })
            .collect();
    }
    let mut out = Vec::new();
    for run in &overlay.runs {
        for ch in run.text.chars() {
            out.push((ch, run.clone()));
        }
    }
    out
}

fn write_overlay_style(out: &mut Vec<u8>, style: &OverlayRun) {
    out.extend_from_slice(b"\x1b[0m");
    if style.bold {
        out.extend_from_slice(b"\x1b[1m");
    }
    if style.italic {
        out.extend_from_slice(b"\x1b[3m");
    }
    if style.underline {
        out.extend_from_slice(b"\x1b[4m");
    }
    if style.inverse {
        out.extend_from_slice(b"\x1b[7m");
    }
    if let Some(n) = style.fg {
        let _ = write!(out, "\x1b[38;5;{n}m");
    }
    if let Some(n) = style.bg {
        let _ = write!(out, "\x1b[48;5;{n}m");
    }
}

fn paint_one_overlay(
    out: &mut Vec<u8>,
    overlay: &PaneOverlay,
    cols: u32,
    rows: u32,
    row_offset: u32,
) {
    if overlay.rows == 0 || overlay.cols == 0 || cols == 0 || rows == 0 {
        return;
    }
    let cells = overlay_cells(overlay);
    let mut cell_i = 0usize;
    if overlay.row < 0 {
        let skipped = overlay.row.unsigned_abs() as usize;
        cell_i = skipped.saturating_mul(usize::from(overlay.cols));
    }
    for overlay_row in 0..overlay.rows {
        let absolute = overlay.row.saturating_add(i32::from(overlay_row));
        if absolute < 0 {
            continue;
        }
        let row = (absolute as u32).saturating_add(row_offset);
        if row >= rows {
            break;
        }
        for col_offset in 0..overlay.cols {
            let (ch, style) = cells.get(cell_i).cloned().unwrap_or((
                ' ',
                OverlayRun {
                    text: String::new(),
                    fg: None,
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    inverse: true,
                },
            ));
            cell_i = cell_i.saturating_add(1);
            let col = u32::from(overlay.col).saturating_add(u32::from(col_offset));
            if col >= cols {
                continue;
            }
            let _ = write!(
                out,
                "\x1b[{};{}H",
                row.saturating_add(1),
                col.saturating_add(1)
            );
            write_overlay_style(out, &style);
            let mut utf8 = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    }
}

/// z1 `cell_rect` then z2 `viewport`. Clip to the attach grid.
/// Skip `cell_rect` while panning history. Skip all overlays on alt.
fn append_rich_overlays(out: &mut Vec<u8>, next: &PaintFrame) {
    if next.overlays.is_empty() || next.content.alt_active {
        return;
    }
    let scrolled = next.view_offset.unwrap_or(0) > 0;
    let mut ordered: Vec<&PaneOverlay> = next
        .overlays
        .iter()
        .filter(|overlay| !(scrolled && overlay.kind == OverlayKind::CellRect))
        .collect();
    ordered.sort_by_key(|overlay| match overlay.kind {
        OverlayKind::CellRect => 0,
        OverlayKind::Viewport => 1,
    });
    if ordered.is_empty() {
        return;
    }
    out.extend_from_slice(b"\x1b[?25l");
    for overlay in ordered {
        paint_one_overlay(
            out,
            overlay,
            next.content.cols,
            next.content.rows,
            next.workspace_rows,
        );
    }
    append_caret(out, next);
}

/// Nested copy-mode chip text.
///
/// An empty live query omits `0/0`. A typed query with no matches
/// shows `0/0`. Unit tests cover both match guards (PT-306).
fn scroll_copy_suffix(scroll: &ScrollState) -> String {
    if !scroll.copy_mode {
        return String::new();
    }
    if let Some(dir) = scroll.search.prompt {
        let rank = match copy_search_rank(scroll) {
            Some((i, n)) => format!(" {i}/{n}"),
            None if scroll.search.query.is_empty() => String::new(),
            None => " 0/0".into(),
        };
        return format!(" copy {}{}█{rank}", dir.prompt_char(), scroll.search.query);
    }
    if !scroll.search.committed.is_empty() {
        let rank = match copy_search_rank(scroll) {
            Some((i, n)) => format!(" {i}/{n}"),
            None => " 0/0".into(),
        };
        return format!(
            " copy {}{}{rank}",
            scroll.search.dir.prompt_char(),
            scroll.search.committed
        );
    }
    if scroll.copy_selecting {
        return " copy select".into();
    }
    " copy".into()
}

fn host_nested_scroll_chip(offset: u32, max: u32, suffix: &str) -> String {
    format!(" {offset}/{max}{suffix} ")
}

/// Thumb start row (1-based) and height for a right-edge scrollbar.
/// `offset == 0` is the live tail (thumb at the bottom).
///
/// `||`→`&&` and `/`→`*` mutants are follow-up coverage
/// (PT-306 mux scrollbar/seat-route), not the log-replica core.
#[mutants::skip]
fn host_nested_scrollbar_thumb(rows: usize, offset: u32, max: u32) -> (usize, usize) {
    let rows = rows.max(1);
    let thumb = (rows / 4).max(1).min(rows);
    if max == 0 || rows == 1 {
        return (rows.saturating_sub(thumb).saturating_add(1), thumb);
    }
    let travel = rows.saturating_sub(thumb);
    let from_bottom = travel.saturating_mul(offset as usize) / max as usize;
    let start = rows.saturating_sub(thumb).saturating_sub(from_bottom) + 1;
    (start.max(1), thumb)
}

/// Nested fallback scrollbar + inverse chip.
///
/// Thumb-row comparisons (`>=`→`<`, `<`→`<=`) are follow-up coverage
/// (PT-306 mux scrollbar/seat-route), not the log-replica core.
#[allow(clippy::too_many_arguments)]
#[mutants::skip]
fn append_host_nested_scroll_chrome(
    bytes: &mut Vec<u8>,
    rows: u32,
    cols: usize,
    offset: u32,
    max: u32,
    suffix: &str,
    status: Option<&str>,
    chip_suffix: &str,
) {
    let rows_usize = rows as usize;
    let (thumb_start, thumb_len) = host_nested_scrollbar_thumb(rows_usize, offset, max);
    let bar_col = cols.max(1);
    bytes.extend_from_slice(b"\x1b[?25l");
    for row in 1..=rows_usize {
        let glyph = if row >= thumb_start && row < thumb_start.saturating_add(thumb_len) {
            '█'
        } else {
            '│'
        };
        let _ = write!(bytes, "\x1b[{row};{bar_col}H\x1b[0m{glyph}");
    }
    let mut chip = format!(
        "{}{}",
        host_nested_scroll_chip(offset, max, suffix),
        chip_suffix
    );
    if let Some(status) = status {
        chip.push_str(" │ ");
        chip.push_str(status);
    }
    let line = truncate_cols(&chip, cols.saturating_sub(1).max(1));
    let chip_cols = line.chars().count().min(cols.saturating_sub(1)).max(1);
    let chip_col = cols.saturating_sub(chip_cols);
    let chip_col = chip_col.max(1);
    let _ = write!(
        bytes,
        "\x1b[{rows};{chip_col}H\x1b[7m{line}\x1b[27m\x1b[?25l"
    );
}

#[allow(clippy::too_many_arguments)]
fn compose_paint_with_terminal_cols(
    prev: Option<&PaintFrame>,
    next: &PaintFrame,
    scroll: &ScrollState,
    mail_depth: u32,
    prev_mail: Option<u32>,
    session_name: Option<&str>,
    toast: Option<&PaneOverlay>,
    terminal_cols: u32,
    chrome: AttachChrome<'_>,
) -> Vec<u8> {
    let mut bytes = paint_bytes(prev, next, scroll.pending_clipboard.as_deref());
    if scroll.copy_mode {
        bytes.extend_from_slice(b"\x1b[?25l");
    }
    append_copy_search(&mut bytes, next, scroll);
    append_copy_selection(&mut bytes, next, scroll);
    append_rich_overlays(&mut bytes, next);
    append_mail_overlay(&mut bytes, next, mail_depth);
    append_attach_toast(&mut bytes, next, toast, terminal_cols);
    let chip = chrome.chip_suffix();
    let status = chrome.pane_status.filter(|text| !text.is_empty());
    let cols = next.content.cols as usize;
    if scroll.active {
        if let Some(max) = scroll.max {
            let row = next.content.rows.max(1);
            let suffix = scroll_copy_suffix(scroll);
            if chrome.host_nested {
                append_host_nested_scroll_chrome(
                    &mut bytes,
                    row,
                    cols,
                    scroll.offset,
                    max,
                    &suffix,
                    status,
                    &chip,
                );
            } else {
                let prefix = format!("[scroll {}/{}{suffix}]{chip}", scroll.offset, max);
                let line = fit_status_line(&prefix, status, cols);
                let _ = write!(bytes, "\x1b[{row};1H\x1b[K\x1b[0m{line}\x1b[?25l");
            }
        }
    } else if status.is_some()
        || !chrome.chips().is_empty()
        || chrome.viewport_hint.is_some()
        || chrome.identity.is_some()
    {
        // Corner overlay. Do not CSI-K the last guest row; restore the caret
        // the same way mail-letter paint does.
        append_live_chrome(&mut bytes, next.content.cols, chrome);
        append_caret(&mut bytes, next);
    }
    append_copy_cursor(&mut bytes, next, scroll);
    // Re-emit on first paint, on a mail change, and while a label is set
    // (a status or title can change without a mail event; the OSC is a few
    // bytes on a local PTY).
    if prev.is_none() || prev_mail != Some(mail_depth) || chrome.pane_status.is_some() {
        bytes.extend_from_slice(&mail_title_osc(
            session_name,
            chrome.pane_status,
            mail_depth,
        ));
    }
    bytes
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn compose_paint(
    prev: Option<&PaintFrame>,
    next: &PaintFrame,
    scroll: &ScrollState,
    mail_depth: u32,
    prev_mail: Option<u32>,
    session_name: Option<&str>,
    toast: Option<&PaneOverlay>,
    chrome: AttachChrome<'_>,
) -> Vec<u8> {
    compose_paint_with_terminal_cols(
        prev,
        next,
        scroll,
        mail_depth,
        prev_mail,
        session_name,
        toast,
        next.content.cols,
        chrome,
    )
}

#[allow(clippy::too_many_arguments)]
fn paint(
    prev: Option<&PaintFrame>,
    next: &PaintFrame,
    scroll: &ScrollState,
    mail_depth: u32,
    prev_mail: Option<u32>,
    session_name: Option<&str>,
    toast: Option<&PaneOverlay>,
    chrome: AttachChrome<'_>,
    term: Option<(u32, u32)>,
) -> io::Result<()> {
    let hint_owned;
    let clipped;
    let toast_rebuilt;
    let (chrome, toast) = if let Some((cols, rows)) = term {
        hint_owned = viewport_hint(next.content.cols, next.content.rows, cols, rows);
        clipped = clip_frame_to_terminal(next.clone(), cols, rows);
        toast_rebuilt = toast.map(|overlay| attach_toast_overlay(&overlay.text, cols));
        (
            AttachChrome {
                viewport_hint: hint_owned.as_deref(),
                ..chrome
            },
            toast_rebuilt.as_ref(),
        )
    } else {
        clipped = next.clone();
        (chrome, toast)
    };
    let toast_cols = term.map(|(cols, _)| cols).unwrap_or(next.content.cols);
    let bytes = compose_paint_with_terminal_cols(
        prev,
        &clipped,
        scroll,
        mail_depth,
        prev_mail,
        session_name,
        toast,
        toast_cols,
        chrome,
    );
    let mut out = io::stdout();
    out.write_all(&bytes)?;
    out.flush()
}

/// Keep `controller` and `[held]` in lockstep with the server lease owner.
fn apply_controller_owner(
    our_id: u64,
    controller_id: Option<u64>,
    lease_held: &mut bool,
    controller: &mut bool,
) -> bool {
    let held = controller_id.is_some_and(|id| id != our_id);
    let we_hold = controller_id == Some(our_id);
    let mut changed = false;
    if *lease_held != held {
        *lease_held = held;
        changed = true;
    }
    if *controller != we_hold {
        *controller = we_hold;
        changed = true;
    }
    changed
}

/// Drain mail, generic attention, and sync-input events since `after_sequence`.
#[allow(clippy::too_many_arguments)]
fn drain_events(
    client: &mut Client,
    pane_id: u64,
    window_id: u64,
    client_id: u64,
    after_sequence: &mut u64,
    mail_depth: &mut u32,
    attention: &mut Option<String>,
    sync_input: &mut bool,
    pane_status: &mut Option<String>,
    lease_held: &mut bool,
    controller: &mut bool,
) -> Result<bool> {
    let mut changed = false;
    let mut title_changed = false;
    loop {
        match client.request(|request_id| ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id,
            after_sequence: *after_sequence,
            limit: Some(64),
        }) {
            Ok(ControlResponseData::Events { batch }) => {
                for envelope in &batch.events {
                    if apply_mail_event(pane_id, &envelope.event, mail_depth) {
                        changed = true;
                    }
                    if apply_attention_event(pane_id, &envelope.event, attention) {
                        if let Event::PaneAttention { message, .. } = &envelope.event {
                            emit_attention(message)?;
                        }
                        changed = true;
                    }
                    if let Event::SyncInputChanged {
                        window_id: changed_window,
                        enabled,
                    } = &envelope.event
                    {
                        if *changed_window == window_id && *sync_input != *enabled {
                            *sync_input = *enabled;
                            changed = true;
                        }
                    }
                    if let Event::PaneStatusChanged {
                        pane_id: changed_pane,
                        status,
                    } = &envelope.event
                    {
                        if *changed_pane == pane_id && pane_status.as_ref() != status.as_ref() {
                            *pane_status = status.clone();
                            changed = true;
                        }
                    }
                    if let Event::PaneRenamed {
                        pane_id: changed_pane,
                        ..
                    } = &envelope.event
                    {
                        // Status-or-title lives in one slot; a fresh
                        // snapshot resolves which one shows (PT-128).
                        if *changed_pane == pane_id {
                            title_changed = true;
                        }
                    }
                    if let Event::LeaseChanged {
                        pane_id: changed_pane,
                        controller_id,
                        ..
                    } = &envelope.event
                    {
                        if *changed_pane == pane_id {
                            changed |= apply_controller_owner(
                                client_id,
                                *controller_id,
                                lease_held,
                                controller,
                            );
                        }
                    }
                }
                *after_sequence = batch.through_sequence;
                if !batch.has_more {
                    break;
                }
            }
            Err(error)
                if matches!(
                    control_code(&error),
                    Some(
                        ControlErrorCode::EventGap
                            | ControlErrorCode::SnapshotRequired
                            | ControlErrorCode::StaleSequence
                    )
                ) =>
            {
                changed |= resnapshot_state(
                    client,
                    pane_id,
                    window_id,
                    client_id,
                    after_sequence,
                    mail_depth,
                    attention,
                    sync_input,
                    pane_status,
                    lease_held,
                    controller,
                )?;
                break;
            }
            Ok(_) | Err(_) => break,
        }
    }
    if title_changed {
        if let Ok(ControlResponseData::Snapshot { snapshot }) =
            client.request(|request_id| ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id,
            })
        {
            let next_status = pane_guest_status(&snapshot, pane_id);
            if next_status != *pane_status {
                *pane_status = next_status;
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Refresh mail, generic attention, and sync-input from a fresh snapshot after an event gap.
#[allow(clippy::too_many_arguments)]
fn resnapshot_state(
    client: &mut Client,
    pane_id: u64,
    window_id: u64,
    client_id: u64,
    after_sequence: &mut u64,
    mail_depth: &mut u32,
    attention: &mut Option<String>,
    sync_input: &mut bool,
    pane_status: &mut Option<String>,
    lease_held: &mut bool,
    controller: &mut bool,
) -> Result<bool> {
    match client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        Ok(ControlResponseData::Snapshot { snapshot }) => {
            *after_sequence = snapshot.sequence;
            let next = pane_mail_depth(&snapshot, pane_id);
            let next_attention = pane_attention(&snapshot, pane_id);
            let next_sync = snapshot
                .sessions
                .iter()
                .flat_map(|session| session.windows.iter())
                .find(|window| window.id == window_id)
                .map(|window| window.sync_input)
                .unwrap_or(*sync_input);
            let mut changed = false;
            if next != *mail_depth {
                *mail_depth = next;
                changed = true;
            }
            if next_attention != *attention {
                if let Some(message) = next_attention.as_deref() {
                    emit_attention(message)?;
                }
                *attention = next_attention;
                changed = true;
            }
            if next_sync != *sync_input {
                *sync_input = next_sync;
                changed = true;
            }
            let next_status = pane_guest_status(&snapshot, pane_id);
            if next_status != *pane_status {
                *pane_status = next_status;
                changed = true;
            }
            let holder = snapshot
                .sessions
                .iter()
                .flat_map(|session| session.windows.iter())
                .flat_map(|window| window.panes.iter())
                .find(|pane| pane.id == pane_id)
                .and_then(|pane| pane.controller_id);
            changed |= apply_controller_owner(client_id, holder, lease_held, controller);
            Ok(changed)
        }
        Ok(_) | Err(_) => Ok(false),
    }
}

enum AttachEnd {
    Detached,
    ChildExited,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CopyPoint {
    /// Rows above the live tail. This is stable while the viewport scrolls.
    row: u32,
    col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CopySearchDir {
    #[default]
    Forward,
    Reverse,
}

impl CopySearchDir {
    fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Reverse,
            Self::Reverse => Self::Forward,
        }
    }

    fn prompt_char(self) -> char {
        match self {
            Self::Forward => '/',
            Self::Reverse => '?',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyMatch {
    point: CopyPoint,
    len: u32,
}

#[derive(Debug, Clone, Default)]
struct CopySearch {
    /// Open `/` or `?` prompt. None after Enter or Esc.
    prompt: Option<CopySearchDir>,
    /// Buffer while the prompt is open.
    query: String,
    /// Last committed needle. `n`/`N` and Esc keep this (vim/tmux repeat).
    committed: String,
    dir: CopySearchDir,
    /// History-stable location of the current match (not a viewport index).
    current: Option<CopyPoint>,
    /// Last painted viewport lines (status row included; search skips it).
    lines: Vec<String>,
}

impl CopySearch {
    fn needle(&self) -> &str {
        if self.prompt.is_some() {
            &self.query
        } else {
            &self.committed
        }
    }
}

impl CopySearch {
    fn reset_keep_lines(&mut self) {
        let lines = std::mem::take(&mut self.lines);
        *self = Self {
            lines,
            ..Self::default()
        };
    }
}

#[derive(Debug, Default)]
struct ScrollState {
    active: bool,
    copy_mode: bool,
    copy_selecting: bool,
    copy_anchor: Option<CopyPoint>,
    copy_cursor: CopyPoint,
    copy_yank_requested: bool,
    pending_clipboard: Option<String>,
    search: CopySearch,
    arrange_step: u8,
    supported: bool,
    offset: u32,
    max: Option<u32>,
    rows: u32,
    cols: u32,
    /// `None` until the first `ReadPaneStyled` that reports the field.
    child_mouse: Option<bool>,
    child_sgr: bool,
    alt_active: bool,
}

impl ScrollState {
    fn page(&self) -> u32 {
        self.rows.saturating_sub(1).max(1)
    }

    fn enter_and_page_up(&mut self) {
        self.active = true;
        self.copy_mode = true;
        self.copy_selecting = false;
        self.copy_anchor = None;
        self.copy_yank_requested = false;
        let offset = self
            .max
            .map(|max| self.offset.saturating_add(self.page()).min(max))
            .unwrap_or(self.offset);
        self.offset = offset;
        self.copy_cursor = CopyPoint {
            row: offset,
            col: 0,
        };
    }

    fn leave(&mut self) {
        self.active = false;
        self.copy_mode = false;
        self.copy_selecting = false;
        self.copy_anchor = None;
        self.copy_yank_requested = false;
        self.search.reset_keep_lines();
        self.offset = 0;
    }

    fn apply_nav(&mut self, nav: NavKey) {
        let Some(max) = self.max else {
            return;
        };
        let page = self.page();
        self.offset = match nav {
            NavKey::Up => self.offset.saturating_add(1).min(max),
            NavKey::Down => self.offset.saturating_sub(1),
            NavKey::Left | NavKey::Right => self.offset,
            NavKey::PageUp => self.offset.saturating_add(page).min(max),
            NavKey::PageDown => self.offset.saturating_sub(page),
            NavKey::Home => max,
            NavKey::End => 0,
        };
    }

    fn copy_last_row(&self) -> u32 {
        // The final terminal row is occupied by the local scroll status line.
        self.rows.saturating_sub(2)
    }

    /// Convert a stable history point to its current viewport row.
    fn view_row_for_copy_point(&self, point: CopyPoint) -> u32 {
        let row = i64::from(self.copy_last_row()) + i64::from(self.offset) - i64::from(point.row);
        row.clamp(0, i64::from(self.copy_last_row())) as u32
    }

    fn copy_view_point(&self, point: CopyPoint) -> CopyPoint {
        CopyPoint {
            row: self.view_row_for_copy_point(point),
            col: point.col,
        }
    }

    fn set_copy_offset(&mut self, offset: u32) {
        self.offset = offset;
    }

    /// Move only the viewport. Copy endpoints are stable history points.
    fn set_copy_offset_preserving_mark(&mut self, offset: u32) {
        self.offset = offset;
    }

    fn apply_copy_nav(&mut self, nav: NavKey) {
        let Some(max) = self.max else {
            return;
        };
        let last_row = self.copy_last_row();
        let page = self.page();
        let marked = self.copy_selecting && self.copy_anchor.is_some();
        let cursor_row = self.view_row_for_copy_point(self.copy_cursor);
        match nav {
            NavKey::Up if cursor_row > 0 => {
                self.copy_cursor.row = self.copy_cursor.row.saturating_add(1)
            }
            NavKey::Up => {
                let offset = self.offset.saturating_add(1).min(max);
                self.set_copy_offset(offset);
                self.copy_cursor.row = self.copy_cursor.row.saturating_add(1);
            }
            NavKey::Down if cursor_row < last_row => {
                self.copy_cursor.row = self.copy_cursor.row.saturating_sub(1)
            }
            NavKey::Down => {
                let offset = self.offset.saturating_sub(1);
                self.set_copy_offset(offset);
                self.copy_cursor.row = self.copy_cursor.row.saturating_sub(1);
            }
            NavKey::Left => self.copy_cursor.col = self.copy_cursor.col.saturating_sub(1),
            NavKey::Right => {
                let last_col = self.cols.saturating_sub(1);
                self.copy_cursor.col = self.copy_cursor.col.saturating_add(1).min(last_col);
            }
            NavKey::PageUp => {
                let offset = self.offset.saturating_add(page).min(max);
                self.set_copy_offset_preserving_mark(offset);
                if !marked {
                    self.copy_cursor.row = offset.saturating_add(last_row);
                }
            }
            NavKey::PageDown => {
                let offset = self.offset.saturating_sub(page);
                self.set_copy_offset_preserving_mark(offset);
                if !marked {
                    self.copy_cursor.row = offset;
                }
            }
            NavKey::Home => {
                self.set_copy_offset_preserving_mark(max);
                if !marked {
                    self.copy_cursor.row = max.saturating_add(last_row);
                }
            }
            NavKey::End => {
                self.set_copy_offset_preserving_mark(0);
                if !marked {
                    self.copy_cursor.row = 0;
                }
            }
        }
    }

    fn toggle_copy_selection(&mut self) {
        if self.copy_selecting {
            self.copy_yank_requested = true;
        } else {
            self.copy_selecting = true;
            self.copy_anchor = Some(self.copy_cursor);
        }
    }

    fn request_copy_yank(&mut self) {
        self.copy_yank_requested = true;
    }

    fn take_copy_yank_request(&mut self) -> bool {
        std::mem::take(&mut self.copy_yank_requested)
    }

    fn clear_pending_clipboard(&mut self) {
        self.pending_clipboard = None;
    }

    /// Wheel-up enters scroll mode (or scrolls deeper) by WHEEL_LINES.
    fn wheel_up(&mut self) {
        let Some(max) = self.max else {
            return;
        };
        if max == 0 {
            return;
        }
        self.active = true;
        let offset = self.offset.saturating_add(WHEEL_LINES).min(max);
        if self.copy_mode && self.copy_selecting && self.copy_anchor.is_some() {
            self.set_copy_offset_preserving_mark(offset);
        } else {
            self.set_copy_offset(offset);
        }
    }

    /// Wheel-down scrolls toward the tail. Copy mode remains active at live view.
    fn wheel_down(&mut self) {
        let offset = self.offset.saturating_sub(WHEEL_LINES);
        if self.copy_mode && self.copy_selecting && self.copy_anchor.is_some() {
            self.set_copy_offset_preserving_mark(offset);
        } else if self.copy_mode {
            self.set_copy_offset(offset);
        } else {
            self.offset = offset;
            if self.offset == 0 {
                self.leave();
            }
        }
    }

    /// Keep the same history window when new lines land at the live tail.
    fn anchor_to(&mut self, new_max: Option<u32>) {
        match (self.active, self.max, new_max) {
            (true, Some(old), Some(new)) if new > old => {
                let growth = new - old;
                self.offset = self.offset.saturating_add(growth).min(new);
                self.max = Some(new);
                self.copy_cursor.row = self.copy_cursor.row.saturating_add(growth);
                if let Some(anchor) = &mut self.copy_anchor {
                    anchor.row = anchor.row.saturating_add(growth);
                }
                if let Some(point) = &mut self.search.current {
                    point.row = point.row.saturating_add(growth);
                }
            }
            (_, _, Some(new)) => {
                self.max = Some(new);
                if self.offset > new {
                    self.offset = new;
                }
                if new == 0 {
                    self.leave();
                }
            }
            (_, _, None) => {
                self.max = None;
                self.supported = false;
                self.leave();
            }
        }
    }

    fn paint_state(&self) -> (bool, bool, bool, Option<CopyPoint>, CopyPoint, u32) {
        (
            self.active,
            self.copy_mode,
            self.copy_selecting,
            self.copy_anchor,
            self.copy_cursor,
            self.offset,
        )
    }

    fn search_paint_state(&self) -> (Option<CopySearchDir>, String, Option<CopyPoint>) {
        (
            self.search.prompt,
            self.search.needle().to_string(),
            self.search.current,
        )
    }

    fn start_copy_search(&mut self, dir: CopySearchDir) {
        self.search.prompt = Some(dir);
        self.search.dir = dir;
        self.search.query.clear();
    }

    fn cancel_copy_search_prompt(&mut self) {
        self.search.prompt = None;
        self.search.query.clear();
    }

    fn commit_copy_search_prompt(&mut self) {
        if !self.search.query.is_empty() {
            self.search.committed = self.search.query.clone();
        }
        self.search.prompt = None;
        self.search.query.clear();
        self.copy_search_step(false, true);
    }

    fn copy_search_type(&mut self, ch: char) {
        if ch == '\u{7f}' || ch == '\u{8}' {
            self.search.query.pop();
        } else if !ch.is_control() {
            self.search.query.push(ch);
        } else {
            return;
        }
        self.search.current = None;
        self.copy_search_step(false, true);
    }

    fn copy_search_step(&mut self, reverse: bool, inclusive: bool) {
        if self.search.needle().is_empty() {
            self.search.current = None;
            return;
        }
        let dir = if reverse {
            self.search.dir.opposite()
        } else {
            self.search.dir
        };
        let matches = copy_search_matches(self);
        if matches.is_empty() {
            self.search.current = None;
            return;
        }
        let cursor = self.copy_view_point(self.copy_cursor);
        let view_of = |m: &CopyMatch| {
            let view = self.copy_view_point(m.point);
            (view.row, view.col)
        };
        let here = (cursor.row, cursor.col);
        let idx = match dir {
            CopySearchDir::Forward => matches.iter().position(|m| {
                if inclusive {
                    view_of(m) >= here
                } else {
                    view_of(m) > here
                }
            }),
            CopySearchDir::Reverse => matches.iter().rposition(|m| {
                if inclusive {
                    view_of(m) <= here
                } else {
                    view_of(m) < here
                }
            }),
        }
        .unwrap_or(match dir {
            CopySearchDir::Forward => 0,
            CopySearchDir::Reverse => matches.len() - 1,
        });
        self.search.current = Some(matches[idx].point);
        self.reveal_copy_point(matches[idx].point);
    }

    fn reveal_copy_point(&mut self, point: CopyPoint) {
        let last = self.copy_last_row();
        if let Some(max) = self.max {
            let view = i64::from(last) + i64::from(self.offset) - i64::from(point.row);
            if view < 0 {
                let delta = u32::try_from(-view).unwrap_or(0);
                self.offset = self.offset.saturating_add(delta).min(max);
            } else if view > i64::from(last) {
                let delta = u32::try_from(view - i64::from(last)).unwrap_or(0);
                self.offset = self.offset.saturating_sub(delta);
            }
        }
        self.copy_cursor = point;
    }

    fn apply_frame_flags(&mut self, frame: &PaintFrame) {
        self.cols = frame.content.cols;
        self.search.lines = frame.content.lines.clone();
        if frame.max_view_scroll.is_some() {
            self.supported = true;
        }
        if let Some(tracking) = frame.child_mouse_tracking {
            self.child_mouse = Some(tracking);
        }
        if let Some(sgr) = frame.child_mouse_sgr {
            self.child_sgr = sgr;
        }
        self.alt_active = frame.content.alt_active;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WheelDecision {
    HostScrollUp,
    HostScrollDown,
    Forward(Vec<u8>),
    Ignore,
}

/// Re-encode one wheel notch for the child (SGR or X10). `x`/`y` are 1-based.
fn encode_child_wheel(sgr: bool, up: bool, x: u32, y: u32) -> Vec<u8> {
    let btn = if up { 64u32 } else { 65 };
    let x = x.max(1);
    let y = y.max(1);
    if sgr {
        format!("\x1b[<{btn};{x};{y}M").into_bytes()
    } else {
        let enc_b = u8::try_from(btn).unwrap_or(64).saturating_add(32);
        let enc_x = u8::try_from(x.min(223)).unwrap_or(1).saturating_add(32);
        let enc_y = u8::try_from(y.min(223)).unwrap_or(1).saturating_add(32);
        vec![0x1b, b'[', b'M', enc_b, enc_x, enc_y]
    }
}

/// Alternate-scroll fallback: three CSI arrows per notch (option a).
fn alternate_scroll_arrows(up: bool) -> Vec<u8> {
    let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
    let mut out = Vec::with_capacity(seq.len() * WHEEL_LINES as usize);
    for _ in 0..WHEEL_LINES {
        out.extend_from_slice(seq);
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct WheelCtx {
    up: bool,
    shift: bool,
    controller: bool,
    scroll_active: bool,
    scroll_supported: bool,
    max: Option<u32>,
    child_mouse: Option<bool>,
    child_sgr: bool,
    alt_active: bool,
    x: u32,
    y: u32,
}

fn decide_wheel(ctx: WheelCtx) -> WheelDecision {
    if ctx.scroll_active {
        return if ctx.up {
            WheelDecision::HostScrollUp
        } else {
            WheelDecision::HostScrollDown
        };
    }
    if !ctx.shift && ctx.child_mouse == Some(true) && ctx.controller {
        return WheelDecision::Forward(encode_child_wheel(ctx.child_sgr, ctx.up, ctx.x, ctx.y));
    }
    // Option (a): empty history, known-off mouse, alt screen (TUIs). Skip when
    // mouse flags are still unknown so the first poll cannot emit CSI arrows.
    if !ctx.shift
        && ctx.max == Some(0)
        && ctx.child_mouse == Some(false)
        && ctx.alt_active
        && ctx.controller
    {
        return WheelDecision::Forward(alternate_scroll_arrows(ctx.up));
    }
    if ctx.up && ctx.scroll_supported {
        return WheelDecision::HostScrollUp;
    }
    if !ctx.up && ctx.scroll_active {
        return WheelDecision::HostScrollDown;
    }
    WheelDecision::Ignore
}

/// Server-owned history the attach client can pan.
/// `supported` is true after the first styled frame; empty TUIs still report
/// `max == 0` and must not enter scroll mode.
fn attach_history_available(scroll: &ScrollState) -> bool {
    scroll.supported && scroll.max.unwrap_or(0) > 0
}

/// Buffered lone ESC (incomplete CSI) after POLL_WAIT. Search prompt
/// cancels; copy mode otherwise leaves.
fn apply_lone_escape(scroll: &mut ScrollState) -> bool {
    if scroll.search.prompt.is_some() {
        scroll.cancel_copy_search_prompt();
        true
    } else if scroll.active {
        scroll.leave();
        true
    } else {
        false
    }
}

/// Idle poll tick: `pending_keys == [0x1b]` and no further input. Drop a
/// leftover UTF-8 fragment so `handle_input` does not grab a lease from
/// inside copy mode.
fn apply_idle_lone_escape(scroll: &mut ScrollState, pending_utf8: &mut Vec<u8>) -> bool {
    if apply_lone_escape(scroll) {
        pending_utf8.clear();
        true
    } else {
        false
    }
}

/// Remove ESC / CSI / SS3 from search-prompt bytes so `[` and CSI letters
/// never enter the query. A lone `[` without ESC stays (literal search).
fn strip_search_csi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                i += 2;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
            }
            b'O' => {
                i += 2;
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn feed_copy_search_bytes(scroll: &mut ScrollState, pending_utf8: &mut Vec<u8>, bytes: &[u8]) {
    pending_utf8.extend_from_slice(&strip_search_csi(bytes));
    while let Some(chunk) = take_utf8_prefix(pending_utf8) {
        if chunk.is_empty() {
            break;
        }
        for ch in chunk.chars() {
            scroll.copy_search_type(ch);
        }
    }
}

/// Wheel notches that belong to the child if we can hold the lease.
/// Shift and in-progress attach scroll stay host-owned.
#[cfg(test)]
fn wheel_wants_child(scroll: &ScrollState, shift: bool) -> bool {
    if shift || scroll.active {
        return false;
    }
    match scroll.child_mouse {
        Some(true) => true,
        Some(false) => scroll.max == Some(0) && scroll.alt_active,
        None => false,
    }
}

#[cfg(test)]
fn forward_child_mouse(scroll: &ScrollState, controller: bool, shift: bool) -> bool {
    controller && !scroll.active && !shift && scroll.child_mouse == Some(true)
}

/// Outer-terminal mouse modes for attach.
///
/// Click+drag (1000/1002) only when this attach holds the lease **and** the
/// pane child enabled application mouse. An idle observer (#143) must stay
/// on 7700 so prismattyc-host can drag-select. Wheel-only (7700) leaves buttons
/// with the host. Full mouse turns 7700 off so the host does not see both.
fn outer_mouse_full(controller: bool, child_mouse: Option<bool>, structured_focus: bool) -> bool {
    controller && (child_mouse == Some(true) || structured_focus)
}

fn outer_mouse_seq(child_full: bool) -> &'static [u8] {
    if child_full {
        // Bool from the server: tracking is on. Enable click+drag (1000/1002),
        // not 1003 any-motion, until the control plane reports a level.
        b"\x1b[?7700l\x1b[?1006h\x1b[?1000h\x1b[?1002h"
    } else {
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?7700h"
    }
}

fn sync_outer_mouse(child_full: bool, last: &mut Option<bool>) {
    if *last == Some(child_full) {
        return;
    }
    *last = Some(child_full);
    let mut out = io::stdout();
    let _ = out.write_all(outer_mouse_seq(child_full));
    let _ = out.flush();
}

struct InteractiveOpts {
    socket: PathBuf,
    already_controller: bool,
    mail_depth: u32,
    attention: Option<String>,
    event_seq: u64,
    session_name: Option<String>,
    space_name: Option<String>,
    sync_input: bool,
    pane_status: Option<String>,
    read_only: bool,
    lease_held: bool,
    fit: bool,
}

#[allow(clippy::too_many_arguments)]
fn retarget_interactive(
    client: &mut Client,
    client_id: u64,
    pane_id: &mut u64,
    window_id: &mut u64,
    session_name: &mut Option<String>,
    controller: &mut bool,
    last_typed: &mut Option<Instant>,
    mail_depth: &mut u32,
    attention: &mut Option<String>,
    pane_status: &mut Option<String>,
    lease_held: &mut bool,
    sync_input: &mut bool,
    event_seq: &mut u64,
    cycle_names: &mut Vec<String>,
    space_name: Option<&str>,
    target: &str,
) -> Result<()> {
    if *controller {
        let _ = keep_request_ok(
            "ReleaseLease",
            *pane_id,
            session_name.as_deref(),
            client.request(|request_id| ControlRequest::ReleaseLease {
                version: PROTOCOL_VERSION,
                request_id,
                client_id,
                pane_id: *pane_id,
            }),
        );
        // Leaving this pane: do not claim the next pane's lease.
        *controller = false;
        *last_typed = None;
    }
    let snapshot = take_snapshot(client)?;
    *cycle_names = cycle_session_names(space_name, &snapshot);
    let session_id = snapshot
        .sessions
        .iter()
        .find(|session| session.name == target)
        .map(|session| session.id)
        .with_context(|| format!("no session matching {target:?}"))?;
    keep_request_ok(
        "SwitchSession",
        *pane_id,
        session_name.as_deref(),
        client.request(|request_id| ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            session_id,
        }),
    )
    .context("SwitchSession failed")?;
    let new_pane = session_pane(&snapshot, target)?;
    let new_window = pane_window(&snapshot, new_pane)
        .context("selected session has no window in the snapshot")?;
    *pane_id = new_pane;
    *window_id = new_window;
    *session_name = Some(target.to_string());
    *mail_depth = pane_mail_depth(&snapshot, new_pane);
    *attention = pane_attention(&snapshot, new_pane);
    *pane_status = pane_guest_status(&snapshot, new_pane);
    *lease_held = pane_controller_id(&snapshot, new_pane).is_some_and(|id| id != client_id);
    *sync_input = pane_sync_input(&snapshot, new_pane).unwrap_or(false);
    *event_seq = snapshot.sequence;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn loop_input_ctx<'a>(
    client: &'a mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: Option<&'a str>,
    sync_input: &'a mut bool,
    controller: &'a mut bool,
    lease_held: &'a mut bool,
    last_typed: &'a mut Option<Instant>,
    pending_keys: &'a mut Vec<u8>,
    pending_utf8: &'a mut Vec<u8>,
    scroll: &'a mut ScrollState,
    previous: Option<&PaintFrame>,
    rich_focus_id: &'a mut Option<u32>,
    structured_focus: &'a mut bool,
    read_only: bool,
    held_notice: &'a mut Option<String>,
    session_switch: &'a mut Option<SessionSwitch>,
) -> InputCtx<'a> {
    InputCtx {
        client,
        client_id,
        pane_id,
        window_id,
        session,
        sync_input,
        controller,
        lease_held,
        last_typed,
        pending_keys,
        pending_utf8,
        scroll,
        experimental_rich: previous.is_some_and(|frame| frame.experimental_rich),
        rich_focus_id,
        structured_focus,
        workspace_rows: previous.map_or(0, |frame| frame.workspace_rows),
        child_pid: previous.and_then(|frame| frame.content.child_pid),
        read_only,
        held_notice,
        session_switch,
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_idle_release_lease(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    session: Option<&str>,
    controller: &mut bool,
    got_input: bool,
    pending_utf8: &[u8],
    rich_focus_id: &mut Option<u32>,
    structured_focus: &mut bool,
    last_typed: &mut Option<Instant>,
) {
    if !*controller
        || got_input
        || !pending_utf8.is_empty()
        || rich_focus_id.is_some()
        || !last_typed.is_some_and(|t| t.elapsed() >= LEASE_IDLE)
    {
        return;
    }
    if keep_request_ok(
        "ReleaseLease",
        pane_id,
        session,
        client.request(|request_id| ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        }),
    )
    .is_none()
    {
        return;
    }
    *controller = false;
    *last_typed = None;
    *rich_focus_id = None;
    *structured_focus = false;
}

#[allow(clippy::too_many_arguments)]
fn apply_local_winsize(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: Option<&str>,
    last_size: &mut Option<LocalWinsize>,
    last_sent: &mut Option<LocalWinsize>,
    previous: &mut Option<PaintFrame>,
    scroll: &mut ScrollState,
) {
    let size = local_winsize();
    apply_observed_winsize(last_size, previous, scroll, size);
    let Some(size) = size else {
        return;
    };
    if !resize_needs_send(*last_sent, size) {
        return;
    }
    if keep_request_ok(
        "Resize",
        pane_id,
        session,
        client.request(|request_id| {
            resize_request(request_id, window_id, size, client_id, false, false)
        }),
    )
    .is_some()
    {
        *last_sent = Some(size);
    }
}

/// Instant equality with `SPACE_STAMP_INTERVAL` is not stable in tests.
/// `<` vs `<=` / `>=` vs `>` only differ on that exact duration.
#[mutants::skip]
fn elapsed_meets_stamp(elapsed: Duration) -> bool {
    elapsed >= SPACE_STAMP_INTERVAL
}

#[allow(clippy::too_many_arguments)]
fn space_stamp_due(space_hinted: bool, spaces_stamp_checked: &mut Instant) -> bool {
    if !space_hinted {
        return false;
    }
    let now = Instant::now();
    if !elapsed_meets_stamp(now.duration_since(*spaces_stamp_checked)) {
        return false;
    }
    *spaces_stamp_checked = now;
    true
}

fn apply_resolved_space(
    client: &mut Client,
    space_hinted: bool,
    session: &str,
    space_name: &mut Option<String>,
    cycle_names: &mut Vec<String>,
    identity: &mut Option<String>,
    previous: &mut Option<PaintFrame>,
) {
    if let Ok(snapshot) = take_snapshot(client) {
        *cycle_names = cycle_session_names(space_name.as_deref(), &snapshot);
    }
    *identity = space_attach_identity(
        space_hinted,
        under_host(),
        space_name.as_deref(),
        Some(session),
        cycle_names,
    );
    *previous = None;
}

#[allow(clippy::too_many_arguments)]
fn refresh_space_identity(
    client: &mut Client,
    space_hinted: bool,
    spaces_stamp_checked: &mut Instant,
    spaces_stamp: &mut Option<SystemTime>,
    session_name: Option<&str>,
    space_name: &mut Option<String>,
    cycle_names: &mut Vec<String>,
    identity: &mut Option<String>,
    previous: &mut Option<PaintFrame>,
) {
    if !space_stamp_due(space_hinted, spaces_stamp_checked) {
        return;
    }
    let stamp = spaces_dir_stamp(&spaces_dir());
    if stamp == *spaces_stamp {
        return;
    }
    *spaces_stamp = stamp;
    let Some(session) = session_name else {
        return;
    };
    let resolved = resolve_space_label(space_name.as_deref(), session, &spaces_dir());
    if resolved == *space_name {
        return;
    }
    *space_name = resolved;
    apply_resolved_space(
        client,
        space_hinted,
        session,
        space_name,
        cycle_names,
        identity,
        previous,
    );
}

fn idle_revoke_rich_focus(ctx: &mut InputCtx<'_>) -> Result<()> {
    ctx.ensure_controller()?;
    if !*ctx.controller {
        return Ok(());
    }
    *ctx.last_typed = Some(Instant::now());
    apply_rich_focus_toggle(ctx, true)
}

fn idle_forward_escape(ctx: &mut InputCtx<'_>) -> Result<()> {
    ctx.ensure_controller()?;
    if !*ctx.controller {
        return Ok(());
    }
    *ctx.last_typed = Some(Instant::now());
    ctx.pending_utf8.push(0x1b);
    while let Some(chunk) = take_utf8_prefix(ctx.pending_utf8) {
        write_as_controller(ctx, chunk)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_idle_escape(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: Option<&str>,
    got_input: bool,
    pending_keys: &mut Vec<u8>,
    pending_utf8: &mut Vec<u8>,
    scroll: &mut ScrollState,
    previous: &mut Option<PaintFrame>,
    rich_focus_id: &mut Option<u32>,
    structured_focus: &mut bool,
    sync_input: &mut bool,
    controller: &mut bool,
    lease_held: &mut bool,
    last_typed: &mut Option<Instant>,
    read_only: bool,
    held_notice: &mut Option<String>,
    session_switch: &mut Option<SessionSwitch>,
) -> Result<()> {
    if got_input || pending_keys.as_slice() != [0x1b] {
        return Ok(());
    }
    pending_keys.clear();
    if apply_idle_lone_escape(scroll, pending_utf8) {
        *previous = None;
        return Ok(());
    }
    let mut ctx = loop_input_ctx(
        client,
        client_id,
        pane_id,
        window_id,
        session,
        sync_input,
        controller,
        lease_held,
        last_typed,
        pending_keys,
        pending_utf8,
        scroll,
        previous.as_ref(),
        rich_focus_id,
        structured_focus,
        read_only,
        held_notice,
        session_switch,
    );
    if ctx.rich_focus_id.is_some() {
        idle_revoke_rich_focus(&mut ctx)
    } else {
        idle_forward_escape(&mut ctx)
    }
}

fn run_interactive(
    client: &mut Client,
    client_id: u64,
    mut pane_id: u64,
    mut window_id: u64,
    opts: InteractiveOpts,
) -> Result<(AttachEnd, Option<String>, u64)> {
    // Do not grab the lease on attach. InjectMail must AcquireLease
    // on an idle unfocused pane; a sticky attach lease made every live
    // agent pane `deferred_lease`. Acquire on the first key; release after
    // LEASE_IDLE with no input.
    let mut controller = opts.already_controller && !opts.read_only;
    let mut mail_depth = opts.mail_depth;
    let mut attention = opts.attention;
    let mut event_seq = opts.event_seq;
    let mut sync_input = opts.sync_input;
    let mut pane_status = opts.pane_status;
    let read_only = opts.read_only;
    let mut lease_held = opts.lease_held;
    let mut session_name = opts.session_name;
    let space_hinted = opts.space_name.is_some();
    let mut space_name = opts.space_name;
    if space_hinted {
        if let Some(session) = session_name.as_deref() {
            space_name = resolve_space_label(space_name.as_deref(), session, &spaces_dir());
        }
    }
    let mut cycle_names = take_snapshot(client)
        .map(|snapshot| cycle_session_names(space_name.as_deref(), &snapshot))
        .unwrap_or_default();
    let mut identity = space_attach_identity(
        space_hinted,
        under_host(),
        space_name.as_deref(),
        session_name.as_deref(),
        &cycle_names,
    );
    let mut spaces_stamp = spaces_dir_stamp(&spaces_dir());
    let mut spaces_stamp_checked = Instant::now();
    let mut toast_label = attach_toast_text(session_name.as_deref());
    let ro_note = if read_only { " [ro]" } else { "" };
    eprintln!(
        "{}; pane {pane_id}{}{ro_note} — detach: C-\\ d  next: C-\\ n  prev: C-\\ p  jump: C-\\ 1-9  sync: C-\\ s  arrange: C-\\ a  scroll: PageUp / C-\\ [  wheel: history or child  select: drag",
        toast_label.trim(),
        if controller { "" } else { " observer" }
    );
    let mut poll_fallback = poll_fallback_requested();
    if poll_fallback {
        eprintln!("pmux-attach: PRISMATTYC_ATTACH_POLL=1; using 50 ms ReadPaneStyled");
    }
    let mut log_wake = if poll_fallback {
        None
    } else {
        match LogPaintWake::start(opts.socket.clone(), pane_id) {
            Ok(wake) => Some(wake),
            Err(error) => {
                eprintln!(
                    "pmux-attach: pane-log subscribe failed ({error}); using 50 ms ReadPaneStyled"
                );
                None
            }
        }
    };
    let mut toast_until = Instant::now() + ATTACH_TOAST_LINGER;
    let mut toast_was_live = true;
    let raw = RawTerminal::enter()?;
    // Disable wrap before the first paint so a full-width first frame cannot
    // trip the outer terminal.
    {
        let mut out = io::stdout();
        // wheel-only DECSET 7700 + SGR 1006. Do not hold 1000 — that
        // claimed buttons and killed host drag-select in every mux pane.
        let _ = out.write_all(b"\x1b[?1049h\x1b[?7l\x1b[?25l\x1b[?1006h\x1b[?7700h");
        let _ = out.flush();
    }
    if let Some(message) = attention.as_deref() {
        emit_attention(message)?;
    }

    let mut last_size = local_winsize();
    let mut last_sent = None;

    let mut pending_keys = Vec::new();
    let mut pending_utf8 = Vec::new();
    let mut held_notice = None;
    let mut last_outer_full: Option<bool> = None;
    let mut scroll = ScrollState {
        rows: last_size.map(|size| size.rows).unwrap_or(24),
        ..ScrollState::default()
    };
    let mut previous = {
        let frame = read_frame(client, client_id, pane_id, None)?;
        if !frame.content.child_alive {
            drop(raw);
            return Ok((AttachEnd::ChildExited, session_name, pane_id));
        }
        scroll.apply_frame_flags(&frame);
        if let Some(size) = last_size {
            if keep_request_ok(
                "Resize",
                pane_id,
                session_name.as_deref(),
                client.request(|request_id| {
                    resize_request(request_id, window_id, size, client_id, opts.fit, false)
                }),
            )
            .is_some()
            {
                last_sent = Some(size);
            }
        }
        sync_outer_mouse(
            outer_mouse_full(controller, scroll.child_mouse, frame.structured_focus),
            &mut last_outer_full,
        );
        scroll.anchor_to(frame.max_view_scroll);
        let _ = drain_events(
            client,
            pane_id,
            window_id,
            client_id,
            &mut event_seq,
            &mut mail_depth,
            &mut attention,
            &mut sync_input,
            &mut pane_status,
            &mut lease_held,
            &mut controller,
        )?;
        let toast = attach_toast_overlay(
            &toast_label,
            last_size
                .map(|size| size.cols)
                .unwrap_or(frame.content.cols),
        );
        paint(
            None,
            &frame,
            &scroll,
            mail_depth,
            None,
            session_name.as_deref(),
            Some(&toast),
            AttachChrome {
                sync_input,
                lease_held,
                read_only,
                pane_status: pane_status.as_deref(),
                viewport_hint: None,
                identity: identity.as_deref(),
                mail_letter: mail_depth > 0,
                host_nested: under_host(),
            },
            last_size.map(|size| (size.cols, size.rows)),
        )?;
        ack_painted_semantic_copy(client, client_id, pane_id, frame.semantic_clipboard_seq);
        Some(frame)
    };
    let mut rich_focus_id = previous.as_ref().and_then(|frame| frame.rich_focus_id);
    let mut structured_focus = previous
        .as_ref()
        .is_some_and(|frame| frame.structured_focus);
    let mut prev_mail = Some(mail_depth);
    // --write then interactive starts already holding the lease. Seed the
    // idle timer so InjectMail is not blocked until the first key.
    let mut last_typed: Option<Instant> = controller.then(Instant::now);
    let mut session_switch = None;
    #[cfg(unix)]
    let stdin = io::stdin();

    #[cfg(windows)]
    let native_input = windows_terminal::Input::new()?;
    let mut host_focus = prismattyc_mux::attach_focus::Reporter::new(&opts.socket);
    let end = loop {
        if host_focus.update(pane_id) {
            break AttachEnd::Detached;
        }
        #[cfg(unix)]
        let (ready, wake_hup) = {
            let mut fds = vec![PollFd::new(&stdin, PollFlags::IN | PollFlags::HUP)];
            if let Some(wake) = log_wake.as_ref() {
                fds.push(PollFd::new(&wake.ready, PollFlags::IN | PollFlags::HUP));
            }
            let _ = poll(&mut fds, Some(&POLL_WAIT));
            let ready = fds[0].revents();
            let wake_revents = fds.get(1).map(|fd| fd.revents());
            let wake_hup = wake_revents.is_some_and(|rev| {
                rev.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
            });
            (ready, wake_hup)
        };
        #[cfg(windows)]
        let (ready, wake_hup) = native_input.poll(log_wake.as_ref().map(|w| &w.native_wake))?;
        let (woke, wake_closed) = log_wake
            .as_ref()
            .map(LogPaintWake::drain)
            .unwrap_or((false, false));
        if log_wake.is_some()
            && (wake_hup || wake_closed || log_wake.as_ref().is_some_and(LogPaintWake::is_dead))
        {
            eprintln!("pmux-attach: pane-log subscribe ended; using 50 ms ReadPaneStyled");
            log_wake = None;
            poll_fallback = true;
            previous = None;
        }
        if ready.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
            break AttachEnd::Detached;
        }
        let mut got_input = false;
        if ready.contains(PollFlags::IN) {
            let mut buf = [0u8; 256];
            #[cfg(unix)]
            let read = rustix::io::read(stdin.as_fd(), &mut buf).map_err(io::Error::from);
            #[cfg(windows)]
            let read = native_input.read(&mut buf);
            match read {
                Ok(0) => break AttachEnd::Detached,
                Ok(n) => {
                    got_input = true;
                    let before_scroll = scroll.paint_state();
                    let before_search = scroll.search_paint_state();
                    let before_sync = sync_input;
                    let was_controller = controller;
                    let detached = handle_input(
                        &mut loop_input_ctx(
                            client,
                            client_id,
                            pane_id,
                            window_id,
                            session_name.as_deref(),
                            &mut sync_input,
                            &mut controller,
                            &mut lease_held,
                            &mut last_typed,
                            &mut pending_keys,
                            &mut pending_utf8,
                            &mut scroll,
                            previous.as_ref(),
                            &mut rich_focus_id,
                            &mut structured_focus,
                            read_only,
                            &mut held_notice,
                            &mut session_switch,
                        ),
                        &buf[..n],
                    )?;
                    if detached {
                        break AttachEnd::Detached;
                    }
                    if let Some(switch) = session_switch.take() {
                        if let Some(target) =
                            target_session_name(session_name.as_deref(), &cycle_names, switch)
                        {
                            if space_hinted {
                                space_name = resolve_space_label(
                                    space_name.as_deref(),
                                    &target,
                                    &spaces_dir(),
                                );
                            }
                            match retarget_interactive(
                                client,
                                client_id,
                                &mut pane_id,
                                &mut window_id,
                                &mut session_name,
                                &mut controller,
                                &mut last_typed,
                                &mut mail_depth,
                                &mut attention,
                                &mut pane_status,
                                &mut lease_held,
                                &mut sync_input,
                                &mut event_seq,
                                &mut cycle_names,
                                space_name.as_deref(),
                                &target,
                            ) {
                                Ok(()) => {
                                    toast_label = attach_toast_text(session_name.as_deref());
                                    toast_until = Instant::now() + ATTACH_TOAST_LINGER;
                                    identity = space_attach_identity(
                                        space_hinted,
                                        under_host(),
                                        space_name.as_deref(),
                                        session_name.as_deref(),
                                        &cycle_names,
                                    );
                                    scroll.leave();
                                    previous = None;
                                    if !poll_fallback {
                                        log_wake =
                                            LogPaintWake::start(opts.socket.clone(), pane_id).ok();
                                    }
                                }
                                Err(error) => {
                                    toast_label = format!(" {error} ");
                                    toast_until = Instant::now() + ATTACH_TOAST_LINGER;
                                    previous = None;
                                }
                            }
                            if !pending_keys.is_empty() || !pending_utf8.is_empty() {
                                let detached = handle_input(
                                    &mut loop_input_ctx(
                                        client,
                                        client_id,
                                        pane_id,
                                        window_id,
                                        session_name.as_deref(),
                                        &mut sync_input,
                                        &mut controller,
                                        &mut lease_held,
                                        &mut last_typed,
                                        &mut pending_keys,
                                        &mut pending_utf8,
                                        &mut scroll,
                                        previous.as_ref(),
                                        &mut rich_focus_id,
                                        &mut structured_focus,
                                        read_only,
                                        &mut held_notice,
                                        &mut session_switch,
                                    ),
                                    &[],
                                )?;
                                if detached {
                                    break AttachEnd::Detached;
                                }
                            }
                        }
                    }
                    if let Some(notice) = held_notice.take() {
                        toast_label = format!(" {notice} ");
                        toast_until = Instant::now() + ATTACH_TOAST_LINGER;
                        previous = None;
                    }
                    // Local scroll/copy state changes are host-owned and must
                    // repaint even when the server revision is unchanged.
                    if !was_controller && controller {
                        if let Some(size) = last_size {
                            if keep_request_ok(
                                "Resize",
                                pane_id,
                                session_name.as_deref(),
                                client.request(|request_id| {
                                    resize_request(
                                        request_id, window_id, size, client_id, false, false,
                                    )
                                }),
                            )
                            .is_some()
                            {
                                last_sent = Some(size);
                            }
                        }
                    }
                    if before_scroll != scroll.paint_state()
                        || before_search != scroll.search_paint_state()
                        || before_sync != sync_input
                    {
                        previous = None;
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(_) => break AttachEnd::Detached,
            }
        }
        handle_idle_escape(
            client,
            client_id,
            pane_id,
            window_id,
            session_name.as_deref(),
            got_input,
            &mut pending_keys,
            &mut pending_utf8,
            &mut scroll,
            &mut previous,
            &mut rich_focus_id,
            &mut structured_focus,
            &mut sync_input,
            &mut controller,
            &mut lease_held,
            &mut last_typed,
            read_only,
            &mut held_notice,
            &mut session_switch,
        )?;

        maybe_idle_release_lease(
            client,
            client_id,
            pane_id,
            session_name.as_deref(),
            &mut controller,
            got_input,
            &pending_utf8,
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );

        apply_local_winsize(
            client,
            client_id,
            pane_id,
            window_id,
            session_name.as_deref(),
            &mut last_size,
            &mut last_sent,
            &mut previous,
            &mut scroll,
        );

        let event_changed = drain_events(
            client,
            pane_id,
            window_id,
            client_id,
            &mut event_seq,
            &mut mail_depth,
            &mut attention,
            &mut sync_input,
            &mut pane_status,
            &mut lease_held,
            &mut controller,
        )?;
        if event_changed && (attention.is_none() || mail_depth == 0) {
            // Overlay cells must come back from the guest grid.
            previous = None;
        }
        let toast_live = Instant::now() < toast_until;
        if toast_was_live && !toast_live {
            previous = None;
        }
        toast_was_live = toast_live;

        refresh_space_identity(
            client,
            space_hinted,
            &mut spaces_stamp_checked,
            &mut spaces_stamp,
            session_name.as_deref(),
            &mut space_name,
            &mut cycle_names,
            &mut identity,
            &mut previous,
        );

        let poll_all = poll_fallback || log_wake.is_none();
        let need_frame = poll_all || previous.is_none() || woke || event_changed;
        if !need_frame {
            continue;
        }

        let request_offset = if scroll.supported || scroll.active {
            Some(scroll.offset)
        } else {
            None
        };
        let frame = read_frame(client, client_id, pane_id, request_offset)?;
        if !frame.content.child_alive {
            cycle_names.retain(|name| Some(name.as_str()) != session_name.as_deref());
            if let Some(target) =
                target_session_name(session_name.as_deref(), &cycle_names, SessionSwitch::Next)
            {
                toast_label = " session ended; skipping ".to_string();
                toast_until = Instant::now() + ATTACH_TOAST_LINGER;
                if space_hinted {
                    space_name = resolve_space_label(space_name.as_deref(), &target, &spaces_dir());
                }
                let _ = retarget_interactive(
                    client,
                    client_id,
                    &mut pane_id,
                    &mut window_id,
                    &mut session_name,
                    &mut controller,
                    &mut last_typed,
                    &mut mail_depth,
                    &mut attention,
                    &mut pane_status,
                    &mut lease_held,
                    &mut sync_input,
                    &mut event_seq,
                    &mut cycle_names,
                    space_name.as_deref(),
                    &target,
                );
                identity = space_attach_identity(
                    space_hinted,
                    under_host(),
                    space_name.as_deref(),
                    session_name.as_deref(),
                    &cycle_names,
                );
                previous = None;
                if !poll_fallback {
                    log_wake = LogPaintWake::start(opts.socket.clone(), pane_id).ok();
                }
                continue;
            }
            break AttachEnd::ChildExited;
        }
        scroll.apply_frame_flags(&frame);
        sync_outer_mouse(
            outer_mouse_full(controller, scroll.child_mouse, frame.structured_focus),
            &mut last_outer_full,
        );
        let prior_offset = scroll.offset;
        let prior_active = scroll.active;
        scroll.anchor_to(frame.max_view_scroll);
        let offset_changed = prior_offset != scroll.offset || prior_active != scroll.active;
        // Re-read if anchoring moved the window so we paint the stable view.
        let mut frame = if offset_changed && scroll.active {
            let frame = read_frame(client, client_id, pane_id, Some(scroll.offset))?;
            scroll.apply_frame_flags(&frame);
            frame
        } else {
            frame
        };
        if scroll.take_copy_yank_request() {
            let text = copy_selection_text(&frame, &scroll);
            scroll.pending_clipboard = Some(text);
            scroll.leave();
            previous = None;
            frame = read_frame(client, client_id, pane_id, None)?;
            scroll.apply_frame_flags(&frame);
        }
        if event_changed
            || previous.as_ref().is_none_or(|prior| {
                prior.content.revision != frame.content.revision
                    || prior.view_offset != frame.view_offset
                    || offset_changed
            })
        {
            if host_focus.update(pane_id) {
                break AttachEnd::Detached;
            }
            let toast = toast_live.then(|| {
                attach_toast_overlay(
                    &toast_label,
                    last_size
                        .map(|size| size.cols)
                        .unwrap_or(frame.content.cols),
                )
            });
            paint(
                previous.as_ref(),
                &frame,
                &scroll,
                mail_depth,
                prev_mail,
                session_name.as_deref(),
                toast.as_ref(),
                AttachChrome {
                    sync_input,
                    lease_held,
                    read_only,
                    pane_status: pane_status.as_deref(),
                    viewport_hint: None,
                    identity: identity.as_deref(),
                    mail_letter: mail_depth > 0,
                    host_nested: under_host(),
                },
                last_size.map(|size| (size.cols, size.rows)),
            )?;
            scroll.clear_pending_clipboard();
            ack_painted_semantic_copy(client, client_id, pane_id, frame.semantic_clipboard_seq);
            previous = Some(frame);
            prev_mail = Some(mail_depth);
            rich_focus_id = previous.as_ref().and_then(|frame| frame.rich_focus_id);
            structured_focus = previous
                .as_ref()
                .is_some_and(|frame| frame.structured_focus);
        }
    };

    if controller {
        let _ = keep_request_ok(
            "ReleaseLease",
            pane_id,
            session_name.as_deref(),
            client.request(|request_id| ControlRequest::ReleaseLease {
                version: PROTOCOL_VERSION,
                request_id,
                client_id,
                pane_id,
            }),
        );
    }
    drop(log_wake);
    drop(raw);
    flush_read_frame_count();
    Ok((end, session_name, pane_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavKey {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachKey {
    Detach,
    ToggleSyncInput,
    Fit,
    NextSession,
    PrevSession,
    JumpSession(u8),
    EnterScroll,
    /// C-\ a: cycle named arrangements (PT-132).
    Arrange,
    Nav {
        kind: NavKey,
        bytes: Vec<u8>,
        modifiers: u8,
    },
    LeaveScroll {
        bytes: Vec<u8>,
    },
    Wheel {
        up: bool,
        x: u32,
        y: u32,
        shift: bool,
    },
    /// Non-wheel SGR mouse (click/drag/release). Forwarded only when the pane
    /// child has application mouse on (AC#3).
    MouseReport {
        bytes: Vec<u8>,
        shift: bool,
        x: u32,
        y: u32,
        primary: bool,
        phase: RichPointerPhase,
    },
    /// Incomplete or junk mouse prefix — drop.
    MouseOther,
    /// Ctrl+Shift+G (xterm 27;6;103~ / kitty 103;6u). Stolen only when rich is on.
    RichFocusToggle {
        bytes: Vec<u8>,
    },
    SemanticCopy {
        bytes: Vec<u8>,
    },
    Forward(Vec<u8>),
}

/// Drain complete keys from `pending`. Incomplete prefixes stay.
fn drain_keys(pending: &mut Vec<u8>) -> Vec<AttachKey> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < pending.len() {
        match pending[i] {
            DETACH_PREFIX => {
                if i + 1 >= pending.len() {
                    break;
                }
                let next = pending[i + 1];
                if next == b'd' || next == b'D' {
                    out.push(AttachKey::Detach);
                } else if next == b's' || next == b'S' {
                    out.push(AttachKey::ToggleSyncInput);
                } else if next == b'z' || next == b'Z' {
                    out.push(AttachKey::Fit);
                } else if next == b'n' || next == b'N' {
                    out.push(AttachKey::NextSession);
                } else if next == b'p' || next == b'P' {
                    out.push(AttachKey::PrevSession);
                } else if (b'1'..=b'9').contains(&next) {
                    out.push(AttachKey::JumpSession(next - b'0'));
                } else if next == b'[' {
                    out.push(AttachKey::EnterScroll);
                } else if next == b'a' || next == b'A' {
                    out.push(AttachKey::Arrange);
                } else {
                    out.push(AttachKey::Forward(vec![DETACH_PREFIX, next]));
                }
                i += 2;
            }
            0x1b => {
                if i + 1 >= pending.len() {
                    break;
                }
                if pending[i + 1] == b'O' {
                    if i + 2 >= pending.len() {
                        break;
                    }
                    let kind = match pending[i + 2] {
                        b'A' => Some(NavKey::Up),
                        b'B' => Some(NavKey::Down),
                        b'C' => Some(NavKey::Right),
                        b'D' => Some(NavKey::Left),
                        b'H' => Some(NavKey::Home),
                        b'F' => Some(NavKey::End),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        out.push(AttachKey::Nav {
                            kind,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                    } else {
                        out.push(AttachKey::Forward(pending[i..i + 3].to_vec()));
                    }
                    i += 3;
                    continue;
                }
                if pending[i + 1] != b'[' {
                    out.push(AttachKey::LeaveScroll { bytes: vec![0x1b] });
                    i += 1;
                    continue;
                }
                if i + 2 >= pending.len() {
                    break;
                }
                let third = pending[i + 2];
                match third {
                    b'A' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::Up,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'B' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::Down,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'C' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::Right,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'D' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::Left,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'H' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::Home,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'F' => {
                        out.push(AttachKey::Nav {
                            kind: NavKey::End,
                            bytes: pending[i..i + 3].to_vec(),
                            modifiers: 0,
                        });
                        i += 3;
                    }
                    b'0'..=b'9' => {
                        if let Some(rel) = pending[i + 3..]
                            .iter()
                            .position(|&b| matches!(b, b'~' | b'A'..=b'Z' | b'a'..=b'z'))
                        {
                            let end = i + 3 + rel;
                            let seq = pending[i..=end].to_vec();
                            if is_rich_focus_toggle_seq(&seq) {
                                out.push(AttachKey::RichFocusToggle { bytes: seq });
                            } else if is_semantic_copy_seq(&seq) {
                                out.push(AttachKey::SemanticCopy { bytes: seq });
                            } else if let Some((kind, modifiers)) = parse_csi_nav(&seq) {
                                out.push(AttachKey::Nav {
                                    kind,
                                    bytes: seq,
                                    modifiers,
                                });
                            } else {
                                out.push(AttachKey::Forward(seq));
                            }
                            i = end + 1;
                        } else if pending[i + 3..]
                            .iter()
                            .all(|&b| b.is_ascii_digit() || b == b';')
                        {
                            break;
                        } else {
                            out.push(AttachKey::Forward(vec![0x1b, b'[', third]));
                            i += 3;
                        }
                    }
                    b'<' => {
                        // SGR mouse report: ESC [ < Pb ; Px ; Py (M|m).
                        if let Some(rel) = pending[i + 3..]
                            .iter()
                            .position(|&b| b == b'M' || b == b'm')
                        {
                            let end = i + 3 + rel;
                            let press = pending[end] == b'M';
                            let params = &pending[i + 3..end];
                            let mut parts = params.split(|&b| b == b';');
                            let button: u32 = parts
                                .next()
                                .and_then(|p| std::str::from_utf8(p).ok())
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(0);
                            let x: u32 = parts
                                .next()
                                .and_then(|p| std::str::from_utf8(p).ok())
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(1);
                            let y: u32 = parts
                                .next()
                                .and_then(|p| std::str::from_utf8(p).ok())
                                .and_then(|p| p.parse().ok())
                                .unwrap_or(1);
                            let shift = button & 4 != 0;
                            // Mask shift(4)/meta(8)/ctrl(16) modifier bits.
                            out.push(match (press, button & !28) {
                                (true, 64) => AttachKey::Wheel {
                                    up: true,
                                    x,
                                    y,
                                    shift,
                                },
                                (true, 65) => AttachKey::Wheel {
                                    up: false,
                                    x,
                                    y,
                                    shift,
                                },
                                _ => AttachKey::MouseReport {
                                    bytes: pending[i..=end].to_vec(),
                                    shift,
                                    x,
                                    y,
                                    primary: matches!(button & !28, 0 | 32),
                                    phase: if !press {
                                        RichPointerPhase::Release
                                    } else if button & 32 != 0 {
                                        RichPointerPhase::Move
                                    } else {
                                        RichPointerPhase::Press
                                    },
                                },
                            });
                            i = end + 1;
                        } else if pending[i + 3..]
                            .iter()
                            .all(|&b| b.is_ascii_digit() || b == b';')
                        {
                            break;
                        } else {
                            out.push(AttachKey::MouseOther);
                            i += 3;
                        }
                    }
                    _ => {
                        out.push(AttachKey::Forward(pending[i..i + 3].to_vec()));
                        i += 3;
                    }
                }
            }
            b'q' | b'Q' => {
                out.push(AttachKey::LeaveScroll {
                    bytes: vec![pending[i]],
                });
                i += 1;
            }
            b => {
                out.push(AttachKey::Forward(vec![b]));
                i += 1;
            }
        }
    }
    pending.drain(..i);
    out
}

fn xterm_modifiers(param: u16) -> u8 {
    let bits = param.saturating_sub(1);
    let mut modifiers = 0u8;
    if bits & 1 != 0 {
        modifiers |= 1; // SHIFT
    }
    if bits & 2 != 0 {
        modifiers |= 4; // ALT
    }
    if bits & 4 != 0 {
        modifiers |= 2; // CONTROL
    }
    modifiers
}

fn parse_csi_nav(seq: &[u8]) -> Option<(NavKey, u8)> {
    let body = seq.strip_prefix(b"\x1b[")?;
    let final_byte = *body.last()?;
    let params = std::str::from_utf8(&body[..body.len().saturating_sub(1)]).ok()?;
    let numbers: Vec<u16> = if params.is_empty() {
        Vec::new()
    } else {
        params
            .split(';')
            .map(|part| {
                if part.is_empty() {
                    0
                } else {
                    part.parse().unwrap_or(0)
                }
            })
            .collect()
    };
    let modifiers = if numbers.len() >= 2 {
        xterm_modifiers(numbers[numbers.len() - 1])
    } else {
        0
    };
    let kind = match (final_byte, numbers.first().copied().unwrap_or(0)) {
        (b'A', _) => NavKey::Up,
        (b'B', _) => NavKey::Down,
        (b'C', _) => NavKey::Right,
        (b'D', _) => NavKey::Left,
        (b'H', _) | (b'~', 1) => NavKey::Home,
        (b'F', _) | (b'~', 4) => NavKey::End,
        (b'~', 5) => NavKey::PageUp,
        (b'~', 6) => NavKey::PageDown,
        _ => return None,
    };
    Some((kind, modifiers))
}

fn is_semantic_copy_seq(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"\x1b[27;6;99~" | b"\x1b[27;6;67~" | b"\x1b[99;6u" | b"\x1b[67;6u"
    )
}

fn ack_painted_semantic_copy(client: &mut Client, client_id: u64, pane_id: u64, seq: Option<u64>) {
    let Some(seq) = seq else {
        return;
    };
    let _ = keep_request_ok(
        "CopySemantic",
        pane_id,
        None,
        client.request(|request_id| ControlRequest::CopySemantic {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            seq: Some(seq),
        }),
    );
}

fn apply_semantic_copy(ctx: &mut InputCtx<'_>) -> Result<()> {
    match ctx
        .client
        .request(|request_id| ControlRequest::CopySemantic {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: ctx.client_id,
            pane_id: ctx.pane_id,
            seq: None,
        }) {
        Ok(ControlResponseData::SemanticCopy {
            text: Some(text), ..
        }) => {
            if let Some(osc) = prismattyc_core::encode_osc52_clipboard(&text) {
                let mut out = io::stdout().lock();
                out.write_all(&osc)?;
                out.flush()?;
            }
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) => Err(error).context("copy semantic text"),
    }
}

fn is_rich_focus_toggle_seq(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"\x1b[27;6;103~" | b"\x1b[27;6;71~" | b"\x1b[103;6u" | b"\x1b[71;6u"
    )
}

struct InputCtx<'a> {
    client: &'a mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: Option<&'a str>,
    sync_input: &'a mut bool,
    controller: &'a mut bool,
    lease_held: &'a mut bool,
    last_typed: &'a mut Option<Instant>,
    pending_keys: &'a mut Vec<u8>,
    pending_utf8: &'a mut Vec<u8>,
    scroll: &'a mut ScrollState,
    experimental_rich: bool,
    rich_focus_id: &'a mut Option<u32>,
    structured_focus: &'a mut bool,
    workspace_rows: u32,
    child_pid: Option<u32>,
    read_only: bool,
    held_notice: &'a mut Option<String>,
    session_switch: &'a mut Option<SessionSwitch>,
}

fn nav_focus_token(kind: NavKey) -> Option<&'static str> {
    match kind {
        NavKey::Up => Some("Up"),
        NavKey::Down => Some("Down"),
        NavKey::Left => Some("Left"),
        NavKey::Right => Some("Right"),
        NavKey::PageUp => Some("PageUp"),
        NavKey::PageDown => Some("PageDown"),
        NavKey::Home => Some("Home"),
        NavKey::End => Some("End"),
    }
}

fn vt_focus_token(data: &[u8]) -> Option<String> {
    match data {
        [b'\r'] | [b'\n'] => Some("Enter".into()),
        [b'\t'] => Some("Tab".into()),
        [0x7f] | [0x08] => Some("Backspace".into()),
        [b] if (0x20..=0x7e).contains(b) => Some(char::from(*b).to_string()),
        _ => None,
    }
}

fn apply_rich_focus_toggle(ctx: &mut InputCtx<'_>, revoke: bool) -> Result<()> {
    match ctx
        .client
        .request(|request_id| ControlRequest::RichFocusToggle {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: ctx.client_id,
            pane_id: ctx.pane_id,
            revoke,
        }) {
        Ok(ControlResponseData::RichFocus {
            granted,
            region_id,
            structured,
            ..
        }) => {
            *ctx.rich_focus_id = if granted { region_id } else { None };
            *ctx.structured_focus = granted && structured;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if control_code(&error) == Some(ControlErrorCode::Backpressure) => {
            thread::sleep(Duration::from_millis(10));
            apply_rich_focus_toggle(ctx, revoke)
        }
        Err(error)
            if absorb_not_controller(
                &error,
                ctx.controller,
                ctx.lease_held,
                ctx.pending_utf8,
                ctx.held_notice,
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn write_focus_key(ctx: &mut InputCtx<'_>, token: &str, modifiers: u8) -> Result<()> {
    if *ctx.structured_focus {
        return send_rich_input(
            ctx,
            RichInputKind::Key {
                key: token.to_string(),
                modifiers,
            },
        );
    }
    let Some(id) = *ctx.rich_focus_id else {
        return Ok(());
    };
    let Ok(bytes) = encode_focus_key(id, token) else {
        return Ok(());
    };
    let Ok(data) = String::from_utf8(bytes) else {
        return Ok(());
    };
    write_as_controller(ctx, data)
}

fn write_as_controller(ctx: &mut InputCtx<'_>, data: String) -> Result<()> {
    let mut retried = false;
    loop {
        match write_pane(ctx.client, ctx.client_id, ctx.pane_id, data.clone()) {
            Ok(()) => return Ok(()),
            Err(error)
                if !retried && control_code(&error) == Some(ControlErrorCode::InputDirty) =>
            {
                // A `pmux send --force` took and released the lease while
                // this attach was blocked in poll: the local flag is stale
                // and the write went lease-free into the forced partial
                // line (PT-140). Re-acquire and retry once so the
                // overlapping key is not dropped.
                retried = true;
                *ctx.controller = false;
                if let Some(holder) = ctx.ensure_controller()? {
                    note_input_blocked(ctx, Some(holder));
                    return Ok(());
                }
                if !*ctx.controller {
                    // Read-only attach: nothing to acquire; drop the key.
                    ctx.pending_utf8.clear();
                    return Ok(());
                }
            }
            Err(error) if control_code(&error) == Some(ControlErrorCode::InputDirty) => {
                // Still dirty after a fresh lease: treat like a lost lease
                // and keep the TTY up.
                *ctx.controller = false;
                ctx.pending_utf8.clear();
                return Ok(());
            }
            Err(error)
                if absorb_not_controller(
                    &error,
                    ctx.controller,
                    ctx.lease_held,
                    ctx.pending_utf8,
                    ctx.held_notice,
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

fn send_rich_input(ctx: &mut InputCtx<'_>, input: RichInputKind) -> Result<()> {
    match ctx.client.request(|request_id| ControlRequest::RichInput {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: ctx.client_id,
        pane_id: ctx.pane_id,
        input,
    }) {
        Ok(ControlResponseData::RichInput { .. }) | Ok(_) => Ok(()),
        Err(error) if control_code(&error) == Some(ControlErrorCode::Backpressure) => {
            thread::sleep(Duration::from_millis(10));
            Err(error)
        }
        Err(error)
            if absorb_not_controller(
                &error,
                ctx.controller,
                ctx.lease_held,
                ctx.pending_utf8,
                ctx.held_notice,
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn ensure_controller(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    controller: &mut bool,
    read_only: bool,
) -> Result<Option<u64>> {
    if read_only {
        return Ok(None);
    }
    if *controller {
        return Ok(None);
    }
    match try_acquire_lease(client, client_id, pane_id)? {
        LeaseAcquire::Acquired => {
            *controller = true;
            Ok(None)
        }
        LeaseAcquire::Held { holder } => Ok(Some(holder)),
    }
}

impl InputCtx<'_> {
    fn ensure_controller(&mut self) -> Result<Option<u64>> {
        ensure_controller(
            self.client,
            self.client_id,
            self.pane_id,
            self.controller,
            self.read_only,
        )
    }
}

fn note_input_blocked(ctx: &mut InputCtx<'_>, holder: Option<u64>) {
    ctx.pending_utf8.clear();
    if let Some(holder) = holder {
        *ctx.lease_held = true;
        *ctx.held_notice = Some(format!("input held by client {holder} — wait or C-\\ d"));
    }
}

fn attach_key_bytes(key: AttachKey) -> Vec<u8> {
    match key {
        AttachKey::Detach => vec![DETACH_PREFIX, b'd'],
        AttachKey::ToggleSyncInput => vec![DETACH_PREFIX, b's'],
        AttachKey::Fit => vec![DETACH_PREFIX, b'z'],
        AttachKey::NextSession => vec![DETACH_PREFIX, b'n'],
        AttachKey::PrevSession => vec![DETACH_PREFIX, b'p'],
        AttachKey::JumpSession(n) => vec![DETACH_PREFIX, b'0' + n],
        AttachKey::EnterScroll => vec![DETACH_PREFIX, b'['],
        AttachKey::Arrange => vec![DETACH_PREFIX, b'a'],
        AttachKey::Nav { bytes, .. }
        | AttachKey::LeaveScroll { bytes }
        | AttachKey::MouseReport { bytes, .. }
        | AttachKey::RichFocusToggle { bytes }
        | AttachKey::SemanticCopy { bytes }
        | AttachKey::Forward(bytes) => bytes,
        AttachKey::Wheel { .. } | AttachKey::MouseOther => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputView {
    read_only: bool,
    copy_mode: bool,
    scroll_active: bool,
    search_prompt: bool,
    experimental_rich: bool,
    structured_focus: bool,
    history_available: bool,
    scroll_supported: bool,
    rich_focus: bool,
    workspace_rows: u32,
    controller: bool,
    child_mouse: Option<bool>,
    alt_active: bool,
    history_max: Option<u32>,
}

impl InputView {
    fn from_ctx(ctx: &InputCtx<'_>) -> Self {
        Self {
            read_only: ctx.read_only,
            copy_mode: ctx.scroll.copy_mode,
            scroll_active: ctx.scroll.active,
            search_prompt: ctx.scroll.search.prompt.is_some(),
            experimental_rich: ctx.experimental_rich,
            structured_focus: *ctx.structured_focus,
            history_available: attach_history_available(ctx.scroll),
            scroll_supported: ctx.scroll.supported,
            rich_focus: ctx.rich_focus_id.is_some(),
            workspace_rows: ctx.workspace_rows,
            controller: *ctx.controller,
            child_mouse: ctx.scroll.child_mouse,
            alt_active: ctx.scroll.alt_active,
            history_max: ctx.scroll.max,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputRoute {
    Detach,
    NextSession,
    PrevSession,
    JumpSession(u8),
    Fit,
    Arrange,
    Skip,
    ToggleSync,
    SemanticCopy,
    RichFocusToggle,
    Write(Vec<u8>),
    EnterScroll,
    StayLive,
    CopyNav(NavKey),
    ScrollNav(NavKey),
    PageUpEnter,
    FocusNav {
        token: &'static str,
        modifiers: u8,
    },
    RichScroll {
        up: bool,
        x: u32,
        y: u32,
    },
    Wheel {
        up: bool,
        x: u32,
        y: u32,
        shift: bool,
        acquire: bool,
    },
    RichPointer {
        x: u32,
        y: u32,
        phase: RichPointerPhase,
    },
    ForwardMouse(Vec<u8>),
    Ignore,
    CancelSearch,
    FeedSearch(Vec<u8>),
    LeaveScroll,
    RichFocusEsc,
    SearchCommit,
    SearchBackspace,
    CopySearchStart(CopySearchDir),
    CopySearchStep {
        reverse: bool,
    },
    CopyToggleSelect,
    CopyYank,
    FocusVt(String),
}

fn route_attach_key(key: &AttachKey, view: &InputView) -> InputRoute {
    match key {
        AttachKey::Detach => InputRoute::Detach,
        AttachKey::NextSession => InputRoute::NextSession,
        AttachKey::PrevSession => InputRoute::PrevSession,
        AttachKey::JumpSession(n) => InputRoute::JumpSession(*n),
        AttachKey::Fit => InputRoute::Fit,
        AttachKey::Arrange => {
            if view.read_only || view.copy_mode {
                InputRoute::Skip
            } else {
                InputRoute::Arrange
            }
        }
        AttachKey::ToggleSyncInput => {
            if view.read_only {
                InputRoute::Skip
            } else {
                InputRoute::ToggleSync
            }
        }
        AttachKey::SemanticCopy { .. } => {
            if view.experimental_rich && !view.scroll_active {
                InputRoute::SemanticCopy
            } else {
                InputRoute::Skip
            }
        }
        AttachKey::RichFocusToggle { bytes } => route_rich_focus(view, bytes),
        AttachKey::EnterScroll => route_enter_scroll(view),
        AttachKey::Nav {
            kind,
            bytes,
            modifiers,
        } => route_nav(view, *kind, bytes, *modifiers),
        AttachKey::Wheel { up, x, y, shift } => route_wheel(view, *up, *x, *y, *shift),
        AttachKey::MouseReport {
            x,
            y,
            shift,
            primary,
            phase,
            bytes,
            ..
        } => route_mouse(view, *x, *y, *shift, *primary, *phase, bytes),
        AttachKey::MouseOther => InputRoute::Ignore,
        AttachKey::LeaveScroll { bytes } => route_leave_scroll(view, bytes),
        AttachKey::Forward(data) => route_forward(view, data),
    }
}

fn route_rich_focus(view: &InputView, bytes: &[u8]) -> InputRoute {
    if view.experimental_rich && !view.scroll_active {
        InputRoute::RichFocusToggle
    } else if !view.scroll_active {
        InputRoute::Write(bytes.to_vec())
    } else {
        InputRoute::Skip
    }
}

fn route_enter_scroll(view: &InputView) -> InputRoute {
    if view.history_available {
        InputRoute::EnterScroll
    } else if view.scroll_supported {
        InputRoute::StayLive
    } else if !view.scroll_active {
        InputRoute::Write(vec![DETACH_PREFIX, b'['])
    } else {
        InputRoute::Skip
    }
}

fn route_nav(view: &InputView, kind: NavKey, bytes: &[u8], modifiers: u8) -> InputRoute {
    if view.search_prompt {
        InputRoute::Skip
    } else if view.copy_mode {
        InputRoute::CopyNav(kind)
    } else if view.scroll_active {
        InputRoute::ScrollNav(kind)
    } else if kind == NavKey::PageUp && view.history_available {
        InputRoute::PageUpEnter
    } else if view.rich_focus {
        match nav_focus_token(kind) {
            Some(token) => InputRoute::FocusNav { token, modifiers },
            None => InputRoute::Skip,
        }
    } else {
        InputRoute::Write(bytes.to_vec())
    }
}

fn route_wheel(view: &InputView, up: bool, x: u32, y: u32, shift: bool) -> InputRoute {
    if view.structured_focus
        && !shift
        && !view.scroll_active
        && y > 0
        && y <= view.workspace_rows
        && x > 0
    {
        return InputRoute::RichScroll { up, x, y };
    }
    InputRoute::Wheel {
        up,
        x,
        y,
        shift,
        acquire: wheel_wants_child_view(view, shift),
    }
}

fn wheel_wants_child_view(view: &InputView, shift: bool) -> bool {
    if shift || view.scroll_active {
        return false;
    }
    match view.child_mouse {
        Some(true) => true,
        Some(false) => view.history_max == Some(0) && view.alt_active,
        None => false,
    }
}

fn route_mouse(
    view: &InputView,
    x: u32,
    y: u32,
    shift: bool,
    primary: bool,
    phase: RichPointerPhase,
    bytes: &[u8],
) -> InputRoute {
    if view.structured_focus
        && !shift
        && primary
        && !view.scroll_active
        && y > 0
        && y <= view.workspace_rows
        && x > 0
    {
        InputRoute::RichPointer { x, y, phase }
    } else if view.controller && !view.scroll_active && !shift && view.child_mouse == Some(true) {
        InputRoute::ForwardMouse(bytes.to_vec())
    } else {
        InputRoute::Ignore
    }
}

fn route_leave_scroll(view: &InputView, bytes: &[u8]) -> InputRoute {
    if view.search_prompt {
        if bytes == [0x1b] {
            InputRoute::CancelSearch
        } else {
            InputRoute::FeedSearch(bytes.to_vec())
        }
    } else if view.scroll_active {
        InputRoute::LeaveScroll
    } else if view.rich_focus && bytes == [0x1b] {
        InputRoute::RichFocusEsc
    } else {
        InputRoute::Write(bytes.to_vec())
    }
}

fn route_forward(view: &InputView, data: &[u8]) -> InputRoute {
    if view.search_prompt {
        match data {
            [b'\r'] | [b'\n'] => InputRoute::SearchCommit,
            [0x7f] | [0x08] => InputRoute::SearchBackspace,
            other => InputRoute::FeedSearch(other.to_vec()),
        }
    } else if view.copy_mode {
        match data {
            [b'/'] => InputRoute::CopySearchStart(CopySearchDir::Forward),
            [b'?'] => InputRoute::CopySearchStart(CopySearchDir::Reverse),
            [b'n'] => InputRoute::CopySearchStep { reverse: false },
            [b'N'] => InputRoute::CopySearchStep { reverse: true },
            [b' ', ..] | [b'v' | b'V'] => InputRoute::CopyToggleSelect,
            [b'y' | b'Y'] | [b'\r'] | [b'\n'] => InputRoute::CopyYank,
            _ => InputRoute::Skip,
        }
    } else if !view.scroll_active && view.rich_focus {
        match vt_focus_token(data) {
            Some(token) => InputRoute::FocusVt(token),
            None => InputRoute::Skip,
        }
    } else if !view.scroll_active {
        InputRoute::Write(data.to_vec())
    } else {
        InputRoute::Skip
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputLoop {
    Detach,
    Break,
    Continue,
}

fn stash_session_leftover(
    ctx: &mut InputCtx<'_>,
    rest: &mut impl Iterator<Item = AttachKey>,
    switch: SessionSwitch,
) {
    *ctx.session_switch = Some(switch);
    let leftover: Vec<u8> = rest.by_ref().flat_map(attach_key_bytes).collect();
    ctx.pending_keys.splice(0..0, leftover);
}

fn queue_utf8(ctx: &mut InputCtx<'_>, want_write: &mut bool, bytes: &[u8]) {
    *want_write = true;
    ctx.pending_utf8.extend_from_slice(bytes);
}

fn with_lease(
    ctx: &mut InputCtx<'_>,
    body: impl FnOnce(&mut InputCtx<'_>) -> Result<()>,
) -> Result<()> {
    ctx.ensure_controller()?;
    if !*ctx.controller {
        return Ok(());
    }
    *ctx.last_typed = Some(Instant::now());
    body(ctx)
}

fn apply_session_route(
    ctx: &mut InputCtx<'_>,
    route: &InputRoute,
    rest: &mut impl Iterator<Item = AttachKey>,
) -> InputLoop {
    match route {
        InputRoute::Detach => InputLoop::Detach,
        InputRoute::NextSession => {
            stash_session_leftover(ctx, rest, SessionSwitch::Next);
            InputLoop::Break
        }
        InputRoute::PrevSession => {
            stash_session_leftover(ctx, rest, SessionSwitch::Prev);
            InputLoop::Break
        }
        InputRoute::JumpSession(n) => {
            stash_session_leftover(ctx, rest, SessionSwitch::Jump(*n));
            InputLoop::Break
        }
        _ => InputLoop::Continue,
    }
}

fn apply_fit(ctx: &mut InputCtx<'_>) {
    let Some(size) = local_winsize() else {
        return;
    };
    let _ = keep_request_ok(
        "Resize",
        ctx.pane_id,
        ctx.session,
        ctx.client.request(|request_id| {
            resize_request(request_id, ctx.window_id, size, ctx.client_id, true, false)
        }),
    );
}

fn apply_arrange(ctx: &mut InputCtx<'_>) {
    let kinds = ArrangementWire::ALL;
    let idx = (ctx.scroll.arrange_step as usize) % kinds.len();
    ctx.scroll.arrange_step = ctx.scroll.arrange_step.wrapping_add(1);
    let _ = ctx
        .client
        .request(|request_id| ControlRequest::ApplyArrangement {
            version: PROTOCOL_VERSION,
            request_id,
            window_id: ctx.window_id,
            kind: kinds[idx],
            focused_pane_id: Some(ctx.pane_id),
        });
}

fn apply_toggle_sync(ctx: &mut InputCtx<'_>) -> Result<()> {
    let enabled = !*ctx.sync_input;
    let _ = ctx
        .client
        .request(|request_id| ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: ctx.client_id,
            window_id: ctx.window_id,
            enabled,
        })?;
    *ctx.sync_input = enabled;
    Ok(())
}

fn apply_rich_chrome(ctx: &mut InputCtx<'_>, route: InputRoute) -> Result<()> {
    match route {
        InputRoute::SemanticCopy => {
            ctx.ensure_controller()?;
            if *ctx.controller {
                apply_semantic_copy(ctx)?;
            }
            Ok(())
        }
        InputRoute::RichFocusToggle => with_lease(ctx, |ctx| apply_rich_focus_toggle(ctx, false)),
        _ => Ok(()),
    }
}

fn apply_chrome_fit(ctx: &mut InputCtx<'_>) {
    apply_fit(ctx);
}

fn apply_chrome_route(ctx: &mut InputCtx<'_>, route: InputRoute) -> Result<InputLoop> {
    match route {
        InputRoute::Fit => apply_chrome_fit(ctx),
        InputRoute::Arrange => apply_arrange(ctx),
        InputRoute::ToggleSync => apply_toggle_sync(ctx)?,
        InputRoute::SemanticCopy | InputRoute::RichFocusToggle => apply_rich_chrome(ctx, route)?,
        _ => {}
    }
    Ok(InputLoop::Continue)
}

fn apply_scroll_nav_route(ctx: &mut InputCtx<'_>, route: InputRoute) -> InputLoop {
    match route {
        InputRoute::EnterScroll | InputRoute::PageUpEnter => ctx.scroll.enter_and_page_up(),
        InputRoute::CopyNav(kind) => ctx.scroll.apply_copy_nav(kind),
        InputRoute::ScrollNav(kind) => ctx.scroll.apply_nav(kind),
        InputRoute::LeaveScroll => ctx.scroll.leave(),
        _ => {}
    }
    InputLoop::Continue
}

fn apply_search_prompt_route(ctx: &mut InputCtx<'_>, route: InputRoute) {
    match route {
        InputRoute::CancelSearch => {
            ctx.scroll.cancel_copy_search_prompt();
            ctx.pending_utf8.clear();
        }
        InputRoute::FeedSearch(bytes) => {
            feed_copy_search_bytes(ctx.scroll, ctx.pending_utf8, &bytes);
        }
        InputRoute::SearchCommit => {
            ctx.pending_utf8.clear();
            ctx.scroll.commit_copy_search_prompt();
        }
        InputRoute::SearchBackspace => {
            ctx.pending_utf8.clear();
            ctx.scroll.copy_search_type('\u{8}');
        }
        _ => {}
    }
}

fn apply_copy_keys(ctx: &mut InputCtx<'_>, route: InputRoute) {
    match route {
        InputRoute::CopySearchStart(dir) => ctx.scroll.start_copy_search(dir),
        InputRoute::CopySearchStep { reverse } => ctx.scroll.copy_search_step(reverse, false),
        InputRoute::CopyToggleSelect => ctx.scroll.toggle_copy_selection(),
        InputRoute::CopyYank => ctx.scroll.request_copy_yank(),
        _ => {}
    }
}

fn apply_copy_edit_route(ctx: &mut InputCtx<'_>, route: InputRoute) -> Result<InputLoop> {
    match route {
        InputRoute::RichFocusEsc => {
            with_lease(ctx, |ctx| apply_rich_focus_toggle(ctx, true))?;
        }
        InputRoute::CopySearchStart(_)
        | InputRoute::CopySearchStep { .. }
        | InputRoute::CopyToggleSelect
        | InputRoute::CopyYank => apply_copy_keys(ctx, route),
        other => apply_search_prompt_route(ctx, other),
    }
    Ok(InputLoop::Continue)
}

fn apply_scroll_copy_route(ctx: &mut InputCtx<'_>, route: InputRoute) -> Result<InputLoop> {
    match route {
        InputRoute::EnterScroll
        | InputRoute::PageUpEnter
        | InputRoute::CopyNav(_)
        | InputRoute::ScrollNav(_)
        | InputRoute::LeaveScroll => Ok(apply_scroll_nav_route(ctx, route)),
        other => apply_copy_edit_route(ctx, other),
    }
}

fn send_rich_scroll(ctx: &mut InputCtx<'_>, up: bool, x: u32, y: u32) -> Result<()> {
    send_rich_input(
        ctx,
        RichInputKind::Scroll {
            row: u16::try_from(y - 1).unwrap_or(u16::MAX),
            col: u16::try_from(x - 1).unwrap_or(u16::MAX),
            delta: if up {
                -(WHEEL_LINES as i16)
            } else {
                WHEEL_LINES as i16
            },
        },
    )
}

fn send_rich_pointer(
    ctx: &mut InputCtx<'_>,
    x: u32,
    y: u32,
    phase: RichPointerPhase,
) -> Result<()> {
    send_rich_input(
        ctx,
        RichInputKind::Pointer {
            phase,
            row: u16::try_from(y - 1).unwrap_or(u16::MAX),
            col: u16::try_from(x - 1).unwrap_or(u16::MAX),
        },
    )
}

fn apply_wheel_route(
    ctx: &mut InputCtx<'_>,
    up: bool,
    x: u32,
    y: u32,
    shift: bool,
    acquire: bool,
    want_write: &mut bool,
) -> Result<()> {
    if acquire {
        ctx.ensure_controller()?;
    }
    match decide_wheel(WheelCtx {
        up,
        shift,
        controller: *ctx.controller,
        scroll_active: ctx.scroll.active,
        scroll_supported: ctx.scroll.supported,
        max: ctx.scroll.max,
        child_mouse: ctx.scroll.child_mouse,
        child_sgr: ctx.scroll.child_sgr,
        alt_active: ctx.scroll.alt_active,
        x,
        y,
    }) {
        WheelDecision::HostScrollUp => ctx.scroll.wheel_up(),
        WheelDecision::HostScrollDown => ctx.scroll.wheel_down(),
        WheelDecision::Forward(bytes) => queue_utf8(ctx, want_write, &bytes),
        WheelDecision::Ignore => {}
    }
    Ok(())
}

fn apply_rich_key_route(ctx: &mut InputCtx<'_>, route: InputRoute) -> Result<()> {
    match route {
        InputRoute::FocusNav { token, modifiers } => {
            with_lease(ctx, |ctx| write_focus_key(ctx, token, modifiers))
        }
        InputRoute::FocusVt(token) => with_lease(ctx, |ctx| write_focus_key(ctx, &token, 0)),
        InputRoute::RichScroll { up, x, y } => {
            with_lease(ctx, |ctx| send_rich_scroll(ctx, up, x, y))
        }
        InputRoute::RichPointer { x, y, phase } => {
            with_lease(ctx, |ctx| send_rich_pointer(ctx, x, y, phase))
        }
        _ => Ok(()),
    }
}

fn apply_write_pointer_route(
    ctx: &mut InputCtx<'_>,
    route: InputRoute,
    want_write: &mut bool,
) -> Result<InputLoop> {
    match route {
        InputRoute::Wheel {
            up,
            x,
            y,
            shift,
            acquire,
        } => apply_wheel_route(ctx, up, x, y, shift, acquire, want_write)?,
        InputRoute::ForwardMouse(bytes) | InputRoute::Write(bytes) => {
            queue_utf8(ctx, want_write, &bytes);
        }
        other => apply_rich_key_route(ctx, other)?,
    }
    Ok(InputLoop::Continue)
}

fn apply_input_route(
    ctx: &mut InputCtx<'_>,
    route: InputRoute,
    rest: &mut impl Iterator<Item = AttachKey>,
    want_write: &mut bool,
) -> Result<InputLoop> {
    match route {
        InputRoute::Detach
        | InputRoute::NextSession
        | InputRoute::PrevSession
        | InputRoute::JumpSession(_) => Ok(apply_session_route(ctx, &route, rest)),
        InputRoute::Fit
        | InputRoute::Arrange
        | InputRoute::Skip
        | InputRoute::ToggleSync
        | InputRoute::SemanticCopy
        | InputRoute::RichFocusToggle
        | InputRoute::Ignore
        | InputRoute::StayLive => apply_chrome_route(ctx, route),
        InputRoute::EnterScroll
        | InputRoute::CopyNav(_)
        | InputRoute::ScrollNav(_)
        | InputRoute::PageUpEnter
        | InputRoute::CancelSearch
        | InputRoute::FeedSearch(_)
        | InputRoute::LeaveScroll
        | InputRoute::RichFocusEsc
        | InputRoute::SearchCommit
        | InputRoute::SearchBackspace
        | InputRoute::CopySearchStart(_)
        | InputRoute::CopySearchStep { .. }
        | InputRoute::CopyToggleSelect
        | InputRoute::CopyYank => apply_scroll_copy_route(ctx, route),
        other => apply_write_pointer_route(ctx, other, want_write),
    }
}

fn flush_pending_write(ctx: &mut InputCtx<'_>, want_write: bool) -> Result<()> {
    if !want_write && ctx.pending_utf8.is_empty() {
        return Ok(());
    }
    if ctx.read_only {
        ctx.pending_utf8.clear();
        return Ok(());
    }
    if let Some(holder) = ctx.ensure_controller()? {
        note_input_blocked(ctx, Some(holder));
        return Ok(());
    }
    if !*ctx.controller {
        ctx.pending_utf8.clear();
        return Ok(());
    }
    *ctx.last_typed = Some(Instant::now());
    while let Some(chunk) = take_utf8_prefix(ctx.pending_utf8) {
        let chunk = expand_empty_bracketed_paste(&chunk, ctx.child_pid).unwrap_or(chunk);
        write_as_controller(ctx, chunk)?;
    }
    Ok(())
}

fn handle_input(ctx: &mut InputCtx<'_>, bytes: &[u8]) -> Result<bool> {
    ctx.pending_keys.extend_from_slice(bytes);
    let mut want_write = false;
    let keys = drain_keys(ctx.pending_keys);
    let mut rest = keys.into_iter();
    while let Some(key) = rest.next() {
        let route = route_attach_key(&key, &InputView::from_ctx(ctx));
        match apply_input_route(ctx, route, &mut rest, &mut want_write)? {
            InputLoop::Detach => return Ok(true),
            InputLoop::Break => break,
            InputLoop::Continue => {}
        }
    }
    flush_pending_write(ctx, want_write)?;
    Ok(false)
}

// --json / --styled-json --watch stay on a 50 ms ReadPane poll. Event-driven
// SubscribePane paint is the TTY attach path only (PT-113).
fn dump_json(client: &mut Client, client_id: u64, pane_id: u64, watch: bool) -> Result<()> {
    let mut previous_revision = None;
    loop {
        let content = read_pane(client, client_id, pane_id)?;
        if previous_revision != Some(content.revision) {
            println!("{}", serde_json::to_string(&content)?);
            previous_revision = Some(content.revision);
        }
        if !watch {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn dump_styled_json(client: &mut Client, client_id: u64, pane_id: u64, watch: bool) -> Result<()> {
    let mut previous = None;
    loop {
        let response = client.request(|request_id| ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            view_offset: None,
        })?;
        let ControlResponseData::PaneStyled { content } = response else {
            bail!("server returned an unexpected styled-pane response");
        };
        let encoded = serde_json::to_string(&content)?;
        if previous.as_deref() != Some(encoded.as_str()) {
            println!("{encoded}");
            previous = Some(encoded);
        }
        if !watch {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn main() -> Result<()> {
    prismattyc_mux::release_update::forward_installed("pmux-attach")?;
    let cli = Cli::parse(std::env::args().skip(1))?;
    diagnose_socket(&cli.socket)?;
    let mut client = Client::connect(&cli.socket)?;
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        bail!("server returned an unexpected registration response");
    };

    if drain_probe_requested() {
        run_drain_probe(&mut client)?;
        return Ok(());
    }

    if let Some(name) = cli.create_session.as_deref() {
        let created = client.request(|request_id| ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id,
            name: name.to_string(),
            spawn: default_create_spawn(),
            cols: None,
            rows: None,
            agent_id: None,
            headless: false,
        })?;
        let ControlResponseData::Session { session_id, .. } = created else {
            bail!("server returned an unexpected create-session response");
        };
        client.request(|request_id| ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            session_id,
        })?;
    }

    let snapshot_response = client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    })?;
    let ControlResponseData::Snapshot { snapshot } = snapshot_response else {
        bail!("server returned an unexpected snapshot response");
    };

    if let Some(key) = cli.session.as_deref() {
        let session_id = snapshot
            .sessions
            .iter()
            .find(|session| session.name == key || session.id.to_string() == key)
            .map(|session| session.id)
            .with_context(|| format!("no session matching {key:?}"))?;
        client.request(|request_id| ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            session_id,
        })?;
    }

    if cli.session_id.is_some() && cli.session.is_some() {
        bail!("--session-id cannot be combined with --session");
    }
    let pane_id = if let Some(pane) = cli.pane {
        pane
    } else if let Some(session_id) = cli.session_id {
        session_pane_by_id(&snapshot, session_id)?
    } else if let Some(key) = cli.session.as_deref().or(cli.create_session.as_deref()) {
        session_pane(&snapshot, key)?
    } else {
        first_pane(&snapshot).context("snapshot contains no pane to attach")?
    };

    let mut wrote = false;
    if let Some(data) = cli.write.clone() {
        client.request(|request_id| ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        })?;
        write_pane(&mut client, client_id, pane_id, data)?;
        wrote = true;
    }

    let interactive =
        attach_wants_interactive(cli.json, cli.styled_json, stdin_is_tty(), stdout_is_tty());
    if interactive && try_host_route_after_resolve(&cli, &snapshot, pane_id)? {
        return Ok(());
    }
    if interactive {
        let window_id = pane_window(&snapshot, pane_id)
            .context("selected pane has no window in the snapshot")?;
        let session_name = pane_session_name(&snapshot, pane_id).map(str::to_string);
        match run_interactive(
            &mut client,
            client_id,
            pane_id,
            window_id,
            InteractiveOpts {
                socket: cli.socket.clone(),
                already_controller: wrote,
                mail_depth: pane_mail_depth(&snapshot, pane_id),
                attention: pane_attention(&snapshot, pane_id),
                sync_input: pane_sync_input(&snapshot, pane_id).unwrap_or(false),
                pane_status: pane_guest_status(&snapshot, pane_id),
                event_seq: snapshot.sequence,
                session_name: session_name.clone(),
                space_name: cli.space.clone(),
                read_only: cli.read_only,
                lease_held: pane_controller_id(&snapshot, pane_id)
                    .is_some_and(|id| id != client_id),
                fit: cli.fit,
            },
        )? {
            (AttachEnd::Detached, name, pane) => match name.as_deref() {
                Some(name) => {
                    eprintln!("[detached] session {name}; pane {pane} still running")
                }
                None => eprintln!("[detached] pane {pane} still running"),
            },
            (AttachEnd::ChildExited, name, pane) => {
                eprintln!("{}", session_ended_line(pane, name.as_deref()))
            }
        }
        return Ok(());
    }

    if cli.styled_json {
        dump_styled_json(&mut client, client_id, pane_id, cli.watch)
    } else {
        dump_json(&mut client, client_id, pane_id, cli.watch)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_attach_toast, apply_arrange, apply_attention_event, apply_chrome_route,
        apply_controller_owner, apply_copy_keys, apply_fit, apply_idle_lone_escape,
        apply_local_winsize, apply_lone_escape, apply_mail_event, apply_observed_winsize,
        apply_resolved_space, apply_rich_chrome, apply_rich_key_route, apply_scroll_nav_route,
        apply_search_prompt_route, apply_session_route, apply_wheel_route, attach_identity_line,
        attach_is_dump_or_write, attach_toast_overlay, attach_toast_text, attach_wants_interactive,
        attention_osc_bytes, cell_px_from_window, clip_frame_to_terminal, compose_paint,
        copy_cell_selected, copy_search_rank, copy_selection_text, dirty_line_indices, drain_keys,
        flush_pending_write, handle_idle_escape, handle_input, host_nested_scrollbar_thumb,
        host_route_session_after_resolve, idle_forward_escape, idle_revoke_rich_focus,
        keep_request_ok, mail_letter_cells, mail_letter_overlay_bytes, maybe_idle_release_lease,
        normalize_winsize, paint_bytes, pane_session_id, parse_focus_border_rgb,
        refresh_space_identity, request_err_line, resize_needs_send, resolve_space_label,
        route_attach_key, route_leave_scroll, run_drain_probe, scroll_copy_suffix,
        send_rich_pointer, send_rich_scroll, should_host_route_cli, space_attach_identity,
        space_stamp_due, spaces_dir_stamp, target_session_name, try_host_route_after_resolve,
        viewport_hint, wheel_wants_child_view, with_lease, AttachChrome, AttachKey, Cli, Client,
        CopyPoint, CopySearch, CopySearchDir, InjectedWinsize, InputCtx, InputLoop, InputRoute,
        InputView, LocalWinsize, LogPaintWake, NavKey, PaintFrame, ScrollState, SessionSwitch,
        DEFAULT_TOAST_FOCUS_RGB, DETACH_PREFIX, LEASE_IDLE, MAIL_CELL_COLS, MAIL_LETTER_RGB,
        REQUEST_ERRS, SPACE_STAMP_INTERVAL, WHEEL_LINES,
    };
    use prismattyc_mux::should_host_route_seat;
    use std::{
        path::PathBuf,
        time::{Duration, Instant, SystemTime},
    };

    #[test]
    fn interactive_attach_requires_stdin_and_stdout_ttys() {
        assert!(attach_wants_interactive(false, false, true, true));
        assert!(
            !attach_wants_interactive(false, false, true, false),
            "piped stdout must not enter raw mode"
        );
        assert!(!attach_wants_interactive(false, false, false, true));
        assert!(!attach_wants_interactive(true, false, true, true));
        assert!(!attach_wants_interactive(false, true, true, true));
    }

    #[test]
    fn session_ended_line_names_the_session() {
        assert_eq!(
            super::session_ended_line(3, Some("work")),
            "session ended: work (pane 3 child exited)"
        );
        assert!(super::session_ended_line(1, None).contains("session ended"));
    }

    #[test]
    fn styled_json_is_explicit_and_mutually_exclusive_with_plain_json() {
        let cli = Cli::parse(
            ["--socket", "/tmp/prism-test.sock", "--styled-json"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert!(cli.styled_json);
        assert!(!cli.json);
        let cli = Cli::parse(
            [
                "--socket",
                "/tmp/prism-test.sock",
                "--space",
                "today",
                "--session",
                "alpha",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert_eq!(cli.space.as_deref(), Some("today"));
        assert_eq!(cli.session.as_deref(), Some("alpha"));
        assert!(Cli::parse(
            [
                "--socket",
                "/tmp/prism-test.sock",
                "--json",
                "--styled-json",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .is_err());
        let interactive = Cli::parse(
            ["--socket", "/tmp/prism-test.sock", "--session", "astra-pc"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert!(!attach_is_dump_or_write(&interactive));
        let json = Cli::parse(
            [
                "--socket",
                "/tmp/prism-test.sock",
                "--session",
                "astra-pc",
                "--json",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        assert!(attach_is_dump_or_write(&json));
        assert!(!should_host_route_seat(
            true,
            false,
            attach_is_dump_or_write(&json)
        ));
        assert!(should_host_route_seat(
            true,
            false,
            attach_is_dump_or_write(&interactive)
        ));
    }

    fn parse_attach_cli(extra: &[&str]) -> Cli {
        let mut args = vec!["--socket", "/tmp/prism-test.sock"];
        args.extend_from_slice(extra);
        Cli::parse(args.into_iter().map(str::to_string)).unwrap()
    }

    #[test]
    fn attach_is_dump_or_write_tables_each_flag() {
        let cases = [
            (&[][..], false),
            (&["--session", "astra-pc"][..], false),
            (&["--json"][..], true),
            (&["--styled-json"][..], true),
            (&["--watch"][..], true),
            (&["--write", "hi"][..], true),
            (&["--pane", "42"][..], true),
            (&["--read-only"][..], true),
            (&["--fit"][..], true),
        ];
        for (extra, dump) in cases {
            assert_eq!(
                attach_is_dump_or_write(&parse_attach_cli(extra)),
                dump,
                "{extra:?}"
            );
        }
    }

    fn empty_ledger() -> prismattyc_mux::PaneInputLedger {
        prismattyc_mux::PaneInputLedger {
            last_output_at_ms: None,
            focused: false,
            controller_id: None,
            last_controller_write_at_ms: None,
            last_write_ended_with_cr: false,
            dirty_input: false,
            last_input_at_ms: None,
        }
    }

    fn pane_snapshot(
        session_id: u64,
        name: &str,
        window_id: u64,
        pane_id: u64,
    ) -> prismattyc_mux::Snapshot {
        use prismattyc_mux::{
            LayoutSnapshot, PaneGeometry, PaneSnapshot, SessionSnapshot, WindowBounds,
            WindowSnapshot,
        };
        prismattyc_mux::Snapshot {
            sequence: 1,
            sessions: vec![SessionSnapshot {
                space_id: None,
                id: session_id,
                name: name.into(),
                agent_id: None,
                windows: vec![WindowSnapshot {
                    id: window_id,
                    title: "main".into(),
                    bounds: WindowBounds {
                        window_id,
                        cols: 80,
                        rows: 24,
                    },
                    layout: LayoutSnapshot::Leaf { pane_id },
                    panes: vec![PaneSnapshot {
                        pane_write: None,
                        id: pane_id,
                        title: name.into(),
                        title_pinned: false,
                        controller_id: None,
                        geometry: PaneGeometry {
                            pane_id,
                            col: 0,
                            row: 0,
                            cols: 80,
                            rows: 24,
                        },
                        spawn: None,
                        child_pid: None,
                        mail: None,
                        status: None,
                        attention: None,
                        mail_inject: None,
                        ledger: empty_ledger(),
                        size_owner: None,
                    }],
                    sync_input: false,
                }],
            }],
        }
    }

    fn two_session_snapshot() -> prismattyc_mux::Snapshot {
        let mut snapshot = pane_snapshot(7, "astra-pc", 99, 42);
        let other = pane_snapshot(8, "other", 100, 43);
        snapshot.sessions.extend(other.sessions);
        snapshot
    }

    #[test]
    fn pane_session_id_maps_a_pane_to_its_session() {
        let snapshot = two_session_snapshot();
        assert_eq!(pane_session_id(&snapshot, 42), Some(7));
        assert_eq!(pane_session_id(&snapshot, 43), Some(8));
        assert_eq!(pane_session_id(&snapshot, 99), None);
        assert_eq!(pane_session_id(&snapshot, 0), None);
        assert_eq!(pane_session_id(&snapshot, 1), None);
    }

    #[test]
    fn host_route_session_after_resolve_requires_route_and_id() {
        let cases = [
            (false, Some(7), None),
            (true, None, None),
            (false, None, None),
            (true, Some(7), Some(7)),
            (true, Some(8), Some(8)),
        ];
        for (should_route, session_id, expected) in cases {
            assert_eq!(
                host_route_session_after_resolve(should_route, session_id),
                expected,
                "should_route={should_route} session_id={session_id:?}"
            );
        }
    }

    static HOST_ROUTE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_host_route_env(host: Option<&str>, pty: Option<&str>, body: impl FnOnce()) {
        let _lock = HOST_ROUTE_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let old_host = std::env::var_os("PRISMATTYC_HOST");
        let old_pty = std::env::var_os("PRISMATTYC_ATTACH_PTY");
        match host {
            Some(value) => std::env::set_var("PRISMATTYC_HOST", value),
            None => std::env::remove_var("PRISMATTYC_HOST"),
        }
        match pty {
            Some(value) => std::env::set_var("PRISMATTYC_ATTACH_PTY", value),
            None => std::env::remove_var("PRISMATTYC_ATTACH_PTY"),
        }
        body();
        match old_host {
            Some(value) => std::env::set_var("PRISMATTYC_HOST", value),
            None => std::env::remove_var("PRISMATTYC_HOST"),
        }
        match old_pty {
            Some(value) => std::env::set_var("PRISMATTYC_ATTACH_PTY", value),
            None => std::env::remove_var("PRISMATTYC_ATTACH_PTY"),
        }
    }

    #[test]
    fn should_host_route_cli_follows_host_pty_and_dump_flags() {
        let interactive = parse_attach_cli(&["--session", "astra-pc"]);
        let json = parse_attach_cli(&["--session", "astra-pc", "--json"]);
        with_host_route_env(None, None, || {
            assert!(!should_host_route_cli(&interactive));
        });
        with_host_route_env(Some("1"), None, || {
            assert!(should_host_route_cli(&interactive));
            assert!(!should_host_route_cli(&json));
        });
        with_host_route_env(Some("1"), Some("1"), || {
            assert!(!should_host_route_cli(&interactive));
        });
        with_host_route_env(Some("true"), None, || {
            assert!(should_host_route_cli(&interactive));
        });
    }

    fn write_ack_when_cache_appears(socket: &std::path::Path) {
        let cache = prismattyc_mux::attach_tabs::layout_path_from_socket(socket);
        let ack = prismattyc_mux::host_ack_path_from_socket(socket);
        std::thread::spawn(move || {
            for _ in 0..80 {
                if cache.exists() {
                    let _ = prismattyc_mux::touch_host_ack(&ack);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    #[test]
    fn try_host_route_after_resolve_routes_only_a_host_seat() {
        let dir = std::env::temp_dir().join(format!(
            "pmux-attach-host-route-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("pmux.sock");
        let mut interactive = parse_attach_cli(&["--session", "astra-pc"]);
        interactive.socket = socket.clone();
        let mut json = parse_attach_cli(&["--session", "astra-pc", "--json"]);
        json.socket = socket.clone();
        let snapshot = two_session_snapshot();
        with_host_route_env(Some("1"), None, || {
            assert!(
                !try_host_route_after_resolve(&interactive, &snapshot, 42).unwrap(),
                "no registered host"
            );
            let live = std::process::id();
            prismattyc_mux::register_host_pid(
                &prismattyc_mux::host_pid_path_from_socket(&socket),
                live,
            )
            .unwrap();
            write_ack_when_cache_appears(&socket);
            assert!(
                try_host_route_after_resolve(&interactive, &snapshot, 42).unwrap(),
                "interactive seat under a live host"
            );
            assert!(
                !try_host_route_after_resolve(&json, &snapshot, 42).unwrap(),
                "dump flags must not host-route"
            );
            assert!(
                !try_host_route_after_resolve(&interactive, &snapshot, 1).unwrap(),
                "unknown pane"
            );
            prismattyc_mux::unregister_host_pid(
                &prismattyc_mux::host_pid_path_from_socket(&socket),
                live,
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::{
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::UnixStream,
    };

    use prismattyc_emulator::Emulator;
    use prismattyc_mux::{
        spaces_dir, ArrangementWire, ColorWire, ControlError, ControlErrorCode, ControlRequest,
        ControlResponse, ControlResponseBody, ControlResponseData, CursorShapeWire, Event,
        OverlayKind, PaneContent, PaneOverlay, PaneStyled, RichPointerPhase, StyleRun,
        WorkspaceInverseRun, WorkspaceStyleRun, PROTOCOL_VERSION,
    };

    #[test]
    fn zero_in_either_dimension_is_unknown() {
        assert_eq!(normalize_winsize(0, 0), None);
        assert_eq!(normalize_winsize(0, 24), None);
        assert_eq!(normalize_winsize(80, 0), None);
    }

    #[test]
    fn measured_size_is_kept_with_min_clamp() {
        assert_eq!(normalize_winsize(80, 24), Some((80, 24)));
        assert_eq!(normalize_winsize(1, 24), Some((2, 24)));
        assert_eq!(normalize_winsize(80, 1), Some((80, 1)));
    }

    #[test]
    fn cell_px_from_window_divides_ioctl_pixels() {
        assert_eq!(cell_px_from_window(800, 80), 10);
        assert_eq!(cell_px_from_window(552, 24), 23);
        assert_eq!(cell_px_from_window(0, 80), 0);
    }

    fn pane_grid(cols: u32, rows: u32, cursor_row: u32, cursor_col: u32) -> PaintFrame {
        let line = "x".repeat(cols as usize);
        let lines: Vec<String> = (0..rows).map(|_| line.clone()).collect();
        PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision: 1,
            cols,
            rows,
            cursor_row,
            cursor_col,
            cursor_visible: true,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines,
            cursor_shape: None,
        })
    }

    #[test]
    fn viewport_clips_83x57_into_120x40_and_keeps_last_row_cursor() {
        let frame = pane_grid(83, 57, 56, 0);
        let clipped = clip_frame_to_terminal(frame, 120, 40);
        assert_eq!(clipped.content.cols, 83);
        assert_eq!(clipped.content.rows, 40);
        assert_eq!(clipped.content.cursor_row, 39);
        assert!(clipped.content.cursor_row < 40);
        let paint = String::from_utf8(paint_bytes(None, &clipped, None)).unwrap();
        assert!(paint.contains("\x1b[40;1H"), "{paint:?}");
        assert_eq!(
            viewport_hint(83, 57, 120, 40).as_deref(),
            Some("pane 83×57 > 120×40 · C-\\ z fit")
        );
    }

    #[test]
    fn viewport_clips_83x57_into_60x20_and_keeps_last_row_cursor() {
        let frame = pane_grid(83, 57, 56, 40);
        let clipped = clip_frame_to_terminal(frame, 60, 20);
        assert_eq!(clipped.content.cols, 60);
        assert_eq!(clipped.content.rows, 20);
        assert_eq!(clipped.content.cursor_row, 19);
        assert!(clipped.content.cursor_col < 60);
        let paint = String::from_utf8(paint_bytes(None, &clipped, None)).unwrap();
        assert!(paint.contains("\x1b[20;"), "{paint:?}");
    }

    #[test]
    fn clip_identity_keeps_sgr() {
        let mut next = frame(1, &["RED"]);
        next.runs = vec![vec![StyleRun {
            text: "RED".into(),
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            fg: ColorWire::Ansi { n: 1 },
            bg: ColorWire::Default,
        }]];
        let clipped = clip_frame_to_terminal(next, 80, 24);
        assert_eq!(clipped.runs[0][0].fg, ColorWire::Ansi { n: 1 });
        let out = String::from_utf8(paint_bytes(None, &clipped, None)).unwrap();
        assert!(out.contains("\x1b[31mRED"), "{out:?}");
    }

    #[test]
    fn clip_keeps_sgr_on_visible_span() {
        let line = format!("{}{}", "R".repeat(10), "G".repeat(73));
        let mut next = PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision: 1,
            cols: 83,
            rows: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines: vec![line.clone()],
            cursor_shape: None,
        });
        next.runs = vec![vec![
            StyleRun {
                text: "R".repeat(10),
                bold: false,
                italic: false,
                underline: false,
                inverse: false,
                fg: ColorWire::Ansi { n: 1 },
                bg: ColorWire::Default,
            },
            StyleRun {
                text: "G".repeat(73),
                bold: false,
                italic: false,
                underline: false,
                inverse: false,
                fg: ColorWire::Ansi { n: 2 },
                bg: ColorWire::Default,
            },
        ]];
        let clipped = clip_frame_to_terminal(next, 60, 20);
        assert_eq!(clipped.runs[0][0].fg, ColorWire::Ansi { n: 1 });
        assert_eq!(clipped.runs[0][1].fg, ColorWire::Ansi { n: 2 });
        let out = String::from_utf8(paint_bytes(None, &clipped, None)).unwrap();
        assert!(out.contains("\x1b[31m"), "{out:?}");
        assert!(out.contains("\x1b[32m"), "{out:?}");
    }

    #[test]
    fn clip_keeps_viewport_overlay_in_viewport_coords() {
        let mut frame = pane_grid(83, 57, 56, 0);
        frame.overlays.push(PaneOverlay {
            id: 1,
            kind: OverlayKind::Viewport,
            row: 1,
            col: 0,
            rows: 1,
            cols: 4,
            text: "hud".into(),
            runs: Vec::new(),
        });
        let clipped = clip_frame_to_terminal(frame, 120, 40);
        assert_eq!(clipped.overlays[0].kind, OverlayKind::Viewport);
        assert_eq!(clipped.overlays[0].row, 1);
        assert_eq!(clipped.overlays[0].col, 0);
    }

    #[test]
    fn clip_translates_cell_rect_overlay_into_viewport() {
        let mut frame = pane_grid(83, 57, 56, 0);
        frame.overlays.push(PaneOverlay {
            id: 1,
            kind: OverlayKind::CellRect,
            row: 50,
            col: 10,
            rows: 1,
            cols: 4,
            text: "stat".into(),
            runs: Vec::new(),
        });
        let clipped = clip_frame_to_terminal(frame, 120, 40);
        assert_eq!(clipped.overlays[0].kind, OverlayKind::CellRect);
        assert_eq!(clipped.overlays[0].row, 33);
        assert_eq!(clipped.overlays[0].col, 10);
    }

    #[test]
    fn clip_drops_cell_rect_left_of_origin() {
        let mut frame = pane_grid(83, 57, 56, 82);
        frame.overlays.push(PaneOverlay {
            id: 1,
            kind: OverlayKind::CellRect,
            row: 50,
            col: 0,
            rows: 1,
            cols: 4,
            text: "gone".into(),
            runs: Vec::new(),
        });
        let clipped = clip_frame_to_terminal(frame, 60, 20);
        assert!(clipped.overlays.is_empty());
    }

    #[test]
    fn clip_trims_cell_rect_that_straddles_origin() {
        let mut frame = pane_grid(83, 57, 56, 82);
        frame.overlays.push(PaneOverlay {
            id: 1,
            kind: OverlayKind::CellRect,
            row: 50,
            col: 20,
            rows: 1,
            cols: 10,
            text: "0123456789".into(),
            runs: Vec::new(),
        });
        let clipped = clip_frame_to_terminal(frame, 60, 20);
        assert_eq!(clipped.overlays.len(), 1);
        assert_eq!(clipped.overlays[0].col, 0);
        assert_eq!(clipped.overlays[0].cols, 7);
        assert_eq!(clipped.overlays[0].text, "3456789");
    }

    fn frame(revision: u64, lines: &[&str]) -> PaintFrame {
        let lines: Vec<String> = lines.iter().map(|s| (*s).to_string()).collect();
        PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision,
            cols: 80,
            rows: lines.len() as u32,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines,
            cursor_shape: None,
        })
    }

    #[test]
    fn copy_mode_entry_navigation_and_quit_reset_local_state() {
        let mut scroll = ScrollState {
            supported: true,
            max: Some(20),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        scroll.enter_and_page_up();
        assert!(scroll.active);
        assert!(scroll.copy_mode);
        assert_eq!(scroll.offset, 3);
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 3, col: 0 });

        scroll.apply_copy_nav(NavKey::Right);
        scroll.apply_copy_nav(NavKey::Right);
        scroll.apply_copy_nav(NavKey::Up);
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 4, col: 2 });
        scroll.toggle_copy_selection();
        assert!(scroll.copy_selecting);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 4, col: 2 }));

        scroll.leave();
        assert!(!scroll.active);
        assert!(!scroll.copy_mode);
        assert!(!scroll.copy_selecting);
        assert_eq!(scroll.copy_anchor, None);
        assert_eq!(scroll.offset, 0);
    }

    #[test]
    fn copy_selection_anchor_tracks_history_scroll() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            copy_selecting: true,
            copy_anchor: Some(CopyPoint { row: 2, col: 0 }),
            copy_cursor: CopyPoint { row: 2, col: 0 },
            supported: true,
            max: Some(20),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        scroll.apply_copy_nav(NavKey::Up);
        assert_eq!(scroll.offset, 1);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 2, col: 0 }));
        assert!(copy_cell_selected(&scroll, 0, 0));
        assert!(copy_cell_selected(&scroll, 1, 0));
    }

    #[test]
    fn copy_cursor_and_selection_reserve_status_row() {
        let mut scroll = ScrollState {
            supported: true,
            max: Some(20),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        scroll.enter_and_page_up();
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 3, col: 0 });

        let next = frame(1, &["one", "two", "three", "status"]);
        let painted = compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        let text = String::from_utf8_lossy(&painted);
        assert!(text.contains("\x1b[4;1H\x1b[K\x1b[0m[scroll 3/20 copy]\x1b[?25l"));
        assert!(text.contains("\x1b[3;1H\x1b[?25h"));
        assert!(!text.contains("\x1b[4;1H\x1b[?25h"));
    }

    #[test]
    fn marked_history_survives_page_home_and_end_navigation() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            copy_selecting: true,
            copy_anchor: Some(CopyPoint { row: 4, col: 2 }),
            copy_cursor: CopyPoint { row: 3, col: 4 },
            supported: true,
            max: Some(20),
            offset: 3,
            rows: 4,
            cols: 12,
            ..Default::default()
        };

        scroll.apply_copy_nav(NavKey::PageUp);
        assert_eq!(scroll.offset, 6);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 4, col: 2 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 3, col: 4 });

        scroll.apply_copy_nav(NavKey::Home);
        assert_eq!(scroll.offset, 20);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 4, col: 2 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 3, col: 4 });

        scroll.apply_copy_nav(NavKey::End);
        assert_eq!(scroll.offset, 0);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 4, col: 2 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 3, col: 4 });
    }

    #[test]
    fn wheel_preserves_marked_history_and_copy_mode_at_live_tail() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            copy_selecting: true,
            copy_anchor: Some(CopyPoint { row: 3, col: 0 }),
            copy_cursor: CopyPoint { row: 2, col: 3 },
            supported: true,
            max: Some(20),
            offset: 2,
            rows: 4,
            cols: 12,
            ..Default::default()
        };

        scroll.wheel_up();
        assert_eq!(scroll.offset, 5);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 3, col: 0 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 2, col: 3 });

        scroll.wheel_down();
        assert_eq!(scroll.offset, 2);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 3, col: 0 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 2, col: 3 });

        scroll.wheel_down();
        assert_eq!(scroll.offset, 0);
        assert!(scroll.active);
        assert!(scroll.copy_mode);
        assert!(scroll.copy_selecting);
        assert_eq!(scroll.copy_anchor, Some(CopyPoint { row: 3, col: 0 }));
        assert_eq!(scroll.copy_cursor, CopyPoint { row: 2, col: 3 });
    }

    #[test]
    fn copy_selection_extracts_known_cells_and_paints_inverse_cells() {
        let next = frame(1, &["alpha", "bravo", "charlie"]);
        let mut scroll = ScrollState {
            copy_mode: true,
            copy_selecting: true,
            copy_anchor: Some(CopyPoint { row: 1, col: 1 }),
            copy_cursor: CopyPoint { row: 0, col: 3 },
            rows: 3,
            cols: 80,
            ..Default::default()
        };
        assert_eq!(copy_selection_text(&next, &scroll), "lpha\nbrav");
        let painted = compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        let text = String::from_utf8_lossy(&painted);
        assert!(text.contains("\x1b[1;2H\x1b[7ml"), "{text:?}");
        assert!(text.contains("\x1b[2;4H\x1b[7mv"), "{text:?}");
        assert!(
            text.contains("\x1b[2;4H\x1b[?25h"),
            "copy cursor missing: {text:?}"
        );

        let wide = frame(2, &["你好"]);
        let wide_scroll = ScrollState {
            copy_mode: true,
            copy_cursor: CopyPoint { row: 0, col: 3 },
            rows: 1,
            cols: 80,
            ..Default::default()
        };
        assert_eq!(copy_selection_text(&wide, &wide_scroll), "你好");

        scroll.copy_cursor = CopyPoint { row: 1, col: 0 };
        assert_eq!(copy_selection_text(&next, &scroll), "al");
    }

    #[test]
    fn known_line_yank_emits_osc52_clipboard() {
        let next = frame(1, &["first", "known line", "tail"]);
        let scroll = ScrollState {
            copy_mode: true,
            copy_cursor: CopyPoint { row: 0, col: 0 },
            rows: 3,
            cols: 80,
            ..Default::default()
        };
        let text = copy_selection_text(&next, &scroll);
        assert_eq!(text, "known line");
        let osc = prismattyc_core::encode_osc52_clipboard(&text).unwrap();
        let painted = paint_bytes(None, &next, Some(&text));
        assert!(painted.starts_with(&osc), "clipboard sequence missing");
    }

    #[test]
    fn copy_mode_search_finds_wraps_and_paints_matches() {
        let next = frame(1, &["alpha unique", "beta unique", "gamma"]);
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(0),
            offset: 0,
            rows: 4,
            cols: 80,
            search: CopySearch {
                lines: next.content.lines.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        scroll.copy_cursor = CopyPoint { row: 0, col: 0 };
        scroll.start_copy_search(CopySearchDir::Forward);
        for ch in "unique".chars() {
            scroll.copy_search_type(ch);
        }
        assert_eq!(copy_search_rank(&scroll), Some((1, 2)));
        assert_eq!(scroll.copy_cursor.col, 6);
        scroll.copy_search_step(false, false);
        assert_eq!(copy_search_rank(&scroll), Some((2, 2)));
        scroll.copy_search_step(false, false);
        assert_eq!(
            copy_search_rank(&scroll),
            Some((1, 2)),
            "n wraps to the first match"
        );
        scroll.copy_search_step(true, false);
        assert_eq!(copy_search_rank(&scroll), Some((2, 2)), "N steps backward");

        let painted = compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        let text = String::from_utf8_lossy(&painted);
        assert!(
            text.contains("\x1b[2;6H\x1b[7mu"),
            "current match inverse: {text:?}"
        );
        assert!(
            text.contains("\x1b[1;7H\x1b[4mu"),
            "other match underline: {text:?}"
        );
        assert!(
            text.contains("copy /unique█ 2/2"),
            "rank in status: {text:?}"
        );
    }

    #[test]
    fn scroll_copy_suffix_tables_empty_query_rank_guards() {
        let lines = ["alpha unique", "beta"];
        let cases = [
            ("not copy mode", false, None, "", "", false, ""),
            ("copy idle", true, None, "", "", false, " copy"),
            ("copy select", true, None, "", "", true, " copy select"),
            (
                "empty live query omits 0/0",
                true,
                Some(CopySearchDir::Forward),
                "",
                "",
                false,
                " copy /█",
            ),
            (
                "typed miss shows 0/0",
                true,
                Some(CopySearchDir::Forward),
                "zzz",
                "",
                false,
                " copy /zzz█ 0/0",
            ),
            (
                "reverse empty live query omits 0/0",
                true,
                Some(CopySearchDir::Reverse),
                "",
                "",
                false,
                " copy ?█",
            ),
            (
                "committed miss shows 0/0",
                true,
                None,
                "",
                "zzz",
                false,
                " copy /zzz 0/0",
            ),
        ];
        for (label, copy_mode, prompt, query, committed, selecting, want) in cases {
            let dir = prompt.unwrap_or(CopySearchDir::Forward);
            let scroll = ScrollState {
                copy_mode,
                copy_selecting: selecting,
                cols: 80,
                rows: 4,
                search: CopySearch {
                    prompt,
                    query: query.into(),
                    committed: committed.into(),
                    dir,
                    lines: lines.iter().map(|s| (*s).to_string()).collect(),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(scroll_copy_suffix(&scroll), want, "{label}");
        }

        let mut hit = ScrollState {
            copy_mode: true,
            cols: 80,
            rows: 4,
            search: CopySearch {
                lines: lines.iter().map(|s| (*s).to_string()).collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        hit.start_copy_search(CopySearchDir::Forward);
        for ch in "unique".chars() {
            hit.copy_search_type(ch);
        }
        assert_eq!(
            scroll_copy_suffix(&hit),
            " copy /unique█ 1/1",
            "live hit shows rank"
        );
    }

    #[test]
    fn copy_mode_search_question_prompt_is_reverse() {
        let next = frame(1, &["one hit", "two hit", "end"]);
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(0),
            rows: 4,
            cols: 80,
            search: CopySearch {
                lines: next.content.lines.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        scroll.copy_cursor = CopyPoint {
            row: scroll.copy_last_row(),
            col: 0,
        };
        scroll.start_copy_search(CopySearchDir::Reverse);
        for ch in "hit".chars() {
            scroll.copy_search_type(ch);
        }
        assert_eq!(scroll.search.dir, CopySearchDir::Reverse);
        assert_eq!(copy_search_rank(&scroll), Some((2, 2)));
    }

    #[test]
    fn copy_mode_search_esc_closes_prompt_without_leaving_copy_mode() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"/ab").unwrap());
        assert!(ctx.scroll.search.prompt.is_some());
        assert_eq!(ctx.scroll.search.query, "ab");
        assert!(ctx.pending_utf8.is_empty());
        ctx.pending_utf8.push(0xc3);
        // Lone ESC stays buffered; a following non-CSI byte completes LeaveScroll.
        assert!(!handle_input(&mut ctx, b"\x1b.").unwrap());
        assert!(ctx.scroll.copy_mode);
        assert!(ctx.scroll.search.prompt.is_none());
        assert!(ctx.scroll.search.query.is_empty());
        assert!(ctx.pending_utf8.is_empty());
        assert!(!*ctx.controller);
    }

    #[test]
    fn copy_mode_search_csi_does_not_leak_into_query() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"/ab").unwrap());
        assert_eq!(ctx.scroll.search.query, "ab");
        assert!(!handle_input(&mut ctx, b"\x1b[31m").unwrap());
        assert_eq!(
            ctx.scroll.search.query, "ab",
            "SGR must not leak '[' or letters"
        );
        assert!(!handle_input(&mut ctx, b"[").unwrap());
        assert_eq!(ctx.scroll.search.query, "ab[", "literal '[' stays");
        assert!(!handle_input(&mut ctx, b"c").unwrap());
        assert_eq!(ctx.scroll.search.query, "ab[c");
        assert!(!*ctx.controller);
    }

    #[test]
    fn copy_mode_search_lone_esc_cancels_prompt_not_copy_mode() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        scroll.copy_search_type('a');
        assert!(apply_lone_escape(&mut scroll));
        assert!(scroll.copy_mode);
        assert!(scroll.search.prompt.is_none());
        assert!(scroll.active);
    }

    /// Idle path after POLL_WAIT: lone 0x1b plus a leftover UTF-8 prefix.
    /// Must cancel the prompt, stay in copy mode, and drop the fragment.
    #[test]
    fn copy_mode_search_idle_esc_clears_pending_utf8() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        scroll.copy_search_type('a');
        let mut pending_utf8 = vec![0xc3];
        let mut pending_keys = vec![0x1b];
        assert_eq!(pending_keys.as_slice(), [0x1b]);
        pending_keys.clear();
        assert!(apply_idle_lone_escape(&mut scroll, &mut pending_utf8));
        assert!(scroll.copy_mode);
        assert!(scroll.search.prompt.is_none());
        assert!(scroll.active);
        assert!(pending_utf8.is_empty());
    }

    #[test]
    fn copy_mode_search_committed_query_survives_esc_and_drives_n() {
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            search: CopySearch {
                lines: vec!["hit one".into(), "hit two".into(), "plain".into()],
                ..Default::default()
            },
            copy_cursor: CopyPoint { row: 0, col: 0 },
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        for ch in "hit".chars() {
            scroll.copy_search_type(ch);
        }
        scroll.commit_copy_search_prompt();
        assert_eq!(scroll.search.committed, "hit");
        assert_eq!(copy_search_rank(&scroll), Some((1, 2)));
        scroll.copy_search_step(false, false);
        assert_eq!(copy_search_rank(&scroll), Some((2, 2)));

        scroll.start_copy_search(CopySearchDir::Forward);
        scroll.copy_search_type('z');
        let mut pending_utf8 = vec![0xc3];
        assert!(apply_idle_lone_escape(&mut scroll, &mut pending_utf8));
        assert!(scroll.copy_mode);
        assert_eq!(scroll.search.committed, "hit");
        assert!(scroll.search.query.is_empty());
        scroll.copy_search_step(true, false);
        assert_eq!(copy_search_rank(&scroll), Some((1, 2)));
    }

    #[test]
    fn copy_mode_search_utf8_query_accumulates_across_bytes() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            search: CopySearch {
                lines: vec!["café".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"/").unwrap());
        // Two Forward keys in one drain: first byte is incomplete UTF-8.
        assert!(!handle_input(&mut ctx, &[0xc3, 0xbc]).unwrap());
        assert_eq!(ctx.scroll.search.query, "ü");
        assert!(ctx.pending_utf8.is_empty());
    }

    #[test]
    fn copy_mode_search_zero_matches_and_wide_cols() {
        let next = frame(1, &["你好 unique", "plain"]);
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(0),
            rows: 3,
            cols: 80,
            search: CopySearch {
                lines: next.content.lines.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        for ch in "zzz".chars() {
            scroll.copy_search_type(ch);
        }
        assert_eq!(copy_search_rank(&scroll), None);
        scroll.search.query.clear();
        for ch in "unique".chars() {
            scroll.copy_search_type(ch);
        }
        assert_eq!(scroll.copy_cursor.col, 5);
        assert_eq!(copy_search_rank(&scroll), Some((1, 1)));
    }

    #[test]
    fn copy_mode_search_current_survives_offset_growth() {
        let next = frame(1, &["alpha unique", "beta", "gamma"]);
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(2),
            offset: 0,
            rows: 4,
            cols: 80,
            search: CopySearch {
                lines: next.content.lines.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        for ch in "unique".chars() {
            scroll.copy_search_type(ch);
        }
        let before = scroll.search.current;
        assert!(before.is_some());
        scroll.anchor_to(Some(3));
        assert_eq!(scroll.offset, 1);
        assert_eq!(
            scroll.search.current.map(|p| p.row),
            before.map(|p| p.row + 1),
            "live tail growth keeps the match in history coords"
        );
        scroll.apply_frame_flags(&next);
        assert_eq!(copy_search_rank(&scroll), Some((1, 1)));
    }

    #[test]
    fn copy_mode_consumes_unrecognized_input_without_pty_forwarding() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(5),
            rows: 4,
            cols: 12,
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"x").unwrap());
        assert!(ctx.pending_utf8.is_empty());
        assert!(!*ctx.controller);
        assert!(!handle_input(&mut ctx, b"\x1b[C").unwrap());
        assert_eq!(ctx.scroll.copy_cursor.col, 1);
        assert!(!handle_input(&mut ctx, b"y").unwrap());
        assert!(ctx.scroll.copy_yank_requested);
    }

    #[test]
    fn session_switch_chord_leaves_trailing_bytes_for_the_new_pane() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'n', b'l', b's']).unwrap());
        assert_eq!(*ctx.session_switch, Some(SessionSwitch::Next));
        assert!(!*ctx.controller, "must not acquire on the old pane");
        assert_eq!(ctx.pending_keys, b"ls");
        assert!(ctx.pending_utf8.is_empty());
    }

    #[test]
    fn handle_input_jump_session_keeps_trailing_bytes() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'3', b'x']).unwrap());
        assert_eq!(*ctx.session_switch, Some(SessionSwitch::Jump(3)));
        assert_eq!(ctx.pending_keys, b"x");
        ctx.pending_keys.clear();
        assert!(handle_input(&mut ctx, &[DETACH_PREFIX, b'd']).unwrap());
    }

    #[test]
    fn handle_input_enter_scroll_empty_history_stays_live() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            supported: true,
            max: Some(0),
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'[']).unwrap());
        assert!(!ctx.scroll.active);
        assert!(ctx.pending_utf8.is_empty());
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn handle_input_copy_mode_slash_opens_search_prompt() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(4),
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"/").unwrap());
        assert!(ctx.scroll.copy_mode);
        assert!(ctx.scroll.search.prompt.is_some());
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn handle_input_sync_chord_skipped_when_read_only() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: true,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b's']).unwrap());
        assert!(!*ctx.sync_input);
        assert_peer_got_no_request(&mut peer);
    }

    fn assert_peer_got_no_request(peer: &mut UnixStream) {
        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 32];
        match peer.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => panic!("unexpected control bytes: {:?}", &buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => panic!("peer read failed: {err}"),
        }
    }

    #[test]
    fn arrange_chord_skipped_when_read_only_or_copy_mode() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: true,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'a']).unwrap());
        assert_eq!(ctx.scroll.arrange_step, 0);
        assert_peer_got_no_request(&mut peer);
        ctx.read_only = false;
        ctx.scroll.copy_mode = true;
        ctx.scroll.active = true;
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'a']).unwrap());
        assert_eq!(ctx.scroll.arrange_step, 0);
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn lease_changed_clears_stale_controller_flag() {
        let mut lease_held = false;
        let mut controller = true;
        assert!(apply_controller_owner(
            1,
            Some(99),
            &mut lease_held,
            &mut controller
        ));
        assert!(lease_held);
        assert!(!controller);
        assert!(apply_controller_owner(
            1,
            None,
            &mut lease_held,
            &mut controller
        ));
        assert!(!lease_held);
        assert!(!controller);
        assert!(apply_controller_owner(
            1,
            Some(1),
            &mut lease_held,
            &mut controller
        ));
        assert!(!lease_held);
        assert!(controller);
        assert!(!apply_controller_owner(
            1,
            Some(1),
            &mut lease_held,
            &mut controller
        ));
    }

    #[test]
    fn handle_input_not_controller_keeps_attach_alive() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let responder = std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: ControlRequest = serde_json::from_str(&line).unwrap();
            let request_id = match request {
                ControlRequest::WritePane { request_id, .. } => request_id,
                other => panic!("expected WritePane, got {other:?}"),
            };
            let response = ControlResponse {
                version: PROTOCOL_VERSION,
                request_id,
                body: ControlResponseBody::Error {
                    error: ControlError {
                        code: ControlErrorCode::NotController,
                        message: "not controller (held by 99)".into(),
                        resnapshot_required: false,
                        oldest_available_sequence: None,
                        current_sequence: None,
                        holder: Some(99),
                    },
                },
            };
            serde_json::to_writer(&peer, &response).unwrap();
            peer.write_all(b"\n").unwrap();
        });
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = true;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(
            !handle_input(&mut ctx, b"x").unwrap(),
            "NotController must not end attach"
        );
        assert!(!*ctx.controller);
        assert!(*ctx.lease_held);
        assert!(
            ctx.held_notice
                .as_deref()
                .is_some_and(|text| text.contains("client 99")),
            "{:?}",
            ctx.held_notice
        );
        responder.join().unwrap();
    }

    fn live_view() -> InputView {
        InputView {
            read_only: false,
            copy_mode: false,
            scroll_active: false,
            search_prompt: false,
            experimental_rich: false,
            structured_focus: false,
            history_available: false,
            scroll_supported: false,
            rich_focus: false,
            workspace_rows: 0,
            controller: true,
            child_mouse: None,
            alt_active: false,
            history_max: None,
        }
    }

    #[test]
    fn route_attach_key_table_covers_session_chrome_scroll_and_write() {
        let nav = |kind: NavKey| AttachKey::Nav {
            kind,
            bytes: b"\x1b[A".to_vec(),
            modifiers: 0,
        };
        let cases: &[(&str, AttachKey, InputView, InputRoute)] = &[
            ("detach", AttachKey::Detach, live_view(), InputRoute::Detach),
            (
                "next",
                AttachKey::NextSession,
                live_view(),
                InputRoute::NextSession,
            ),
            (
                "prev",
                AttachKey::PrevSession,
                live_view(),
                InputRoute::PrevSession,
            ),
            (
                "jump",
                AttachKey::JumpSession(3),
                live_view(),
                InputRoute::JumpSession(3),
            ),
            ("fit", AttachKey::Fit, live_view(), InputRoute::Fit),
            (
                "arrange",
                AttachKey::Arrange,
                live_view(),
                InputRoute::Arrange,
            ),
            (
                "arrange-ro",
                AttachKey::Arrange,
                InputView {
                    read_only: true,
                    ..live_view()
                },
                InputRoute::Skip,
            ),
            (
                "arrange-copy",
                AttachKey::Arrange,
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::Skip,
            ),
            (
                "sync",
                AttachKey::ToggleSyncInput,
                live_view(),
                InputRoute::ToggleSync,
            ),
            (
                "sync-ro",
                AttachKey::ToggleSyncInput,
                InputView {
                    read_only: true,
                    ..live_view()
                },
                InputRoute::Skip,
            ),
            (
                "semantic-rich",
                AttachKey::SemanticCopy {
                    bytes: b"copy".to_vec(),
                },
                InputView {
                    experimental_rich: true,
                    ..live_view()
                },
                InputRoute::SemanticCopy,
            ),
            (
                "semantic-plain",
                AttachKey::SemanticCopy {
                    bytes: b"copy".to_vec(),
                },
                live_view(),
                InputRoute::Skip,
            ),
            (
                "rich-toggle",
                AttachKey::RichFocusToggle {
                    bytes: b"g".to_vec(),
                },
                InputView {
                    experimental_rich: true,
                    ..live_view()
                },
                InputRoute::RichFocusToggle,
            ),
            (
                "rich-toggle-write",
                AttachKey::RichFocusToggle {
                    bytes: b"g".to_vec(),
                },
                live_view(),
                InputRoute::Write(b"g".to_vec()),
            ),
            (
                "enter-history",
                AttachKey::EnterScroll,
                InputView {
                    history_available: true,
                    scroll_supported: true,
                    ..live_view()
                },
                InputRoute::EnterScroll,
            ),
            (
                "enter-empty",
                AttachKey::EnterScroll,
                InputView {
                    scroll_supported: true,
                    ..live_view()
                },
                InputRoute::StayLive,
            ),
            (
                "enter-unsupported",
                AttachKey::EnterScroll,
                live_view(),
                InputRoute::Write(vec![DETACH_PREFIX, b'[']),
            ),
            (
                "nav-search",
                nav(NavKey::Up),
                InputView {
                    search_prompt: true,
                    scroll_active: true,
                    copy_mode: true,
                    ..live_view()
                },
                InputRoute::Skip,
            ),
            (
                "nav-copy",
                nav(NavKey::Down),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopyNav(NavKey::Down),
            ),
            (
                "nav-scroll",
                nav(NavKey::Up),
                InputView {
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::ScrollNav(NavKey::Up),
            ),
            (
                "nav-pageup",
                nav(NavKey::PageUp),
                InputView {
                    history_available: true,
                    scroll_supported: true,
                    ..live_view()
                },
                InputRoute::PageUpEnter,
            ),
            (
                "nav-pageup-no-history",
                nav(NavKey::PageUp),
                live_view(),
                InputRoute::Write(b"\x1b[A".to_vec()),
            ),
            (
                "nav-focus",
                nav(NavKey::Left),
                InputView {
                    rich_focus: true,
                    ..live_view()
                },
                InputRoute::FocusNav {
                    token: "Left",
                    modifiers: 0,
                },
            ),
            (
                "nav-write",
                nav(NavKey::Right),
                live_view(),
                InputRoute::Write(b"\x1b[A".to_vec()),
            ),
            (
                "leave-search-esc",
                AttachKey::LeaveScroll { bytes: vec![0x1b] },
                InputView {
                    search_prompt: true,
                    scroll_active: true,
                    copy_mode: true,
                    ..live_view()
                },
                InputRoute::CancelSearch,
            ),
            (
                "leave-search-feed",
                AttachKey::LeaveScroll {
                    bytes: b"x".to_vec(),
                },
                InputView {
                    search_prompt: true,
                    scroll_active: true,
                    copy_mode: true,
                    ..live_view()
                },
                InputRoute::FeedSearch(b"x".to_vec()),
            ),
            (
                "leave-scroll",
                AttachKey::LeaveScroll {
                    bytes: b"q".to_vec(),
                },
                InputView {
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::LeaveScroll,
            ),
            (
                "leave-rich-esc",
                AttachKey::LeaveScroll { bytes: vec![0x1b] },
                InputView {
                    rich_focus: true,
                    ..live_view()
                },
                InputRoute::RichFocusEsc,
            ),
            (
                "leave-write",
                AttachKey::LeaveScroll { bytes: vec![0x1b] },
                live_view(),
                InputRoute::Write(vec![0x1b]),
            ),
            (
                "fwd-search-enter",
                AttachKey::Forward(b"\r".to_vec()),
                InputView {
                    search_prompt: true,
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::SearchCommit,
            ),
            (
                "fwd-search-bs",
                AttachKey::Forward(vec![0x7f]),
                InputView {
                    search_prompt: true,
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::SearchBackspace,
            ),
            (
                "fwd-copy-slash",
                AttachKey::Forward(b"/".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopySearchStart(CopySearchDir::Forward),
            ),
            (
                "fwd-copy-qmark",
                AttachKey::Forward(b"?".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopySearchStart(CopySearchDir::Reverse),
            ),
            (
                "fwd-copy-n",
                AttachKey::Forward(b"n".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopySearchStep { reverse: false },
            ),
            (
                "fwd-copy-N",
                AttachKey::Forward(b"N".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopySearchStep { reverse: true },
            ),
            (
                "fwd-copy-v",
                AttachKey::Forward(b"v".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopyToggleSelect,
            ),
            (
                "fwd-copy-y",
                AttachKey::Forward(b"y".to_vec()),
                InputView {
                    copy_mode: true,
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::CopyYank,
            ),
            (
                "fwd-focus-vt",
                AttachKey::Forward(b"a".to_vec()),
                InputView {
                    rich_focus: true,
                    ..live_view()
                },
                InputRoute::FocusVt("a".into()),
            ),
            (
                "fwd-write",
                AttachKey::Forward(b"x".to_vec()),
                live_view(),
                InputRoute::Write(b"x".to_vec()),
            ),
            (
                "fwd-scroll-skip",
                AttachKey::Forward(b"x".to_vec()),
                InputView {
                    scroll_active: true,
                    ..live_view()
                },
                InputRoute::Skip,
            ),
            (
                "mouse-other",
                AttachKey::MouseOther,
                live_view(),
                InputRoute::Ignore,
            ),
        ];
        for (name, key, view, want) in cases {
            assert_eq!(&route_attach_key(key, view), want, "{name}");
        }
    }

    fn wheel_at(x: u32, y: u32, shift: bool) -> AttachKey {
        AttachKey::Wheel {
            up: true,
            x,
            y,
            shift,
        }
    }

    fn mouse_at(x: u32, y: u32, shift: bool, primary: bool) -> AttachKey {
        AttachKey::MouseReport {
            bytes: b"\x1b[<0;2;3M".to_vec(),
            shift,
            x,
            y,
            primary,
            phase: RichPointerPhase::Press,
        }
    }

    fn rich_view() -> InputView {
        InputView {
            structured_focus: true,
            workspace_rows: 10,
            ..live_view()
        }
    }

    #[test]
    fn route_wheel_and_mouse_table() {
        let cases: &[(&str, AttachKey, InputView, InputRoute)] = &[
            (
                "wheel-rich",
                wheel_at(2, 3, false),
                rich_view(),
                InputRoute::RichScroll {
                    up: true,
                    x: 2,
                    y: 3,
                },
            ),
            (
                "wheel-rich-shift",
                wheel_at(2, 3, true),
                rich_view(),
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: true,
                    acquire: false,
                },
            ),
            (
                "wheel-rich-scroll",
                wheel_at(2, 3, false),
                InputView {
                    scroll_active: true,
                    ..rich_view()
                },
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "wheel-rich-y0",
                wheel_at(2, 0, false),
                rich_view(),
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 0,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "wheel-rich-y-past",
                wheel_at(2, 11, false),
                rich_view(),
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 11,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "wheel-rich-x0",
                wheel_at(0, 3, false),
                rich_view(),
                InputRoute::Wheel {
                    up: true,
                    x: 0,
                    y: 3,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "wheel-child-acquire",
                wheel_at(2, 3, false),
                InputView {
                    child_mouse: Some(true),
                    ..live_view()
                },
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: false,
                    acquire: true,
                },
            ),
            (
                "wheel-child-shift",
                wheel_at(2, 3, true),
                InputView {
                    child_mouse: Some(true),
                    ..live_view()
                },
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: true,
                    acquire: false,
                },
            ),
            (
                "wheel-unknown-mouse",
                wheel_at(2, 3, false),
                live_view(),
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "wheel-alt-empty-history",
                wheel_at(2, 3, false),
                InputView {
                    child_mouse: Some(false),
                    history_max: Some(0),
                    alt_active: true,
                    ..live_view()
                },
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: false,
                    acquire: true,
                },
            ),
            (
                "wheel-off-mouse-no-alt",
                wheel_at(2, 3, false),
                InputView {
                    child_mouse: Some(false),
                    history_max: Some(0),
                    alt_active: false,
                    ..live_view()
                },
                InputRoute::Wheel {
                    up: true,
                    x: 2,
                    y: 3,
                    shift: false,
                    acquire: false,
                },
            ),
            (
                "mouse-rich",
                mouse_at(2, 3, false, true),
                rich_view(),
                InputRoute::RichPointer {
                    x: 2,
                    y: 3,
                    phase: RichPointerPhase::Press,
                },
            ),
            (
                "mouse-rich-y0",
                mouse_at(2, 0, false, true),
                InputView {
                    controller: true,
                    child_mouse: Some(true),
                    ..rich_view()
                },
                InputRoute::ForwardMouse(b"\x1b[<0;2;3M".to_vec()),
            ),
            (
                "mouse-rich-x0",
                mouse_at(0, 3, false, true),
                InputView {
                    controller: true,
                    child_mouse: Some(true),
                    ..rich_view()
                },
                InputRoute::ForwardMouse(b"\x1b[<0;2;3M".to_vec()),
            ),
            (
                "mouse-rich-shift",
                mouse_at(2, 3, true, true),
                InputView {
                    controller: true,
                    child_mouse: Some(true),
                    ..rich_view()
                },
                InputRoute::Ignore,
            ),
            (
                "mouse-not-primary",
                mouse_at(2, 3, false, false),
                InputView {
                    controller: true,
                    child_mouse: Some(true),
                    ..rich_view()
                },
                InputRoute::ForwardMouse(b"\x1b[<0;2;3M".to_vec()),
            ),
            (
                "mouse-forward",
                mouse_at(2, 3, false, true),
                InputView {
                    controller: true,
                    child_mouse: Some(true),
                    ..live_view()
                },
                InputRoute::ForwardMouse(b"\x1b[<0;2;3M".to_vec()),
            ),
            (
                "mouse-observer",
                mouse_at(2, 3, false, true),
                InputView {
                    controller: false,
                    child_mouse: Some(true),
                    ..live_view()
                },
                InputRoute::Ignore,
            ),
        ];
        for (name, key, view, want) in cases {
            assert_eq!(&route_attach_key(key, view), want, "{name}");
        }
    }

    #[test]
    fn handle_input_copy_yank_and_leave_scroll() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            supported: true,
            max: Some(4),
            ..Default::default()
        };
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, b"y").unwrap());
        assert!(ctx.scroll.take_copy_yank_request());
        ctx.scroll.active = true;
        ctx.scroll.copy_mode = true;
        assert!(!handle_input(&mut ctx, b"q").unwrap());
        assert!(!ctx.scroll.active);
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn handle_input_prev_session_keeps_trailing_bytes() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'p', b'x']).unwrap());
        assert_eq!(*ctx.session_switch, Some(SessionSwitch::Prev));
        assert_eq!(ctx.pending_keys, b"x");
    }

    fn reply_ok_lines(mut peer: UnixStream, n: usize) {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            for _ in 0..n {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let request_id = request.get("request_id").and_then(|v| v.as_u64()).unwrap();
                let response = ControlResponse {
                    version: PROTOCOL_VERSION,
                    request_id,
                    body: ControlResponseBody::Ok {
                        response: ControlResponseData::Pong,
                    },
                };
                serde_json::to_writer(&peer, &response).unwrap();
                peer.write_all(b"\n").unwrap();
            }
        });
    }

    #[test]
    fn handle_input_arrange_cycles_named_kinds() {
        let (stream, peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        reply_ok_lines(peer.try_clone().unwrap(), 2);
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: false,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: false,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'a']).unwrap());
        assert_eq!(ctx.scroll.arrange_step, 1);
        assert!(!handle_input(&mut ctx, &[DETACH_PREFIX, b'a']).unwrap());
        assert_eq!(ctx.scroll.arrange_step, 2);
        let kinds = ArrangementWire::ALL;
        assert_ne!(kinds[1], kinds[0], "cycle must not collapse via /");
    }

    #[test]
    fn maybe_idle_release_lease_requires_every_idle_guard() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let old = Instant::now()
            .checked_sub(LEASE_IDLE + Duration::from_millis(20))
            .unwrap_or_else(Instant::now);
        let mut last_typed = Some(old);
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut controller = false;
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert_peer_got_no_request(&mut peer);

        controller = true;
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            true,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert_peer_got_no_request(&mut peer);

        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            b"x",
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert_peer_got_no_request(&mut peer);

        rich_focus_id = Some(7);
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert_peer_got_no_request(&mut peer);
        rich_focus_id = None;

        last_typed = Some(Instant::now());
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert_peer_got_no_request(&mut peer);

        last_typed = Some(old);
        peer.set_nonblocking(false).unwrap();
        reply_ok_lines(peer.try_clone().unwrap(), 1);
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert!(!controller);
        assert!(last_typed.is_none());
        assert!(rich_focus_id.is_none());
        assert!(!structured_focus);
    }

    #[test]
    fn maybe_idle_release_lease_keeps_controller_when_request_fails() {
        let (mut client, _peer) = dummy_client();
        let old = Instant::now()
            .checked_sub(LEASE_IDLE + Duration::from_millis(20))
            .unwrap_or_else(Instant::now);
        let mut last_typed = Some(old);
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut controller = true;
        maybe_idle_release_lease(
            &mut client,
            1,
            1,
            Some("s"),
            &mut controller,
            false,
            &[],
            &mut rich_focus_id,
            &mut structured_focus,
            &mut last_typed,
        );
        assert!(
            controller,
            "timed-out ReleaseLease must not drop the local controller flag"
        );
        assert!(
            last_typed.is_some(),
            "idle stamp stays until ReleaseLease Ok"
        );
        assert!(rich_focus_id.is_none());
        assert!(!structured_focus);
    }

    #[test]
    fn keep_request_ok_per_verb_ok_timeout_transport() {
        let verbs = ["ReleaseLease", "Resize", "SwitchSession"];
        for op in verbs {
            assert_eq!(
                request_err_line(op, 3, Some("nexus"), &"timed out"),
                format!("pmux-attach: {op} failed pane=3 session=nexus: timed out")
            );
            assert_eq!(
                request_err_line(op, 3, Some("nexus"), &"Broken pipe"),
                format!("pmux-attach: {op} failed pane=3 session=nexus: Broken pipe")
            );
            assert_eq!(
                request_err_line(op, 3, None, &"timed out"),
                format!("pmux-attach: {op} failed pane=3 session=-: timed out")
            );
            assert_eq!(keep_request_ok(op, 3, Some("nexus"), Ok(1u8)), Some(1));
            REQUEST_ERRS.lock().unwrap().clear();
            assert_eq!(
                keep_request_ok::<u8>(op, 3, Some("nexus"), Err(anyhow::anyhow!("timed out"))),
                None
            );
            assert_eq!(
                keep_request_ok::<u8>(op, 3, Some("nexus"), Err(anyhow::anyhow!("Broken pipe"))),
                None
            );
            let lines = REQUEST_ERRS.lock().unwrap().clone();
            assert_eq!(
                lines,
                [
                    format!("pmux-attach: {op} failed pane=3 session=nexus: timed out"),
                    format!("pmux-attach: {op} failed pane=3 session=nexus: Broken pipe"),
                ]
            );
        }
        // Idle-escape uses the same logger with the known session, not `-`.
        REQUEST_ERRS.lock().unwrap().clear();
        assert_eq!(
            keep_request_ok::<u8>(
                "Resize",
                3,
                Some("nexus"),
                Err(anyhow::anyhow!("timed out")),
            ),
            None
        );
        assert_eq!(
            REQUEST_ERRS.lock().unwrap().as_slice(),
            ["pmux-attach: Resize failed pane=3 session=nexus: timed out"]
        );
        assert!(
            !REQUEST_ERRS
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("session=-")),
            "idle-escape must not log session=- when session_name is known"
        );
    }

    #[test]
    fn apply_observed_winsize_and_resize_needs_send_tables() {
        let a = LocalWinsize {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
        };
        let b = LocalWinsize {
            cols: 100,
            rows: 30,
            cell_width_px: 8,
            cell_height_px: 16,
        };
        let cases = [(None, a, true), (Some(a), a, false), (Some(a), b, true)];
        for (last_sent, size, want) in cases {
            assert_eq!(resize_needs_send(last_sent, size), want, "{last_sent:?}");
        }

        let mut last_size = Some(a);
        let mut previous = Some(frame(1, &["hello"]));
        let mut scroll = ScrollState::default();
        apply_observed_winsize(&mut last_size, &mut previous, &mut scroll, Some(a));
        assert!(
            previous.is_some(),
            "same ioctl must not drop the paint frame"
        );
        apply_observed_winsize(&mut last_size, &mut previous, &mut scroll, Some(b));
        assert!(previous.is_none());
        assert_eq!(last_size, Some(b));
        assert_eq!(scroll.rows, 30);
        assert_eq!(scroll.cols, 100);
        apply_observed_winsize(&mut last_size, &mut previous, &mut scroll, None);
        assert_eq!(last_size, None);
        assert!(previous.is_none());
    }

    #[test]
    fn space_stamp_due_needs_hint_and_interval() {
        let mut checked = Instant::now();
        assert!(!space_stamp_due(false, &mut checked));
        checked = Instant::now()
            .checked_sub(SPACE_STAMP_INTERVAL + Duration::from_millis(20))
            .unwrap_or_else(Instant::now);
        assert!(space_stamp_due(true, &mut checked));
        let mut fresh = Instant::now();
        assert!(!space_stamp_due(true, &mut fresh));
    }

    fn dummy_client() -> (Client, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        (client, peer)
    }

    fn write_snapshot(peer: &mut UnixStream, request_id: u64) {
        let response = prismattyc_mux::ControlResponse {
            version: prismattyc_mux::PROTOCOL_VERSION,
            request_id,
            body: prismattyc_mux::ControlResponseBody::Ok {
                response: prismattyc_mux::ControlResponseData::Snapshot {
                    snapshot: prismattyc_mux::Snapshot {
                        sequence: 0,
                        sessions: Vec::new(),
                    },
                },
            },
        };
        serde_json::to_writer(&mut *peer, &response).unwrap();
        peer.write_all(b"\n").unwrap();
        peer.flush().unwrap();
    }

    fn write_registered(peer: &mut UnixStream, request_id: u64) {
        let response = prismattyc_mux::ControlResponse {
            version: prismattyc_mux::PROTOCOL_VERSION,
            request_id,
            body: prismattyc_mux::ControlResponseBody::Ok {
                response: prismattyc_mux::ControlResponseData::ClientRegistered { client_id: 9 },
            },
        };
        serde_json::to_writer(&mut *peer, &response).unwrap();
        peer.write_all(b"\n").unwrap();
        peer.flush().unwrap();
    }

    #[test]
    fn drain_probe_recovers_after_two_orphaned_snapshots() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_snapshot(&mut peer, 1);
        write_snapshot(&mut peer, 2);
        write_snapshot(&mut peer, 3);
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        run_drain_probe(&mut client).expect("probe recovers after two stale snapshots");
    }

    #[test]
    fn request_drains_two_stale_ids_and_the_next_request_succeeds() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_registered(&mut peer, 1);
        write_registered(&mut peer, 2);
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 2,
        };
        let first = client
            .request(
                |request_id| prismattyc_mux::ControlRequest::RegisterClient {
                    version: prismattyc_mux::PROTOCOL_VERSION,
                    request_id,
                },
            )
            .expect("drain stale id 1 and hit 2");
        match first {
            prismattyc_mux::ControlResponseData::ClientRegistered { client_id } => {
                assert_eq!(client_id, 9);
            }
            other => panic!("expected ClientRegistered, got {other:?}"),
        }
        write_registered(&mut peer, 3);
        let second = client
            .request(
                |request_id| prismattyc_mux::ControlRequest::RegisterClient {
                    version: prismattyc_mux::PROTOCOL_VERSION,
                    request_id,
                },
            )
            .expect("next request after drain");
        match second {
            prismattyc_mux::ControlResponseData::ClientRegistered { client_id } => {
                assert_eq!(client_id, 9);
            }
            other => panic!("expected ClientRegistered, got {other:?}"),
        }
    }

    #[test]
    fn request_fails_when_response_id_is_ahead() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_registered(&mut peer, 5);
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 2,
        };
        let err = client
            .request(
                |request_id| prismattyc_mux::ControlRequest::RegisterClient {
                    version: prismattyc_mux::PROTOCOL_VERSION,
                    request_id,
                },
            )
            .expect_err("ahead id is a protocol error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ahead of awaited 2"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn apply_resolved_space_clears_previous_even_without_snapshot() {
        let (mut client, _peer) = dummy_client();
        let mut space_name = Some("no-such-space".into());
        let mut cycle_names = vec!["alpha".into()];
        let mut identity = Some("keep".into());
        let mut previous = Some(frame(1, &["hello"]));
        apply_resolved_space(
            &mut client,
            true,
            "alpha",
            &mut space_name,
            &mut cycle_names,
            &mut identity,
            &mut previous,
        );
        assert!(previous.is_none());
    }

    #[test]
    fn refresh_space_identity_applies_when_stamp_and_label_differ() {
        let (mut client, _peer) = dummy_client();
        let mut checked = Instant::now()
            .checked_sub(SPACE_STAMP_INTERVAL + Duration::from_millis(20))
            .unwrap_or_else(Instant::now);
        let mut stamp = Some(SystemTime::UNIX_EPOCH);
        let mut space_name = Some("no-such-space".into());
        let mut cycle_names = Vec::new();
        let mut identity = Some("keep".into());
        let mut previous = Some(frame(1, &["hello"]));
        refresh_space_identity(
            &mut client,
            true,
            &mut checked,
            &mut stamp,
            Some("alpha"),
            &mut space_name,
            &mut cycle_names,
            &mut identity,
            &mut previous,
        );
        assert_ne!(stamp, Some(SystemTime::UNIX_EPOCH));
    }

    #[test]
    fn handle_idle_escape_clears_lone_esc_in_copy_search() {
        let (mut client, mut peer) = dummy_client();
        let mut pending_keys = vec![0x1b];
        let mut pending_utf8 = b"leftover".to_vec();
        let mut scroll = ScrollState {
            active: true,
            copy_mode: true,
            ..Default::default()
        };
        scroll.start_copy_search(CopySearchDir::Forward);
        let mut previous = Some(frame(1, &["hello"]));
        let mut rich_focus_id = None;
        let mut structured_focus = false;
        let mut sync_input = false;
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut held_notice = None;
        let mut session_switch = None;
        handle_idle_escape(
            &mut client,
            1,
            1,
            1,
            Some("nexus"),
            false,
            &mut pending_keys,
            &mut pending_utf8,
            &mut scroll,
            &mut previous,
            &mut rich_focus_id,
            &mut structured_focus,
            &mut sync_input,
            &mut controller,
            &mut lease_held,
            &mut last_typed,
            false,
            &mut held_notice,
            &mut session_switch,
        )
        .unwrap();
        assert!(pending_keys.is_empty(), "idle ESC must be consumed");
        assert!(pending_utf8.is_empty());
        assert!(previous.is_none());
        assert!(scroll.search.prompt.is_none());
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn idle_forward_and_revoke_are_noops_when_read_only() {
        let (mut client, mut peer) = dummy_client();
        let mut pending_keys = Vec::new();
        let mut pending_utf8 = Vec::new();
        let mut scroll = ScrollState::default();
        let mut rich_focus_id = Some(3);
        let mut structured_focus = true;
        let mut sync_input = false;
        let mut controller = false;
        let mut lease_held = false;
        let mut last_typed = None;
        let mut held_notice = None;
        let mut session_switch = None;
        let mut ctx = InputCtx {
            client: &mut client,
            client_id: 1,
            pane_id: 1,
            window_id: 1,
            session: Some("s"),
            sync_input: &mut sync_input,
            controller: &mut controller,
            lease_held: &mut lease_held,
            last_typed: &mut last_typed,
            pending_keys: &mut pending_keys,
            pending_utf8: &mut pending_utf8,
            scroll: &mut scroll,
            experimental_rich: true,
            rich_focus_id: &mut rich_focus_id,
            structured_focus: &mut structured_focus,
            workspace_rows: 0,
            child_pid: None,
            read_only: true,
            held_notice: &mut held_notice,
            session_switch: &mut session_switch,
        };
        idle_revoke_rich_focus(&mut ctx).unwrap();
        idle_forward_escape(&mut ctx).unwrap();
        assert!(last_typed.is_none());
        assert!(pending_utf8.is_empty());
        assert_eq!(rich_focus_id, Some(3));
        assert_peer_got_no_request(&mut peer);
    }

    struct AttachBits {
        client: Client,
        peer: UnixStream,
        sync_input: bool,
        controller: bool,
        lease_held: bool,
        last_typed: Option<Instant>,
        pending_keys: Vec<u8>,
        pending_utf8: Vec<u8>,
        scroll: ScrollState,
        rich_focus_id: Option<u32>,
        structured_focus: bool,
        held_notice: Option<String>,
        session_switch: Option<SessionSwitch>,
        read_only: bool,
        workspace_rows: u32,
        experimental_rich: bool,
    }

    impl AttachBits {
        fn new() -> Self {
            let (stream, peer) = UnixStream::pair().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            Self {
                client: Client {
                    reader: BufReader::new(stream.try_clone().unwrap()),
                    writer: stream,
                    next_request_id: 1,
                },
                peer,
                sync_input: false,
                controller: false,
                lease_held: false,
                last_typed: None,
                pending_keys: Vec::new(),
                pending_utf8: Vec::new(),
                scroll: ScrollState::default(),
                rich_focus_id: None,
                structured_focus: false,
                held_notice: None,
                session_switch: None,
                read_only: false,
                workspace_rows: 24,
                experimental_rich: true,
            }
        }

        fn ctx(&mut self) -> InputCtx<'_> {
            InputCtx {
                client: &mut self.client,
                client_id: 1,
                pane_id: 1,
                window_id: 1,
                session: Some("s"),
                sync_input: &mut self.sync_input,
                controller: &mut self.controller,
                lease_held: &mut self.lease_held,
                last_typed: &mut self.last_typed,
                pending_keys: &mut self.pending_keys,
                pending_utf8: &mut self.pending_utf8,
                scroll: &mut self.scroll,
                experimental_rich: self.experimental_rich,
                rich_focus_id: &mut self.rich_focus_id,
                structured_focus: &mut self.structured_focus,
                workspace_rows: self.workspace_rows,
                child_pid: None,
                read_only: self.read_only,
                held_notice: &mut self.held_notice,
                session_switch: &mut self.session_switch,
            }
        }
    }

    fn reply_ok_and_capture(
        mut peer: UnixStream,
        n: usize,
    ) -> std::sync::mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(peer.try_clone().unwrap());
            for _ in 0..n {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let request_id = request.get("request_id").and_then(|v| v.as_u64()).unwrap();
                let _ = tx.send(request);
                let response = ControlResponse {
                    version: PROTOCOL_VERSION,
                    request_id,
                    body: ControlResponseBody::Ok {
                        response: ControlResponseData::Pong,
                    },
                };
                serde_json::to_writer(&peer, &response).unwrap();
                peer.write_all(b"\n").unwrap();
            }
        });
        rx
    }

    #[test]
    fn refresh_space_identity_skips_apply_when_label_unchanged() {
        let (mut client, _peer) = dummy_client();
        let mut checked = Instant::now()
            .checked_sub(SPACE_STAMP_INTERVAL + Duration::from_millis(20))
            .unwrap_or_else(Instant::now);
        let mut stamp = Some(SystemTime::UNIX_EPOCH);
        let dir = spaces_dir();
        let mut space_name = resolve_space_label(None, "alpha", &dir);
        let start = space_name.clone();
        let mut cycle_names = Vec::new();
        let mut identity = Some("keep".into());
        let mut previous = Some(frame(1, &["hello"]));
        refresh_space_identity(
            &mut client,
            true,
            &mut checked,
            &mut stamp,
            Some("alpha"),
            &mut space_name,
            &mut cycle_names,
            &mut identity,
            &mut previous,
        );
        assert_eq!(space_name, start);
        assert!(
            previous.is_some(),
            "unchanged label must not call apply_resolved_space"
        );
    }

    #[test]
    fn idle_revoke_and_forward_run_when_controller() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 2);
        idle_revoke_rich_focus(&mut bits.ctx()).unwrap();
        assert!(
            bits.last_typed.is_some(),
            "idle_revoke must stamp last_typed"
        );
        bits.last_typed = None;
        idle_forward_escape(&mut bits.ctx()).unwrap();
        assert!(
            bits.last_typed.is_some(),
            "idle_forward must stamp last_typed"
        );
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first["type"], "rich_focus_toggle");
        assert_eq!(second["type"], "write_pane");
    }

    #[test]
    fn with_lease_runs_body_only_when_controller() {
        let mut bits = AttachBits::new();
        bits.read_only = true;
        bits.controller = false;
        let mut ran = false;
        with_lease(&mut bits.ctx(), |_| {
            ran = true;
            Ok(())
        })
        .unwrap();
        assert!(!ran, "not-controller must skip the body");
        assert!(bits.last_typed.is_none());

        bits.read_only = false;
        bits.controller = true;
        with_lease(&mut bits.ctx(), |_| {
            ran = true;
            Ok(())
        })
        .unwrap();
        assert!(ran, "controller must run the body");
        assert!(bits.last_typed.is_some());
    }

    #[test]
    fn apply_arrange_cycles_kinds_via_remainder() {
        let mut bits = AttachBits::new();
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 2);
        apply_arrange(&mut bits.ctx());
        apply_arrange(&mut bits.ctx());
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first["type"], "apply_arrangement");
        assert_ne!(
            first.get("kind"),
            second.get("kind"),
            "% must pick successive ArrangementWire values"
        );
        assert_eq!(bits.scroll.arrange_step, 2);
        assert_ne!(ArrangementWire::ALL[0], ArrangementWire::ALL[1]);
    }

    #[test]
    fn apply_fit_sends_resize_when_winsize_is_injected() {
        let mut bits = AttachBits::new();
        let size = LocalWinsize {
            cols: 120,
            rows: 40,
            cell_width_px: 8,
            cell_height_px: 16,
        };
        let _inject = InjectedWinsize::set(Some(size));
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 1);
        apply_fit(&mut bits.ctx());
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req["type"], "resize");
        assert_eq!(req["cols"], 120);
        assert_eq!(req["rows"], 40);
        assert_eq!(req["fit"], true);
        assert_eq!(req["host"], false);
        assert_eq!(req["cell_width_px"], 8);
        assert_eq!(req["cell_height_px"], 16);
    }

    #[test]
    fn apply_fit_is_noop_when_injected_winsize_is_none() {
        let mut bits = AttachBits::new();
        let _inject = InjectedWinsize::set(None);
        apply_fit(&mut bits.ctx());
        assert_peer_got_no_request(&mut bits.peer);
    }

    #[test]
    fn apply_chrome_fit_sends_resize_through_fit_arm() {
        let mut bits = AttachBits::new();
        let size = LocalWinsize {
            cols: 80,
            rows: 24,
            cell_width_px: 9,
            cell_height_px: 18,
        };
        let _inject = InjectedWinsize::set(Some(size));
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 1);
        apply_chrome_route(&mut bits.ctx(), InputRoute::Fit).unwrap();
        let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(req["type"], "resize");
        assert_eq!(req["fit"], true);
        assert_eq!(req["cols"], 80);
        assert_eq!(req["rows"], 24);
    }

    #[test]
    fn apply_chrome_and_rich_chrome_send_requests() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 2);
        apply_chrome_route(&mut bits.ctx(), InputRoute::SemanticCopy).unwrap();
        apply_rich_chrome(&mut bits.ctx(), InputRoute::RichFocusToggle).unwrap();
        let copy = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let focus = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(copy["type"], "copy_semantic");
        assert_eq!(focus["type"], "rich_focus_toggle");
    }

    #[test]
    fn apply_scroll_nav_copy_search_and_copy_keys_mutate_scroll() {
        let mut bits = AttachBits::new();
        bits.scroll.active = true;
        bits.scroll.supported = true;
        bits.scroll.max = Some(10);
        bits.scroll.offset = 0;
        bits.scroll.rows = 8;
        apply_scroll_nav_route(&mut bits.ctx(), InputRoute::ScrollNav(NavKey::Up));
        assert_eq!(bits.scroll.offset, 1, "ScrollNav arm must call apply_nav");

        bits.scroll.copy_mode = true;
        bits.scroll.start_copy_search(CopySearchDir::Forward);
        bits.pending_utf8 = b"keep".to_vec();
        bits.scroll.search.query = "ab".into();
        apply_search_prompt_route(&mut bits.ctx(), InputRoute::SearchBackspace);
        assert!(bits.pending_utf8.is_empty());
        assert_eq!(bits.scroll.search.query, "a");

        bits.pending_utf8 = b"x".to_vec();
        bits.scroll.search.query = "hi".into();
        bits.scroll.search.prompt = Some(CopySearchDir::Forward);
        apply_search_prompt_route(&mut bits.ctx(), InputRoute::SearchCommit);
        assert!(bits.pending_utf8.is_empty());
        assert!(bits.scroll.search.prompt.is_none());
        assert_eq!(bits.scroll.search.committed, "hi");

        bits.scroll.search.prompt = Some(CopySearchDir::Forward);
        apply_search_prompt_route(&mut bits.ctx(), InputRoute::CancelSearch);
        assert!(bits.scroll.search.prompt.is_none());

        bits.scroll.copy_selecting = false;
        bits.scroll.copy_anchor = None;
        apply_copy_keys(&mut bits.ctx(), InputRoute::CopyToggleSelect);
        assert!(bits.scroll.copy_selecting);
        bits.scroll.search.committed = "hi".into();
        bits.scroll.search.query.clear();
        apply_copy_keys(
            &mut bits.ctx(),
            InputRoute::CopySearchStep { reverse: false },
        );
        assert!(
            bits.scroll.search.current.is_none(),
            "empty viewport lines make CopySearchStep a no-op"
        );
    }

    #[test]
    fn apply_copy_keys_search_step_moves_cursor_on_match() {
        let mut bits = AttachBits::new();
        bits.scroll.copy_mode = true;
        bits.scroll.rows = 8;
        bits.scroll.cols = 80;
        bits.scroll.offset = 0;
        bits.scroll.max = Some(0);
        bits.scroll.search.lines = vec!["hello hi there".into()];
        bits.scroll.search.committed = "hi".into();
        bits.scroll.search.query.clear();
        bits.scroll.search.prompt = None;
        bits.scroll.search.dir = CopySearchDir::Forward;
        bits.scroll.copy_cursor = CopyPoint { row: 0, col: 0 };
        apply_copy_keys(
            &mut bits.ctx(),
            InputRoute::CopySearchStep { reverse: false },
        );
        assert_eq!(
            bits.scroll.search.current,
            Some(CopyPoint { row: 6, col: 6 }),
            "CopySearchStep must walk to the match"
        );
        assert_eq!(bits.scroll.copy_cursor, CopyPoint { row: 6, col: 6 });
    }

    #[test]
    fn send_rich_scroll_and_pointer_subtract_origin() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 2);
        send_rich_scroll(&mut bits.ctx(), true, 4, 6).unwrap();
        send_rich_pointer(&mut bits.ctx(), 4, 6, RichPointerPhase::Press).unwrap();
        let scroll = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let pointer = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(scroll["type"], "rich_input");
        assert_eq!(scroll["input"]["row"], 5);
        assert_eq!(scroll["input"]["col"], 3);
        assert_eq!(scroll["input"]["delta"], -(WHEEL_LINES as i16));
        assert_eq!(pointer["type"], "rich_input");
        assert_eq!(pointer["input"]["row"], 5);
        assert_eq!(pointer["input"]["col"], 3);
    }

    #[test]
    fn apply_wheel_and_rich_key_routes_execute() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        bits.scroll.supported = true;
        bits.scroll.max = Some(10);
        bits.scroll.active = true;
        bits.scroll.offset = 0;
        let mut want_write = false;
        apply_wheel_route(&mut bits.ctx(), true, 1, 1, false, false, &mut want_write).unwrap();
        assert_eq!(
            bits.scroll.offset,
            WHEEL_LINES.min(10),
            "apply_wheel_route must host-scroll"
        );

        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 3);
        apply_rich_key_route(
            &mut bits.ctx(),
            InputRoute::RichScroll {
                up: true,
                x: 2,
                y: 3,
            },
        )
        .unwrap();
        apply_rich_key_route(
            &mut bits.ctx(),
            InputRoute::RichPointer {
                x: 2,
                y: 3,
                phase: RichPointerPhase::Release,
            },
        )
        .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap()["type"],
            "rich_input"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap()["type"],
            "rich_input"
        );
        bits.structured_focus = true;
        apply_rich_key_route(&mut bits.ctx(), InputRoute::FocusVt("Enter".into())).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap()["type"],
            "rich_input"
        );
    }

    #[test]
    fn apply_rich_key_route_focus_nav_writes_rich_input() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        bits.structured_focus = true;
        let rx = reply_ok_and_capture(bits.peer.try_clone().unwrap(), 1);
        apply_rich_key_route(
            &mut bits.ctx(),
            InputRoute::FocusNav {
                token: "Left",
                modifiers: 0,
            },
        )
        .unwrap();
        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request["type"], "rich_input");
        assert_eq!(request["input"]["kind"], "key");
        assert_eq!(request["input"]["key"], "Left");
    }

    #[test]
    fn flush_pending_write_stamps_when_want_write() {
        let mut bits = AttachBits::new();
        bits.controller = true;
        bits.pending_utf8.clear();
        flush_pending_write(&mut bits.ctx(), true).unwrap();
        assert!(
            bits.last_typed.is_some(),
            "!want_write && empty is the only early return"
        );
    }

    #[test]
    fn apply_session_route_breaks_for_session_switches() {
        let mut bits = AttachBits::new();
        let mut rest = std::iter::empty();
        assert_eq!(
            apply_session_route(&mut bits.ctx(), &InputRoute::NextSession, &mut rest),
            InputLoop::Break
        );
        assert_eq!(bits.session_switch, Some(SessionSwitch::Next));
        bits.session_switch = None;
        let mut rest = std::iter::empty();
        assert_eq!(
            apply_session_route(&mut bits.ctx(), &InputRoute::PrevSession, &mut rest),
            InputLoop::Break
        );
        assert_eq!(bits.session_switch, Some(SessionSwitch::Prev));
        bits.session_switch = None;
        let mut rest = std::iter::empty();
        assert_eq!(
            apply_session_route(&mut bits.ctx(), &InputRoute::JumpSession(3), &mut rest),
            InputLoop::Break
        );
        assert_eq!(bits.session_switch, Some(SessionSwitch::Jump(3)));
    }

    #[test]
    fn route_helpers_keep_lockstep_conditions() {
        let view = InputView {
            child_mouse: Some(false),
            history_max: Some(0),
            alt_active: true,
            ..live_view()
        };
        assert!(wheel_wants_child_view(&view, false));
        let searching = InputView {
            search_prompt: true,
            scroll_active: true,
            ..live_view()
        };
        assert_eq!(
            route_leave_scroll(&searching, &[0x1b]),
            InputRoute::CancelSearch
        );
        assert_eq!(
            route_attach_key(
                &AttachKey::Arrange,
                &InputView {
                    read_only: true,
                    copy_mode: false,
                    ..live_view()
                }
            ),
            InputRoute::Skip
        );
    }

    #[test]
    fn apply_local_winsize_clears_previous_when_ioctl_differs() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 1,
        };
        let mut last_size = Some(LocalWinsize {
            cols: 12,
            rows: 4,
            cell_width_px: 8,
            cell_height_px: 16,
        });
        let mut last_sent = last_size;
        let mut previous = Some(frame(1, &["hello"]));
        let mut scroll = ScrollState::default();
        apply_local_winsize(
            &mut client,
            1,
            1,
            1,
            Some("s"),
            &mut last_size,
            &mut last_sent,
            &mut previous,
            &mut scroll,
        );
        assert!(
            previous.is_none(),
            "12x4 is not a live ioctl size; a no-op apply_local_winsize misses this"
        );
        assert_peer_got_no_request(&mut peer);
    }

    #[test]
    fn styled_workspace_paints_above_guest_and_offsets_cursor() {
        let styled = PaneStyled {
            content: PaneContent {
                pane_id: 1,
                revision: 1,
                cols: 8,
                rows: 2,
                cursor_row: 1,
                cursor_col: 2,
                cursor_visible: true,
                alt_active: false,
                child_alive: true,
                child_pid: None,
                lines: vec!["guest-a ".into(), "guest-b ".into()],
                cursor_shape: None,
            },
            runs: vec![
                vec![StyleRun::plain("guest-a ")],
                vec![StyleRun::plain("guest-b ")],
            ],
            workspace: vec!["Runbook ".into(), "> check ".into()],
            view_offset: Some(0),
            max_view_scroll: Some(0),
            child_mouse_tracking: Some(false),
            child_mouse_sgr: Some(false),
            overlays: Vec::new(),
            experimental_rich: true,
            rich_focus_id: None,
            structured_focus: false,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
            workspace_inverse: vec![WorkspaceInverseRun {
                row: 1,
                col: 0,
                cols: 8,
            }],
            workspace_styles: vec![WorkspaceStyleRun {
                row: 0,
                col: 0,
                cols: 7,
                fg: ColorWire::Ansi { n: 2 },
            }],
        };
        let frame = PaintFrame::from_styled(styled);
        assert_eq!(frame.workspace_rows, 2);
        assert_eq!(frame.content.rows, 4);
        assert_eq!(frame.content.cursor_row, 3);
        assert_eq!(frame.content.lines[0], "Runbook ");
        assert_eq!(frame.content.lines[2], "guest-a ");
        assert!(!frame.runs[0][0].inverse);
        assert_eq!(frame.runs[0][0].fg, ColorWire::Ansi { n: 2 });
        assert!(
            frame.runs[1][0].inverse,
            "selected workspace row is inverse"
        );
        let paint = String::from_utf8(paint_bytes(None, &frame, None)).unwrap();
        assert!(paint.find("Runbook").unwrap() < paint.find("guest-a").unwrap());
    }

    #[test]
    fn inverse_runs_use_cell_columns_for_wide_and_combining() {
        let line = "> 警告 e\u{301} path".to_string();
        let runs = super::selected_workspace_runs(
            &[line],
            &[WorkspaceInverseRun {
                row: 0,
                col: 2,
                cols: 4,
            }],
            &[],
        );
        let inverse: String = runs[0]
            .iter()
            .filter(|run| run.inverse)
            .map(|run| run.text.as_str())
            .collect();
        let plain: String = runs[0]
            .iter()
            .filter(|run| !run.inverse)
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(inverse, "警告");
        assert!(plain.contains("e\u{301}"), "{plain:?}");
        assert!(!inverse.contains('e'), "{inverse:?}");
    }

    #[test]
    fn dirty_lines_report_only_changed_rows() {
        let prev = ["one", "two", "three"];
        let next = ["one", "TWO", "three"];
        assert_eq!(dirty_line_indices(&prev, &next), Some(vec![1]));
        assert_eq!(dirty_line_indices(&prev[..1], &next), None);
    }

    #[test]
    fn incremental_paint_emits_only_dirty_line() {
        let prev = frame(1, &["hello", "world"]);
        let next = frame(2, &["hello", "WORLD"]);
        let out = String::from_utf8(paint_bytes(Some(&prev), &next, None)).unwrap();
        assert!(
            out.contains("\x1b[?25l"),
            "cursor hidden during paint: {out:?}"
        );
        assert!(out.contains("\x1b[2;1H"), "move to dirty row: {out:?}");
        assert!(out.contains("WORLD"), "{out:?}");
        assert!(
            !out.contains("hello"),
            "unchanged first line must not be rewritten: {out:?}"
        );
        assert!(!out.contains("\x1b[?1049h"), "no full alt reset: {out:?}");
    }

    #[test]
    fn styled_run_emits_ansi_sgr() {
        let mut next = frame(1, &["RED"]);
        next.runs = vec![vec![StyleRun {
            text: "RED".into(),
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            fg: ColorWire::Ansi { n: 1 },
            bg: ColorWire::Default,
        }]];
        let out = String::from_utf8(paint_bytes(None, &next, None)).unwrap();
        assert!(out.contains("\x1b[31mRED"), "{out:?}");
        assert!(out.contains("\x1b[0m"), "line-end reset: {out:?}");
    }

    #[test]
    fn full_paint_disables_decawm_and_keeps_every_column() {
        let cols = 16;
        let line = "X".repeat(cols);
        let next = PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision: 1,
            cols: cols as u32,
            rows: 1,
            cursor_row: 0,
            cursor_col: cols as u32,
            cursor_visible: true,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines: vec![line],
            cursor_shape: None,
        });
        let out = String::from_utf8(paint_bytes(None, &next, None)).unwrap();
        assert!(
            out.contains("\x1b[?7l"),
            "DECAWM must be off during attach paint: {out:?}"
        );
        assert_eq!(
            out.matches('X').count(),
            cols,
            "every column of a full-width row must be painted: {out:?}"
        );
        assert!(
            !out.contains("\r\n"),
            "a single full-width row must not wrap in the paint stream: {out:?}"
        );
        assert!(
            !out.contains(" q"),
            "legacy snapshots omit DECSCUSR: {out:?}"
        );
    }

    #[test]
    fn paint_emits_decscusr_when_shape_is_known() {
        let next = PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision: 1,
            cols: 4,
            rows: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines: vec!["abcd".into()],
            cursor_shape: Some(CursorShapeWire::Bar),
        });
        let out = String::from_utf8(paint_bytes(None, &next, None)).unwrap();
        assert!(
            out.contains("\x1b[6 q"),
            "bar DECSCUSR must reach the outer terminal: {out:?}"
        );
        assert!(out.contains("\x1b[?25h"), "DECTCEM still shown: {out:?}");
    }

    fn styled_full_width_frame(cols: usize, rows: usize, ch: char) -> PaintFrame {
        let line = ch.to_string().repeat(cols);
        PaintFrame {
            content: PaneContent {
                pane_id: 1,
                revision: 1,
                cols: cols as u32,
                rows: rows as u32,
                cursor_row: 0,
                cursor_col: cols as u32,
                cursor_visible: false,
                alt_active: false,
                child_alive: true,
                child_pid: None,
                lines: vec![line.clone(); rows],
                cursor_shape: None,
            },
            runs: vec![
                vec![StyleRun {
                    text: line,
                    bold: false,
                    italic: false,
                    underline: false,
                    inverse: false,
                    fg: ColorWire::Ansi { n: 1 },
                    bg: ColorWire::Default,
                }];
                rows
            ],
            view_offset: Some(0),
            max_view_scroll: Some(0),
            child_mouse_tracking: None,
            child_mouse_sgr: None,
            overlays: Vec::new(),
            experimental_rich: false,
            rich_focus_id: None,
            structured_focus: false,
            workspace_rows: 0,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
        }
    }

    fn last_cell(emu: &Emulator, row: usize) -> char {
        emu.screen()
            .row(row)
            .and_then(|cells| cells.last())
            .map(|cell| cell.character)
            .unwrap_or('\0')
    }

    #[test]
    fn outer_mouse_seq_is_wheel_only_until_child_wants_full_mouse() {
        let wheel = String::from_utf8_lossy(super::outer_mouse_seq(false));
        assert!(wheel.contains("?7700h"), "{wheel}");
        assert!(
            !wheel.contains("?1000h"),
            "7700 must not enable 1000: {wheel}"
        );
        let full = String::from_utf8_lossy(super::outer_mouse_seq(true));
        assert!(full.contains("?1000h"), "{full}");
        assert!(full.contains("?1002h"), "{full}");
        assert!(!full.contains("?1003h"), "do not force any-motion: {full}");
        assert!(
            full.contains("?7700l"),
            "full mouse must drop 7700 so host does not see both: {full}"
        );
        assert!(
            !full.contains("?7700h"),
            "full mouse does not need 7700: {full}"
        );
    }

    #[test]
    fn outer_mouse_full_requires_controller_and_child_tracking() {
        assert!(!super::outer_mouse_full(false, Some(true), false));
        assert!(!super::outer_mouse_full(true, Some(false), false));
        assert!(!super::outer_mouse_full(true, None, false));
        assert!(!super::outer_mouse_full(false, None, true));
        assert!(super::outer_mouse_full(true, Some(true), false));
        assert!(super::outer_mouse_full(true, Some(false), true));
    }

    #[test]
    fn forward_child_mouse_only_when_child_tracking_and_not_host_scroll() {
        let mut scroll = super::ScrollState::default();
        assert!(!super::forward_child_mouse(&scroll, true, false));
        scroll.child_mouse = Some(false);
        assert!(!super::forward_child_mouse(&scroll, true, false));
        scroll.child_mouse = Some(true);
        assert!(super::forward_child_mouse(&scroll, true, false));
        assert!(!super::forward_child_mouse(&scroll, true, true));
        assert!(!super::forward_child_mouse(&scroll, false, false));
        scroll.active = true;
        assert!(!super::forward_child_mouse(&scroll, true, false));
    }

    #[test]
    fn full_width_styled_row_survives_el_on_outer_grid() {
        for cols in [80_usize, 83, 101] {
            let next = styled_full_width_frame(cols, 1, 'Z');
            let bytes = paint_bytes(None, &next, None);
            assert!(
                bytes.windows(5).any(|w| w == b"\x1b[?7l"),
                "DECAWM off: {bytes:?}"
            );
            let mut emu = Emulator::new(cols, 1, 0);
            emu.feed(&bytes);
            assert_eq!(
                last_cell(&emu, 0),
                'Z',
                "styled last column must survive EL at {cols} cols"
            );
        }
    }

    #[test]
    fn incremental_full_width_styled_row_survives_el_on_outer_grid() {
        for cols in [80_usize, 83] {
            let prev = styled_full_width_frame(cols, 2, '.');
            let next = styled_full_width_frame(cols, 2, 'Z');
            let mut emu = Emulator::new(cols, 2, 0);
            emu.feed(&paint_bytes(None, &prev, None));
            emu.feed(&paint_bytes(Some(&prev), &next, None));
            assert_eq!(last_cell(&emu, 0), 'Z', "row 0 at {cols}");
            assert_eq!(last_cell(&emu, 1), 'Z', "row 1 at {cols}");
        }
    }

    fn drain(bytes: &[u8]) -> (Vec<AttachKey>, Vec<u8>) {
        let mut pending = bytes.to_vec();
        let keys = drain_keys(&mut pending);
        (keys, pending)
    }

    #[test]
    fn drain_keys_recognizes_scroll_chords() {
        assert_eq!(drain(&[DETACH_PREFIX, b'd']).0, vec![AttachKey::Detach]);
        assert_eq!(
            drain(&[DETACH_PREFIX, b's']).0,
            vec![AttachKey::ToggleSyncInput]
        );
        assert_eq!(
            drain(&[DETACH_PREFIX, b'S']).0,
            vec![AttachKey::ToggleSyncInput]
        );
        assert_eq!(
            drain(&[DETACH_PREFIX, b'[']).0,
            vec![AttachKey::EnterScroll]
        );
        assert_eq!(drain(&[DETACH_PREFIX, b'z']).0, vec![AttachKey::Fit]);
        assert_eq!(drain(&[DETACH_PREFIX, b'Z']).0, vec![AttachKey::Fit]);
        assert_eq!(
            drain(&[DETACH_PREFIX, b'n']).0,
            vec![AttachKey::NextSession]
        );
        assert_eq!(
            drain(&[DETACH_PREFIX, b'P']).0,
            vec![AttachKey::PrevSession]
        );
        assert_eq!(
            drain(&[DETACH_PREFIX, b'2']).0,
            vec![AttachKey::JumpSession(2)]
        );
        assert_eq!(drain(&[DETACH_PREFIX, b'a']).0, vec![AttachKey::Arrange]);
        assert_eq!(drain(&[DETACH_PREFIX, b'A']).0, vec![AttachKey::Arrange]);
        assert_eq!(
            drain(b"\x1b[5~").0,
            vec![AttachKey::Nav {
                kind: NavKey::PageUp,
                bytes: b"\x1b[5~".to_vec(),
                modifiers: 0,
            }]
        );
        assert_eq!(
            drain(b"\x1b[A").0,
            vec![AttachKey::Nav {
                kind: NavKey::Up,
                bytes: b"\x1b[A".to_vec(),
                modifiers: 0,
            }]
        );
        assert_eq!(
            drain(b"\x1bOA").0,
            vec![AttachKey::Nav {
                kind: NavKey::Up,
                bytes: b"\x1bOA".to_vec(),
                modifiers: 0,
            }]
        );
        assert_eq!(
            drain(b"q").0,
            vec![AttachKey::LeaveScroll { bytes: vec![b'q'] }]
        );
        let (keys, leftover) = drain(b"\x1b");
        assert!(keys.is_empty(), "{keys:?}");
        assert_eq!(leftover, b"\x1b");
        let (keys, leftover) = drain(&[DETACH_PREFIX]);
        assert!(keys.is_empty(), "{keys:?}");
        assert_eq!(leftover, [DETACH_PREFIX]);
    }

    #[test]
    fn session_switch_wraps_and_jumps() {
        let names = vec!["alpha".into(), "beta".into(), "gamma".into()];
        assert_eq!(
            target_session_name(Some("alpha"), &names, SessionSwitch::Next).as_deref(),
            Some("beta")
        );
        assert_eq!(
            target_session_name(Some("gamma"), &names, SessionSwitch::Next).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            target_session_name(Some("alpha"), &names, SessionSwitch::Prev).as_deref(),
            Some("gamma")
        );
        assert_eq!(
            target_session_name(Some("alpha"), &names, SessionSwitch::Jump(2)).as_deref(),
            Some("beta")
        );
        assert_eq!(
            target_session_name(Some("beta"), &names, SessionSwitch::Jump(2)),
            None
        );
        assert_eq!(
            target_session_name(Some("alpha"), &names, SessionSwitch::Jump(9)),
            None
        );
        assert_eq!(
            attach_identity_line(Some("today"), Some("beta"), &names).as_deref(),
            Some("space today · session beta (2/3)")
        );
        assert_eq!(
            attach_identity_line(None, Some("alpha"), &names).as_deref(),
            Some("session alpha (1/3)")
        );
        assert_eq!(
            attach_identity_line(None, Some("solo"), &[]).as_deref(),
            Some("session solo")
        );
        assert_eq!(
            space_attach_identity(false, false, None, Some("alpha"), &names),
            None,
            "plain attach has no identity overlay"
        );
        assert_eq!(
            space_attach_identity(true, false, None, Some("alpha"), &names).as_deref(),
            Some("session alpha (1/3)"),
            "hinted attach whose space is gone still shows session X"
        );
        assert_eq!(
            space_attach_identity(true, true, Some("today"), Some("alpha"), &names),
            None,
            "host nested attach has no identity overlay"
        );
    }

    fn space_fixture_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pt-205-space-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_named_space(dir: &std::path::Path, space: &str, session: &str) {
        let saved = prismattyc_mux::SavedSpace {
            id: None,
            version: prismattyc_mux::SAVED_SPACE_VERSION,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![prismattyc_mux::stub_space_session(session)],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        prismattyc_mux::save_space(dir, space, &saved).unwrap();
    }

    #[test]
    fn resolve_space_label_follows_disk_not_the_spawn_hint() {
        let dir = space_fixture_dir();
        assert_eq!(
            resolve_space_label(Some("gone"), "grok-pc", &dir),
            None,
            "hint file missing and no other space lists the session"
        );

        write_named_space(&dir, "beta", "grok-pc");
        assert_eq!(
            resolve_space_label(Some("gone"), "grok-pc", &dir).as_deref(),
            Some("beta"),
            "hint file missing: first file that lists the session"
        );

        write_named_space(&dir, "alpha", "other");
        assert_eq!(
            resolve_space_label(Some("alpha"), "grok-pc", &dir).as_deref(),
            Some("beta"),
            "hint present but session moved to beta"
        );

        let empty = space_fixture_dir();
        write_named_space(&empty, "alpha", "other");
        assert_eq!(
            resolve_space_label(Some("alpha"), "grok-pc", &empty),
            None,
            "no file lists the session"
        );
        assert_eq!(resolve_space_label(None, "grok-pc", &empty), None);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn spaces_dir_stamp_changes_when_a_space_file_is_rewritten() {
        let dir = space_fixture_dir();
        write_named_space(&dir, "beta", "grok-pc");
        let path = dir.join("beta.json");
        let file = std::fs::File::open(&path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        drop(file);
        let before = spaces_dir_stamp(&dir).unwrap();
        write_named_space(&dir, "beta", "other");
        let after = spaces_dir_stamp(&dir).unwrap();
        assert!(
            after > before,
            "in-place rewrite of spaces/beta.json must bump the stamp"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mail_title_osc_prefers_the_label_and_keeps_the_mail_suffix() {
        let base = String::from_utf8(super::mail_title_osc(Some("grok-pc"), None, 0)).unwrap();
        assert_eq!(base, "\x1b]2;pmux: grok-pc\x07");
        let titled =
            String::from_utf8(super::mail_title_osc(Some("grok-pc"), Some(" build "), 0)).unwrap();
        assert_eq!(titled, "\x1b]2;build\x07", "a set label replaces the base");
        let blank =
            String::from_utf8(super::mail_title_osc(Some("grok-pc"), Some("   "), 2)).unwrap();
        assert_eq!(
            blank, "\x1b]2;pmux: grok-pc — 2 mail\x07",
            "blank label falls back"
        );
        let both = String::from_utf8(super::mail_title_osc(None, Some("build"), 1)).unwrap();
        assert_eq!(both, "\x1b]2;build — 1 mail\x07");
    }

    #[test]
    fn compose_paint_shows_space_session_identity() {
        let next = frame(1, &["hello", "world"]);
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &ScrollState::default(),
            0,
            None,
            None,
            None,
            AttachChrome {
                identity: Some("space today · session beta (2/2)"),
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(out.contains("space today · session beta (2/2)"), "{out:?}");
        assert!(
            out.contains("\x1b[1;1H") && out.contains("\x1b[K"),
            "TTY space-attach paints identity on row 1: {out:?}"
        );
    }

    #[test]
    fn compose_paint_omits_identity_under_host() {
        let names = vec!["beta".into()];
        let identity = space_attach_identity(true, true, Some("today"), Some("beta"), &names);
        assert!(identity.is_none());
        let next = frame(1, &["hello", "world"]);
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &ScrollState::default(),
            0,
            None,
            None,
            None,
            AttachChrome {
                identity: identity.as_deref(),
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(
            !out.contains("space today") && !out.contains("session beta"),
            "host attach must not paint identity: {out:?}"
        );
        assert!(
            !out.contains("\x1b[1;1H\x1b[0m"),
            "host attach must not CSI-K guest row 1 for identity: {out:?}"
        );
        assert!(out.contains("hello"), "guest row 1 stays: {out:?}");
    }

    #[test]
    fn compose_paint_identity_does_not_overwrite_mail_letter() {
        let next = frame(1, &["hello", "world"]);
        let out = compose_paint(
            None,
            &next,
            &ScrollState::default(),
            2,
            None,
            None,
            None,
            AttachChrome {
                identity: Some("space today · session beta (2/2)"),
                mail_letter: true,
                ..Default::default()
            },
        );
        let text = String::from_utf8_lossy(&out);
        assert!(
            mail_letter_cells()
                .iter()
                .flatten()
                .any(|ch| text.contains(*ch)),
            "mail letter missing: {text:?}"
        );
        assert!(
            text.contains("\x1b[1;2H\x1b[0m space today · session beta (2/2)"),
            "col 2 must be a written space before identity: {text:?}"
        );
    }

    #[test]
    fn drain_keys_plain_y_is_not_reserved() {
        assert_eq!(drain(b"y").0, vec![AttachKey::Forward(vec![b'y'])]);
        assert_eq!(drain(b"Y").0, vec![AttachKey::Forward(vec![b'Y'])]);
        assert_eq!(
            drain(b"\x1b[27;6;99~").0,
            vec![AttachKey::SemanticCopy {
                bytes: b"\x1b[27;6;99~".to_vec()
            }]
        );
    }

    #[test]
    fn drain_keys_preserves_shift_arrows() {
        assert_eq!(
            drain(b"\x1b[1;2B").0,
            vec![AttachKey::Nav {
                kind: NavKey::Down,
                bytes: b"\x1b[1;2B".to_vec(),
                modifiers: 1,
            }]
        );
        assert_eq!(
            drain(b"\x1b[1;2A").0,
            vec![AttachKey::Nav {
                kind: NavKey::Up,
                bytes: b"\x1b[1;2A".to_vec(),
                modifiers: 1,
            }]
        );
    }

    #[test]
    fn drain_keys_recognizes_ctrl_shift_g() {
        assert_eq!(
            drain(b"\x1b[27;6;103~").0,
            vec![AttachKey::RichFocusToggle {
                bytes: b"\x1b[27;6;103~".to_vec(),
            }]
        );
        assert_eq!(
            drain(b"\x1b[27;6;71~").0,
            vec![AttachKey::RichFocusToggle {
                bytes: b"\x1b[27;6;71~".to_vec(),
            }]
        );
        assert_eq!(
            drain(b"\x07").0,
            vec![AttachKey::Forward(vec![0x07])],
            "Ctrl-G without Shift must not steal"
        );
    }

    #[test]
    fn drain_keys_parses_sgr_wheel_reports() {
        assert_eq!(
            drain(b"\x1b[<64;10;5M").0,
            vec![AttachKey::Wheel {
                up: true,
                x: 10,
                y: 5,
                shift: false,
            }]
        );
        assert_eq!(
            drain(b"\x1b[<65;10;5M").0,
            vec![AttachKey::Wheel {
                up: false,
                x: 10,
                y: 5,
                shift: false,
            }]
        );
        // Shift/ctrl modifier bits still count as wheel.
        assert_eq!(
            drain(b"\x1b[<68;1;1M").0,
            vec![AttachKey::Wheel {
                up: true,
                x: 1,
                y: 1,
                shift: true,
            }]
        );
        assert_eq!(
            drain(b"\x1b[<80;1;1M").0,
            vec![AttachKey::Wheel {
                up: true,
                x: 1,
                y: 1,
                shift: false,
            }]
        );
        // Clicks, drags, and releases keep their SGR bytes for optional child forward.
        assert_eq!(
            drain(b"\x1b[<0;3;4M").0,
            vec![AttachKey::MouseReport {
                bytes: b"\x1b[<0;3;4M".to_vec(),
                shift: false,
                x: 3,
                y: 4,
                primary: true,
                phase: RichPointerPhase::Press,
            }]
        );
        assert_eq!(
            drain(b"\x1b[<0;3;4m").0,
            vec![AttachKey::MouseReport {
                bytes: b"\x1b[<0;3;4m".to_vec(),
                shift: false,
                x: 3,
                y: 4,
                primary: true,
                phase: RichPointerPhase::Release,
            }]
        );
        assert_eq!(
            drain(b"\x1b[<4;3;4M").0,
            vec![AttachKey::MouseReport {
                bytes: b"\x1b[<4;3;4M".to_vec(),
                shift: true,
                x: 3,
                y: 4,
                primary: true,
                phase: RichPointerPhase::Press,
            }]
        );
        assert_eq!(
            drain(b"\x1b[<64;3;4m").0,
            vec![AttachKey::MouseReport {
                bytes: b"\x1b[<64;3;4m".to_vec(),
                shift: false,
                x: 3,
                y: 4,
                primary: false,
                phase: RichPointerPhase::Release,
            }]
        );
        // Two reports in one read.
        assert_eq!(
            drain(b"\x1b[<64;1;1M\x1b[<65;1;1M").0,
            vec![
                AttachKey::Wheel {
                    up: true,
                    x: 1,
                    y: 1,
                    shift: false,
                },
                AttachKey::Wheel {
                    up: false,
                    x: 1,
                    y: 1,
                    shift: false,
                }
            ]
        );
        // Incomplete report stays pending.
        let (keys, leftover) = drain(b"\x1b[<64;1");
        assert!(keys.is_empty(), "{keys:?}");
        assert_eq!(leftover, b"\x1b[<64;1");
    }

    #[test]
    fn encode_child_wheel_sgr_and_x10() {
        assert_eq!(
            super::encode_child_wheel(true, true, 10, 5),
            b"\x1b[<64;10;5M"
        );
        assert_eq!(
            super::encode_child_wheel(true, false, 3, 4),
            b"\x1b[<65;3;4M"
        );
        assert_eq!(
            super::encode_child_wheel(false, true, 1, 1),
            vec![0x1b, b'[', b'M', 64 + 32, 1 + 32, 1 + 32]
        );
    }

    #[test]
    fn decide_wheel_forwards_when_child_tracks_or_history_empty() {
        use super::{decide_wheel, WheelCtx, WheelDecision};
        match decide_wheel(WheelCtx {
            up: true,
            shift: false,
            controller: true,
            scroll_active: false,
            scroll_supported: true,
            max: Some(80),
            child_mouse: Some(true),
            child_sgr: true,
            alt_active: true,
            x: 2,
            y: 3,
        }) {
            WheelDecision::Forward(bytes) => assert_eq!(bytes, b"\x1b[<64;2;3M"),
            other => panic!("{other:?}"),
        }
        match decide_wheel(WheelCtx {
            up: false,
            shift: false,
            controller: true,
            scroll_active: false,
            scroll_supported: true,
            max: Some(0),
            child_mouse: Some(false),
            child_sgr: false,
            alt_active: true,
            x: 1,
            y: 1,
        }) {
            WheelDecision::Forward(bytes) => assert_eq!(bytes, b"\x1b[B\x1b[B\x1b[B"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decide_wheel(WheelCtx {
                up: true,
                shift: false,
                controller: true,
                scroll_active: false,
                scroll_supported: true,
                max: Some(12),
                child_mouse: Some(false),
                child_sgr: false,
                alt_active: false,
                x: 1,
                y: 1,
            }),
            WheelDecision::HostScrollUp
        );
        assert_eq!(
            decide_wheel(WheelCtx {
                up: true,
                shift: true,
                controller: true,
                scroll_active: false,
                scroll_supported: true,
                max: Some(0),
                child_mouse: Some(true),
                child_sgr: true,
                alt_active: true,
                x: 1,
                y: 1,
            }),
            WheelDecision::HostScrollUp
        );
        assert_eq!(
            decide_wheel(WheelCtx {
                up: true,
                shift: false,
                controller: false,
                scroll_active: false,
                scroll_supported: true,
                max: Some(0),
                child_mouse: Some(true),
                child_sgr: true,
                alt_active: true,
                x: 1,
                y: 1,
            }),
            WheelDecision::HostScrollUp
        );
        assert_eq!(
            decide_wheel(WheelCtx {
                up: true,
                shift: false,
                controller: true,
                scroll_active: false,
                scroll_supported: true,
                max: Some(0),
                child_mouse: None,
                child_sgr: false,
                alt_active: true,
                x: 1,
                y: 1,
            }),
            WheelDecision::HostScrollUp,
            "unknown mouse must not emit CSI arrows"
        );
        assert_eq!(
            decide_wheel(WheelCtx {
                up: true,
                shift: false,
                controller: true,
                scroll_active: false,
                scroll_supported: true,
                max: Some(0),
                child_mouse: Some(false),
                child_sgr: false,
                alt_active: false,
                x: 1,
                y: 1,
            }),
            WheelDecision::HostScrollUp,
            "empty-history primary-screen shells must not get arrows"
        );
    }

    #[test]
    fn wheel_wants_child_matches_forward_conditions() {
        let mut scroll = super::ScrollState {
            supported: true,
            max: Some(0),
            alt_active: true,
            child_mouse: Some(true),
            ..Default::default()
        };
        assert!(
            super::wheel_wants_child(&scroll, false),
            "Claude: alt + mouse + empty history"
        );
        assert!(
            !super::wheel_wants_child(&scroll, true),
            "Shift keeps host pan"
        );
        scroll.active = true;
        assert!(!super::wheel_wants_child(&scroll, false));
        scroll.active = false;
        scroll.child_mouse = Some(false);
        assert!(
            super::wheel_wants_child(&scroll, false),
            "empty alt + known-off mouse uses CSI arrows"
        );
        scroll.alt_active = false;
        assert!(
            !super::wheel_wants_child(&scroll, false),
            "primary empty shell is host scroll, not arrows"
        );
        scroll.alt_active = true;
        scroll.child_mouse = None;
        assert!(
            !super::wheel_wants_child(&scroll, false),
            "unknown mouse must not grab the lease for arrows"
        );
        scroll.child_mouse = Some(true);
        scroll.max = Some(80);
        scroll.alt_active = false;
        assert!(
            super::wheel_wants_child(&scroll, false),
            "mouse-on still wants the child when history exists"
        );
    }

    #[test]
    fn attach_history_available_requires_nonzero_max() {
        let mut scroll = super::ScrollState {
            supported: true,
            max: Some(0),
            ..Default::default()
        };
        assert!(!super::attach_history_available(&scroll));
        scroll.max = Some(12);
        assert!(super::attach_history_available(&scroll));
        scroll.supported = false;
        assert!(!super::attach_history_available(&scroll));
        scroll.supported = true;
        scroll.max = None;
        assert!(!super::attach_history_available(&scroll));
    }

    #[test]
    fn wheel_enters_scrolls_and_leaves() {
        let mut scroll = super::ScrollState {
            supported: true,
            max: Some(100),
            rows: 24,
            ..Default::default()
        };
        scroll.wheel_up();
        assert!(scroll.active);
        assert_eq!(scroll.offset, 3);
        scroll.wheel_up();
        assert_eq!(scroll.offset, 6);
        scroll.wheel_down();
        assert_eq!(scroll.offset, 3);
        assert!(scroll.active);
        scroll.wheel_down();
        assert_eq!(scroll.offset, 0);
        assert!(!scroll.active, "reaching the tail returns to live");
        // Empty history: wheel must not enter scroll mode.
        let mut empty = super::ScrollState {
            supported: true,
            max: Some(0),
            rows: 24,
            ..Default::default()
        };
        empty.wheel_up();
        assert!(!empty.active);
        // Offset clamps at max.
        let mut short = super::ScrollState {
            supported: true,
            max: Some(4),
            rows: 24,
            ..Default::default()
        };
        short.wheel_up();
        short.wheel_up();
        assert_eq!(short.offset, 4);
    }

    #[test]
    fn mail_letter_is_nerd_envelope() {
        let cells = mail_letter_cells();
        assert_eq!(cells, [['\u{F0E0}']]);
        assert_ne!(cells[0][0], '✉');
    }

    #[test]
    fn mail_letter_overlay_is_block_envelope_not_unicode_envelope() {
        let out = String::from_utf8(mail_letter_overlay_bytes()).unwrap();
        assert!(out.contains("\x1b[1;1H"), "{out:?}");
        assert!(
            !out.contains("\x1b[2;1H"),
            "one cell, not a second row: {out:?}"
        );
        assert!(out.contains('\u{F0E0}'), "{out:?}");
        let [fr, fg, fb] = MAIL_LETTER_RGB;
        assert!(
            out.contains(&format!("\x1b[38;2;{fr};{fg};{fb}m")),
            "{out:?}"
        );
        assert!(
            !out.contains("\x1b[48;2;"),
            "no full-cell pad under the letter: {out:?}"
        );
        assert!(!out.contains('✉'), "must not steal a text envelope cell");
        let cells = mail_letter_cells();
        for row in cells {
            for ch in row {
                assert!(out.contains(ch), "missing {ch:?} in {out:?}");
            }
        }
        assert_ne!(cells[0][0], ' ');
    }

    #[test]
    fn mail_overlay_does_not_mutate_paint_frame() {
        let next = frame(1, &["hello", "world"]);
        let before = next.clone();
        let scroll = ScrollState::default();
        let out = compose_paint(
            None,
            &next,
            &scroll,
            1,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        assert_eq!(next, before);
        assert!(
            mail_letter_cells()
                .iter()
                .flatten()
                .any(|ch| String::from_utf8_lossy(&out).contains(*ch)),
            "overlay missing from compose stream"
        );
        let guest = paint_bytes(None, &next, None);
        assert!(!String::from_utf8_lossy(&guest).contains("38;2;255;180;84"));
    }

    #[test]
    fn compose_paint_guest_status_is_corner_overlay_and_restores_caret() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState::default();
        assert!(!scroll.active);
        assert!(next.content.cursor_visible);
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome {
                pane_status: Some("build ok 20 chars..."),
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(out.contains("build ok 20 chars..."), "{out:?}");
        let last_row_clear = format!("\x1b[{};1H\x1b[K", next.content.rows);
        assert!(
            !out.contains(&last_row_clear),
            "must not CSI-K the guest last row: {out:?}"
        );
        assert!(out.contains("\x1b[1;"), "status paints on row 1: {out:?}");
        let last_toggle = out
            .rmatch_indices("\x1b[?25")
            .next()
            .map(|(i, _)| out.get(i..i + 6).unwrap_or(""));
        assert_eq!(
            last_toggle,
            Some("\x1b[?25h"),
            "last cursor toggle: {out:?}"
        );
    }

    #[test]
    fn compose_paint_host_nested_scroll_uses_inverse_chip_not_left_label() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState {
            active: true,
            max: Some(10),
            offset: 3,
            rows: 2,
            cols: 10,
            ..Default::default()
        };
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome {
                host_nested: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(
            !out.contains("[scroll"),
            "host nested scroll must not paint the left [scroll] label: {out:?}"
        );
        let last_row_clear = format!("\x1b[{};1H\x1b[K", next.content.rows);
        assert!(
            !out.contains(&last_row_clear),
            "host nested scroll must not CSI-K the last guest row: {out:?}"
        );
        assert!(
            out.contains("\x1b[7m 3/10 \x1b[27m"),
            "host nested scroll paints an inverse chip: {out:?}"
        );
        assert!(
            out.contains('█') && out.contains('│'),
            "host nested scroll paints a right-edge scrollbar: {out:?}"
        );
        assert_eq!(host_nested_scrollbar_thumb(4, 0, 10), (4, 1));
        assert_eq!(host_nested_scrollbar_thumb(4, 10, 10), (1, 1));
    }

    #[test]
    fn compose_paint_guest_status_appends_to_scroll_row() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState {
            active: true,
            max: Some(10),
            offset: 3,
            rows: 2,
            cols: 10,
            ..Default::default()
        };
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome {
                pane_status: Some("build ok"),
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(out.contains("[scroll 3/10] │ build ok"), "{out:?}");
        let last_row_clear = format!("\x1b[{};1H\x1b[K", next.content.rows);
        assert!(
            out.contains(&last_row_clear),
            "scroll chrome CSI-K last row: {out:?}"
        );
    }

    #[test]
    fn compose_paint_skips_letter_when_depth_zero() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState::default();
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ))
        .unwrap();
        assert!(!out.contains("38;2;255;180;84"), "{out:?}");
        assert!(out.contains("pmux-attach"), "title still set: {out:?}");
        assert!(!out.contains("mail"), "{out:?}");
    }

    #[test]
    fn compose_paint_held_chip_is_corner_overlay_and_restores_caret() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState::default();
        assert!(!scroll.active);
        assert!(next.content.cursor_visible);
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome {
                lease_held: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(out.contains("[held]"), "{out:?}");
        let last_row_clear = format!("\x1b[{};1H\x1b[K", next.content.rows);
        assert!(
            !out.contains(&last_row_clear),
            "must not CSI-K the guest last row: {out:?}"
        );
        let last_toggle = out
            .rmatch_indices("\x1b[?25")
            .next()
            .map(|(i, _)| out.get(i..i + 6).unwrap_or(""));
        assert_eq!(
            last_toggle,
            Some("\x1b[?25h"),
            "last cursor toggle: {out:?}"
        );
    }

    #[test]
    fn compose_paint_sync_chip_is_corner_overlay_and_restores_caret() {
        let next = frame(1, &["hello", "world"]);
        let scroll = ScrollState::default();
        assert!(!scroll.active);
        assert!(next.content.cursor_visible);
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome {
                sync_input: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(out.contains("[sync]"), "{out:?}");
        let last_row_clear = format!("\x1b[{};1H\x1b[K", next.content.rows);
        assert!(
            !out.contains(&last_row_clear),
            "must not CSI-K the guest last row: {out:?}"
        );
        let last_toggle = out
            .rmatch_indices("\x1b[?25")
            .next()
            .map(|(i, _)| out.get(i..i + 6).unwrap_or(""));
        assert_eq!(
            last_toggle,
            Some("\x1b[?25h"),
            "last cursor toggle: {out:?}"
        );
    }

    #[test]
    fn compose_paint_letter_covers_upper_left_and_keeps_guest_tail() {
        let cols = 16;
        let rows = 4;
        let next = styled_full_width_frame(cols, rows, 'Z');
        let scroll = ScrollState::default();
        let bytes = compose_paint(
            None,
            &next,
            &scroll,
            2,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(" — 2 mail"), "{text}");
        let mut emu = Emulator::new(cols, rows, 0);
        emu.feed(&bytes);
        let cells = mail_letter_cells();
        for (row, line) in cells.iter().enumerate() {
            let row_cells = emu.screen().row(row).expect("row");
            for (col, ch) in line.iter().enumerate() {
                assert_eq!(row_cells[col].character, *ch, "overlay {row},{col}");
            }
            assert_eq!(
                row_cells.last().map(|cell| cell.character),
                Some('Z'),
                "guest tail on row {row}"
            );
        }
        let later = emu.screen().row(3).expect("row 3");
        assert_eq!(later[0].character, 'Z');
    }

    #[test]
    fn incremental_dirty_row_reapplies_mail_letter() {
        let prev = styled_full_width_frame(16, 4, '.');
        let next = styled_full_width_frame(16, 4, 'Z');
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(16, 4, 0);
        emu.feed(&compose_paint(
            None,
            &prev,
            &scroll,
            1,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        emu.feed(&compose_paint(
            Some(&prev),
            &next,
            &scroll,
            1,
            Some(1),
            None,
            None,
            AttachChrome::default(),
        ));
        let want = mail_letter_cells()[0][0];
        assert_eq!(
            emu.screen().row(0).unwrap()[0].character,
            want,
            "overlay survives incremental guest rewrite"
        );
        assert_eq!(emu.screen().row(0).unwrap()[MAIL_CELL_COLS].character, 'Z');
    }

    #[test]
    fn one_by_one_grid_still_paints_mail_letter() {
        let next = PaintFrame::from_plain(PaneContent {
            pane_id: 1,
            revision: 1,
            cols: 1,
            rows: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            alt_active: false,
            child_alive: true,
            child_pid: None,
            lines: vec![".".into()],
            cursor_shape: None,
        });
        let scroll = ScrollState::default();
        let out = String::from_utf8(compose_paint(
            None,
            &next,
            &scroll,
            1,
            None,
            None,
            None,
            AttachChrome::default(),
        ))
        .unwrap();
        assert!(out.contains('\u{F0E0}'), "{out:?}");
        assert!(out.contains("38;2;255;180;84"), "{out:?}");
    }

    #[test]
    fn apply_mail_event_tracks_this_pane_only() {
        let mut depth = 0;
        assert!(!apply_mail_event(
            7,
            &Event::MailAttentionChanged {
                pane_id: 8,
                cell: None,
                gen: None,
                queue_rev: Some(1),
                depth: 3,
                wake: None,
            },
            &mut depth,
        ));
        assert_eq!(depth, 0);
        assert!(apply_mail_event(
            7,
            &Event::MailAttentionChanged {
                pane_id: 7,
                cell: None,
                gen: None,
                queue_rev: Some(1),
                depth: 3,
                wake: None,
            },
            &mut depth,
        ));
        assert_eq!(depth, 3);
        assert!(!apply_mail_event(
            7,
            &Event::MailAttentionChanged {
                pane_id: 7,
                cell: None,
                gen: None,
                queue_rev: Some(2),
                depth: 3,
                wake: None,
            },
            &mut depth,
        ));
        assert!(apply_mail_event(
            7,
            &Event::MailAttentionChanged {
                pane_id: 7,
                cell: None,
                gen: None,
                queue_rev: Some(3),
                depth: 0,
                wake: None,
            },
            &mut depth,
        ));
        assert_eq!(depth, 0);
    }

    #[test]
    fn apply_attention_event_tracks_this_pane_and_emits_safe_osc9() {
        let mut attention = None;
        assert!(!apply_attention_event(
            7,
            &Event::PaneAttention {
                pane_id: 8,
                message: "other".into(),
            },
            &mut attention,
        ));
        assert_eq!(attention, None);
        assert!(apply_attention_event(
            7,
            &Event::PaneAttention {
                pane_id: 7,
                message: "needs input".into(),
            },
            &mut attention,
        ));
        assert_eq!(attention.as_deref(), Some("needs input"));
        assert!(apply_attention_event(
            7,
            &Event::PaneAttentionCleared { pane_id: 7 },
            &mut attention,
        ));
        assert_eq!(attention, None);
        assert!(!apply_attention_event(
            7,
            &Event::PaneAttentionCleared { pane_id: 7 },
            &mut attention,
        ));
        assert_eq!(
            attention_osc_bytes("needs input"),
            Some(b"\x1b]9;needs input\x07".to_vec())
        );
        assert_eq!(attention_osc_bytes("bad\nmessage"), None);
    }
    fn overlay_at(kind: OverlayKind, row: i32, col: u16, text: &str) -> PaneOverlay {
        PaneOverlay {
            id: 1,
            kind,
            row,
            col,
            rows: 1,
            cols: text.chars().count() as u16,
            text: text.into(),
            runs: Vec::new(),
        }
    }

    #[test]
    fn attach_toast_text_names_the_session() {
        assert_eq!(
            attach_toast_text(Some("operator-a")),
            " session operator-a is attached "
        );
        assert_eq!(attach_toast_text(None), " session is attached ");
        assert_eq!(attach_toast_text(Some("")), " session is attached ");
    }

    #[test]
    fn attach_toast_overlay_uses_terminal_width_and_clamps() {
        let text = attach_toast_text(Some("operator-a"));
        let overlay = attach_toast_overlay(&text, 100);
        assert_eq!(overlay.kind, OverlayKind::Viewport);
        assert_eq!(overlay.row, 0);
        assert_eq!(overlay.cols as usize, text.chars().count());
        assert_eq!(overlay.col, 68);

        let narrow = attach_toast_overlay(&text, 20);
        assert_eq!(narrow.col, 0);
    }

    #[test]
    fn parse_focus_border_rgb_accepts_host_names_and_indexes() {
        assert_eq!(
            parse_focus_border_rgb("blue"),
            Some(DEFAULT_TOAST_FOCUS_RGB)
        );
        assert_eq!(parse_focus_border_rgb("4"), Some(DEFAULT_TOAST_FOCUS_RGB));
        assert_eq!(parse_focus_border_rgb("violet"), Some([0x9b, 0x8c, 0xf5]));
        assert_eq!(parse_focus_border_rgb("ink"), Some([0xd0, 0xd0, 0xd0]));
        assert_eq!(parse_focus_border_rgb("nope"), None);
    }

    #[test]
    fn attach_toast_paints_to_terminal_width_beyond_frame_and_clips_narrow() {
        let text = attach_toast_text(Some("operator-a"));
        let mut frame = frame(1, &["hello", "world"]);
        frame.content.cols = 80;

        let wide = attach_toast_overlay(&text, 100);
        let mut wide_bytes = Vec::new();
        append_attach_toast(&mut wide_bytes, &frame, Some(&wide), 100);
        let wide_output = String::from_utf8_lossy(&wide_bytes);
        assert!(wide_output.contains("\x1b[1;69H"), "{wide_output:?}");
        assert!(wide_output.contains("\x1b[1;100H"), "{wide_output:?}");

        let narrow = attach_toast_overlay(&text, 20);
        let mut narrow_bytes = Vec::new();
        append_attach_toast(&mut narrow_bytes, &frame, Some(&narrow), 20);
        let narrow_output = String::from_utf8_lossy(&narrow_bytes);
        assert!(narrow_output.contains("\x1b[1;1H"), "{narrow_output:?}");
        assert!(!narrow_output.contains("\x1b[1;21H"), "{narrow_output:?}");
    }

    #[test]
    fn attach_toast_paints_on_alt_screen() {
        let mut next = frame(1, &["hello", "world"]);
        next.content.alt_active = true;
        next.content.cols = 40;
        next.overlays = vec![overlay_at(OverlayKind::Viewport, 1, 0, "APP")];
        let toast = attach_toast_overlay(&attach_toast_text(Some("work")), 40);
        let scroll = ScrollState::default();
        let bytes = compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            Some("work"),
            Some(&toast),
            AttachChrome::default(),
        );
        let out = String::from_utf8_lossy(&bytes);
        assert!(!out.contains("APP"), "app HUD must stay off alt: {out:?}");
        assert!(out.contains("pmux: work"), "{out:?}");
        assert!(
            out.contains("48;2;98;168;255"),
            "default focus-border blue chip: {out:?}"
        );
        assert!(
            !out.contains("\u{1b}[7m"),
            "toast is focus-border fill, not inverse"
        );
        let mut emu = Emulator::new(40, 2, 0);
        emu.feed(&bytes);
        let row: String = emu
            .screen()
            .row(0)
            .unwrap()
            .iter()
            .map(|cell| cell.character)
            .collect();
        assert!(
            row.contains("session work is attached"),
            "toast missing from alt row: {row:?}"
        );
    }

    #[test]
    fn flag_off_empty_overlays_keep_paint_bytes_identical() {
        let next = frame(1, &["hello", "world"]);
        assert!(next.overlays.is_empty());
        let scroll = ScrollState::default();
        let composed = compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        );
        let guest = paint_bytes(None, &next, None);
        assert!(
            String::from_utf8_lossy(&composed).contains("hello"),
            "{composed:?}"
        );
        assert_eq!(&composed[..guest.len()], guest.as_slice());
    }

    #[test]
    fn cell_rect_overlay_paints_clipped_text() {
        let mut next = styled_full_width_frame(8, 3, '.');
        next.overlays = vec![overlay_at(OverlayKind::CellRect, 0, 2, "STAT")];
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(8, 3, 0);
        emu.feed(&compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        let row = emu.screen().row(0).unwrap();
        assert_eq!(row[2].character, 'S');
        assert_eq!(row[3].character, 'T');
        assert_eq!(row[4].character, 'A');
        assert_eq!(row[5].character, 'T');
        assert_eq!(row[0].character, '.');
        assert_eq!(row[7].character, '.');
    }

    #[test]
    fn viewport_paints_above_cell_rect() {
        let mut next = styled_full_width_frame(8, 2, '.');
        next.overlays = vec![
            overlay_at(OverlayKind::CellRect, 0, 0, "AAAA"),
            overlay_at(OverlayKind::Viewport, 0, 0, "HUD"),
        ];
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(8, 2, 0);
        emu.feed(&compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        let row = emu.screen().row(0).unwrap();
        assert_eq!(row[0].character, 'H');
        assert_eq!(row[1].character, 'U');
        assert_eq!(row[2].character, 'D');
        assert_eq!(row[3].character, 'A');
    }

    #[test]
    fn history_pan_hides_cell_rect_keeps_viewport() {
        let mut next = styled_full_width_frame(8, 2, '.');
        next.view_offset = Some(3);
        next.overlays = vec![
            overlay_at(OverlayKind::CellRect, 0, 0, "STAT"),
            overlay_at(OverlayKind::Viewport, 0, 4, "HUD"),
        ];
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(8, 2, 0);
        emu.feed(&compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        let row = emu.screen().row(0).unwrap();
        assert_eq!(row[0].character, '.');
        assert_eq!(row[4].character, 'H');
        assert_eq!(row[5].character, 'U');
        assert_eq!(row[6].character, 'D');
    }

    #[test]
    fn alt_screen_skips_rich_overlays() {
        let mut next = styled_full_width_frame(8, 2, '.');
        next.content.alt_active = true;
        next.overlays = vec![overlay_at(OverlayKind::CellRect, 0, 0, "STAT")];
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(8, 2, 0);
        emu.feed(&compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        assert_eq!(emu.screen().row(0).unwrap()[0].character, '.');
    }

    #[test]
    fn overlay_clips_past_attach_cols() {
        let mut next = styled_full_width_frame(4, 1, '.');
        next.overlays = vec![overlay_at(OverlayKind::CellRect, 0, 2, "STAT")];
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(4, 1, 0);
        emu.feed(&compose_paint(
            None,
            &next,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        let row = emu.screen().row(0).unwrap();
        assert_eq!(row[2].character, 'S');
        assert_eq!(row[3].character, 'T');
        assert_eq!(row.len(), 4);
    }

    #[test]
    fn incremental_dirty_row_reapplies_rich_overlay() {
        let mut prev = styled_full_width_frame(8, 2, '.');
        prev.overlays = vec![overlay_at(OverlayKind::CellRect, 0, 0, "STAT")];
        let mut next = styled_full_width_frame(8, 2, 'Z');
        next.overlays = prev.overlays.clone();
        let scroll = ScrollState::default();
        let mut emu = Emulator::new(8, 2, 0);
        emu.feed(&compose_paint(
            None,
            &prev,
            &scroll,
            0,
            None,
            None,
            None,
            AttachChrome::default(),
        ));
        emu.feed(&compose_paint(
            Some(&prev),
            &next,
            &scroll,
            0,
            Some(0),
            None,
            None,
            AttachChrome::default(),
        ));
        let row = emu.screen().row(0).unwrap();
        assert_eq!(row[0].character, 'S');
        assert_eq!(row[3].character, 'T');
        assert_eq!(row[4].character, 'Z');
    }

    #[test]
    fn log_paint_wake_dies_on_missing_socket() {
        let wake = LogPaintWake::start(PathBuf::from("/no/such/prismattyc-pt113.sock"), 1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if wake.is_dead() || wake.drain().1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("subscriber on a missing socket must exit so the TTY loop can fall back");
    }
}

#[cfg(windows)]
fn local_winsize() -> Option<LocalWinsize> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let (cols, rows) = normalize_winsize(cols, rows)?;
    Some(LocalWinsize {
        cols,
        rows,
        cell_width_px: prismattyc_emulator::NOMINAL_CELL_W_PX,
        cell_height_px: prismattyc_emulator::NOMINAL_CELL_H_PX,
    })
}
