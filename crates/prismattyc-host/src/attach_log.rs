//! Log-backed attach pane (PT-111).
//!
//! The windowed host used to attach a pmuxd session by spawning
//! `pmux attach --session-id ID` as a PTY child and running a second
//! emulator over the ANSI that child painted. This module replaces that
//! child with two control-plane connections:
//!
//! * a **reader** connection parked in `SubscribePane`, which turns the
//!   per-pane event log into [`LogMessage`]s for [`crate::mux::PaneRuntime`];
//! * a **writer** connection that sends key bytes with `WritePane` under the
//!   controller lease (acquired on the first key, released after
//!   [`LEASE_IDLE`]) and pushes host geometry to the server with `Resize`.
//!
//! The pane's own [`prismattyc_emulator::Emulator`] applies `Output` and
//! `Resize` in log order, so the host paints a replica of the server screen
//! with real scrollback. The replica never forwards
//! `take_pending_replies()`: DSR/CPR/DA answers belong to the PTY owner.
//!
//! Set `PRISMATTYC_ATTACH_PTY=1` to fall back to the nested PTY child.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use prismattyc_mux::{
    classify_control_request_id, default_socket_path, next_stale_skip, ControlError,
    ControlErrorCode, ControlIdMatch, ControlRequest, ControlResponse, ControlResponseBody,
    ControlResponseData, PaneEvent, PaneFramePolicy, PaneLogFrame, PaneStyled, Snapshot,
    PROTOCOL_VERSION,
};

use crate::rich::ChildWrite;

/// Idle gap after the last key before the controller lease is released.
/// Same value as `pmux attach`: a sticky lease makes every live agent pane
/// `deferred_lease` for `InjectMail`.
const LEASE_IDLE: Duration = Duration::from_millis(750);
/// Server-side idle wait per `SubscribePane` request. The server answers
/// `done` after this and the reader immediately re-subscribes from
/// `through_seq`, so nothing is lost across the gap.
const SUBSCRIBE_TIMEOUT_MS: u32 = 2_000;
/// Socket read budget: the subscribe wait plus slack for a large frame.
const SUBSCRIBE_READ_TIMEOUT: Duration = Duration::from_millis(SUBSCRIBE_TIMEOUT_MS as u64 + 5_000);
/// Socket read/write budget for one host attach control request.
/// Hang-prevention, not an operation SLA.
/// Measured 2026-09-07 on this box: 200 Snapshot round-trips on a local
/// pmuxd unix socket (same `Client::request` path `tests/native/spaces-e2e.sh` uses):
/// p50 = 0.15 ms, p99 = 0.55 ms. 2 s is >3000× that p99 so a loaded host
/// still returns before the writer tick (100 ms) looks wedged, without
/// waiting `SUBSCRIBE_READ_TIMEOUT`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Writer wakeup cadence. Bounds lease-release and resize latency when no
/// key is typed.
const WRITER_TICK: Duration = Duration::from_millis(100);
/// Per-stream host heap admission for decoded attach responses.
pub(crate) const HOST_EVENT_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Fixed host queue record cost charged with each encoded response.
const HOST_RECORD_OVERHEAD: usize = 64;
/// Opt-in flood-box override. Zero means unlimited for the control run.
const HOST_EVENT_BUDGET_ENV: &str = "PRISMATTYC_HOST_EVENT_BUDGET_BYTES";

fn host_event_budget_bytes() -> usize {
    match std::env::var(HOST_EVENT_BUDGET_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(bytes) => bytes,
        None => HOST_EVENT_BUDGET_BYTES,
    }
}

#[derive(Clone)]
struct HostByteBudget {
    inner: Arc<HostByteBudgetInner>,
}

#[derive(Debug)]
struct HostByteBudgetInner {
    reserved: Mutex<usize>,
    available: Condvar,
    cap: usize,
    high_water: AtomicUsize,
    blocked_ms: AtomicU64,
}

/// Exact host attach reader-queue measurements for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HostQueueStats {
    pub(crate) budget_bytes: usize,
    pub(crate) queued_bytes: usize,
    pub(crate) high_water_bytes: usize,
    pub(crate) reader_blocked_ms: u64,
}

#[derive(Debug)]
struct HostReservationPart {
    budget: Arc<HostByteBudgetInner>,
    bytes: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct HostReservation(Vec<HostReservationPart>);

impl HostByteBudget {
    fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(HostByteBudgetInner {
                reserved: Mutex::new(0),
                available: Condvar::new(),
                cap,
                high_water: AtomicUsize::new(0),
                blocked_ms: AtomicU64::new(0),
            }),
        }
    }

    fn reserve(&self, bytes: usize) -> Option<HostReservationPart> {
        if bytes > self.inner.cap {
            return None;
        }
        let mut reserved = self
            .inner
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while reserved.saturating_add(bytes) > self.inner.cap {
            let started = Instant::now();
            reserved = self
                .inner
                .available
                .wait(reserved)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.inner.blocked_ms.fetch_add(
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        *reserved = reserved.saturating_add(bytes);
        self.inner
            .high_water
            .fetch_max(*reserved, Ordering::Relaxed);
        Some(HostReservationPart {
            budget: Arc::clone(&self.inner),
            bytes,
        })
    }

    fn stats(&self) -> HostQueueStats {
        HostQueueStats {
            budget_bytes: self.inner.cap,
            queued_bytes: *self
                .inner
                .reserved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            high_water_bytes: self.inner.high_water.load(Ordering::Relaxed),
            reader_blocked_ms: self.inner.blocked_ms.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    fn admit<T>(
        &self,
        bytes: usize,
        allocate: impl FnOnce() -> T,
    ) -> Option<(T, HostReservationPart)> {
        let reservation = self.reserve(bytes)?;
        Some((allocate(), reservation))
    }

    #[cfg(test)]
    fn reserved(&self) -> usize {
        *self
            .inner
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for HostReservationPart {
    fn drop(&mut self) {
        let mut reserved = self
            .budget
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *reserved = reserved.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

/// `PRISMATTYC_ATTACH_PTY=1` keeps the pre-PT-111 nested `pmux attach` child.
pub(crate) fn pty_fallback_requested() -> bool {
    matches!(
        std::env::var("PRISMATTYC_ATTACH_PTY").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Session key of a host attach spawn (PT-111 / PT-306).
///
/// Matches `pmux attach --session-id ID`, `pmux attach --session NAME`,
/// `pmux attach NAME`, and the same selectors on `pmux-attach`. Dump,
/// write, and `--all` argv stay ordinary PTY panes.
pub(crate) fn attach_target(program: &str, args: &[String]) -> Option<String> {
    let stem = std::path::Path::new(program).file_name()?.to_str()?;
    match stem {
        "pmux" => {
            if args.first().map(String::as_str) != Some("attach") {
                return None;
            }
            attach_session_key(&args[1..])
        }
        "pmux-attach" => attach_session_key(args),
        _ => None,
    }
}

fn attach_session_key(args: &[String]) -> Option<String> {
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--all"
                | "--json"
                | "--styled-json"
                | "--watch"
                | "--write"
                | "--pane"
                | "--read-only"
                | "--fit"
        )
    }) {
        return None;
    }
    let mut session_id = None;
    let mut session = None;
    let mut positional = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session-id" => {
                let value = iter.next()?;
                if !value.is_empty() {
                    session_id = Some(value.clone());
                }
            }
            "--session" => {
                let value = iter.next()?;
                if !value.is_empty() {
                    session = Some(value.clone());
                }
            }
            "--socket" | "--space" | "--create-session" => {
                let _ = iter.next()?;
            }
            flag if flag.starts_with('-') => {}
            value if positional.is_none() => positional = Some(value.to_string()),
            _ => {}
        }
    }
    session_id.or(session).or(positional)
}

/// Socket the host attaches: `PMUX_SOCKET` if set, else the default.
fn mux_socket() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os("PMUX_SOCKET") {
        if !raw.is_empty() {
            return Ok(PathBuf::from(raw));
        }
    }
    default_socket_path("default").context("default pmux socket path")
}

/// Live session id → stable name, from a fresh daemon snapshot. Regroup
/// matches sessions by name, not by the ephemeral id a pane attached under
/// (PT-108 open-side mirror). Any failure yields an empty map, so regroup
/// falls back to id matching — no worse than before.
pub(crate) fn session_names() -> HashMap<String, String> {
    live_snapshot()
        .map(|snapshot| {
            snapshot
                .sessions
                .iter()
                .map(|session| (session.id.to_string(), session.name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Live sessions with their pane ids, for adopting nested attaches
/// (PT-210). Empty on any failure.
pub(crate) fn session_directory() -> Vec<crate::attach_adopt::SessionEntry> {
    live_snapshot()
        .map(session_directory_from_snapshot)
        .unwrap_or_default()
}

/// Live sessions from an explicit socket. Tests use this to keep their
/// adoption fixture isolated from the developer's default pmuxd.
#[cfg(test)]
pub(crate) fn session_directory_at(socket: &Path) -> Vec<crate::attach_adopt::SessionEntry> {
    live_snapshot_at(socket)
        .map(session_directory_from_snapshot)
        .unwrap_or_default()
}

fn session_directory_from_snapshot(snapshot: Snapshot) -> Vec<crate::attach_adopt::SessionEntry> {
    snapshot
        .sessions
        .iter()
        .map(|session| crate::attach_adopt::SessionEntry {
            id: session.id,
            name: session.name.clone(),
            pane_ids: session
                .windows
                .iter()
                .flat_map(|window| window.panes.iter().map(|pane| pane.id))
                .collect(),
        })
        .collect()
}

/// One `Snapshot` request on a fresh connection; `None` on any failure.
pub(crate) fn live_snapshot() -> Option<Snapshot> {
    let socket = mux_socket().ok()?;
    live_snapshot_at(&socket)
}

fn live_snapshot_at(socket: &Path) -> Option<Snapshot> {
    let mut client = Client::connect(socket, REQUEST_TIMEOUT).ok()?;
    match client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        Ok(ControlResponseData::Snapshot { snapshot }) => Some(snapshot),
        _ => None,
    }
}

/// One NDJSON control connection with its own registered client identity.
/// Each connection must register its own client (the server rejects a
/// `client_id` claimed by another connection), so a log-backed pane holds
/// two.
struct Client {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_request_id: u64,
    client_id: u64,
}

impl Client {
    fn connect(path: &Path, read_timeout: Duration) -> Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("connect {}", path.display()))?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_request_id: 1,
            client_id: 0,
        };
        let registered = client.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })?;
        let ControlResponseData::ClientRegistered { client_id } = registered else {
            bail!("server returned an unexpected registration response");
        };
        client.client_id = client_id;
        Ok(client)
    }

    fn send(&mut self, request: &ControlRequest) -> Result<()> {
        serde_json::to_writer(&mut self.writer, request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_frame(&mut self, request_id: u64) -> Result<ControlResponseData> {
        let mut skipped = 0u32;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("server closed the pane connection");
            }
            let response: ControlResponse = serde_json::from_str(&line)?;
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => {
                    return match response.body {
                        ControlResponseBody::Ok { response } => Ok(response),
                        ControlResponseBody::Error { error } => Err(anyhow::Error::new(error)),
                    };
                }
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

    fn read_frame_budgeted(
        &mut self,
        request_id: u64,
        budget: &HostByteBudget,
    ) -> Result<(ControlResponseData, HostReservation)> {
        let mut skipped = 0u32;
        loop {
            let mut reservations = vec![budget
                .reserve(HOST_RECORD_OVERHEAD)
                .context("host response overhead exceeds byte budget")?];
            let mut line = Vec::new();
            loop {
                let (take, complete) = {
                    let buffer = self.reader.fill_buf()?;
                    if buffer.is_empty() {
                        bail!("server closed the pane connection");
                    }
                    let newline = buffer.iter().position(|byte| *byte == b'\n');
                    (
                        newline.map_or(buffer.len(), |index| index + 1),
                        newline.is_some(),
                    )
                };
                let reservation = budget
                    .reserve(take)
                    .context("encoded response exceeds host byte budget")?;
                let buffer = self.reader.fill_buf()?;
                line.extend_from_slice(&buffer[..take]);
                self.reader.consume(take);
                reservations.push(reservation);
                if complete {
                    break;
                }
            }
            let response: ControlResponse = serde_json::from_slice(&line)?;
            match classify_control_request_id(response.request_id, request_id) {
                ControlIdMatch::Awaited => {
                    return match response.body {
                        ControlResponseBody::Ok { response } => {
                            Ok((response, HostReservation(reservations)))
                        }
                        ControlResponseBody::Error { error } => Err(anyhow::Error::new(error)),
                    };
                }
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

    fn next_id(&mut self) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("request ID space exhausted")?;
        Ok(request_id)
    }

    fn request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> Result<ControlResponseData> {
        let request_id = self.next_id()?;
        let request = make(request_id);
        self.send(&request)?;
        self.read_frame(request_id)
    }
}

fn error_code(error: &anyhow::Error) -> Option<ControlErrorCode> {
    error.downcast_ref::<ControlError>().map(|err| err.code)
}

/// A socket read that timed out or was interrupted, not a server verdict
/// or a closed connection. The server and the pane are usually fine: the
/// host or pmuxd was starved (swap, a VM starting) for longer than
/// [`SUBSCRIBE_READ_TIMEOUT`]. Re-subscribe instead of ending the pane
/// (PT-135).
fn transient_read_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
        )
    })
}

/// Consecutive transient read failures tolerated before the stream is
/// declared ended; each retry reconnects and waits `n × 250 ms` first.
const SUBSCRIBE_RETRIES: u32 = 5;
const SUBSCRIBE_RETRY_STEP: Duration = Duration::from_millis(250);

/// Policy counters for frames superseded before host delivery.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PolicyCounters {
    pub(crate) superseded_frames: usize,
    pub(crate) superseded_bytes: usize,
}

/// What the reader thread hands the host thread, in log order.
#[derive(Debug)]
pub(crate) enum LogMessage {
    /// One subscribe response, with its byte reservation held until consumed.
    Batch {
        snapshot: Option<Box<PaneStyled>>,
        events: Vec<PaneLogFrame>,
        /// Output at or before this sequence restores state without alerts.
        replay_through: u64,
        reservation: HostReservation,
        counters: PolicyCounters,
    },
    /// StaleSequence: drop replica history before replaying from seq 1.
    Reset,
    /// The server pane is gone or the stream died. Drives PT-68 placeholder.
    Ended { reason: Option<String> },
    /// Writer `WritePane` failed. Pane stays read-only; toast the host.
    WriteFailed { reason: String },
}

/// Toast chip after [`LogMessage::WriteFailed`]. Not a BEL; not gated on
/// `bell_toaster` (PT-119). Spaces match other chips.
pub(crate) const WRITE_FAILED_TOAST: &str = " input disconnected — reopen the pane ";

/// Host geometry waiting to go to the server as `Resize`. Latest wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingResize {
    cols: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
}

/// Resolved server-side identity of an attached session, held open just
/// long enough to fail fast before the PTY fallback is given up.
pub(crate) struct LogConnection {
    pub(crate) initial_size: (usize, usize),
    socket: PathBuf,
    session_key: String,
    space_id: Option<String>,
    pane_id: u64,
    window_id: u64,
    /// Server pane title and pin at open (PT-230). Applied before log drain.
    pub(crate) pane_title: String,
    pub(crate) title_pinned: bool,
}

impl LogConnection {
    /// Resolve `session_key` to its first pane and window. Blocking, with a
    /// 2 s budget; any failure means the caller keeps the PTY child.
    pub(crate) fn open(session_key: &str) -> Result<Self> {
        Self::open_on(&mux_socket()?, session_key)
    }

    /// Same as [`Self::open`] on an explicit socket (adopt / tests).
    pub(crate) fn open_on(socket: &Path, session_key: &str) -> Result<Self> {
        let mut client = Client::connect(socket, REQUEST_TIMEOUT)?;
        let snapshot = client.request(|request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        })?;
        let ControlResponseData::Snapshot { snapshot } = snapshot else {
            bail!("server returned an unexpected snapshot response");
        };
        let found = session_pane(&snapshot, session_key)?;
        Ok(Self {
            initial_size: found.size,
            socket: socket.to_path_buf(),
            session_key: found.session_id,
            space_id: found.space_id,
            pane_id: found.pane_id,
            window_id: found.window_id,
            pane_title: found.title,
            title_pinned: found.title_pinned,
        })
    }

    pub(crate) fn exact_pane(session: &str, pane: u64, pid: u32) -> Result<Self> {
        let socket = mux_socket()?;
        let snapshot = live_snapshot_at(&socket).context("daemon unavailable")?;
        let session = snapshot
            .sessions
            .iter()
            .find(|s| s.id.to_string() == session)
            .context("session disappeared")?;
        let (window, target) = session
            .windows
            .iter()
            .find_map(|w| {
                w.panes
                    .iter()
                    .find(|p| p.id == pane && p.child_pid == Some(pid))
                    .map(|p| (w, p))
            })
            .context("pane moved or its child changed")?;
        Ok(Self {
            initial_size: (target.geometry.cols as usize, target.geometry.rows as usize),
            socket,
            session_key: session.id.to_string(),
            space_id: session.space_id.clone(),
            pane_id: pane,
            window_id: window.id,
            pane_title: target.title.clone(),
            title_pinned: target.title_pinned,
        })
    }

    pub(crate) fn require_space(&self, expected: &str) -> Result<()> {
        if self.space_id.as_deref() != Some(expected) {
            bail!("session belongs to another Space");
        }
        Ok(())
    }

    pub(crate) fn session_key(&self) -> &str {
        &self.session_key
    }

    /// Start the reader and writer threads. `to_child_rx` is the same
    /// channel a PTY pane uses, so every host input path is unchanged.
    pub(crate) fn into_pane(
        self,
        cols: usize,
        rows: usize,
        cell_w: usize,
        cell_h: usize,
        to_child_rx: mpsc::Receiver<ChildWrite>,
        wake: Option<crate::mux::Wake>,
    ) -> Result<LogPane> {
        let (events_tx, events_rx) = mpsc::channel::<LogMessage>();
        let event_budget = HostByteBudget::new(host_event_budget_bytes());
        let resize = Arc::new(Mutex::new(Some(PendingResize {
            cols: u32::try_from(cols).unwrap_or(u32::MAX),
            rows: u32::try_from(rows).unwrap_or(u32::MAX),
            cell_w: u32::try_from(cell_w).unwrap_or(1).max(1),
            cell_h: u32::try_from(cell_h).unwrap_or(1).max(1),
        })));

        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_socket = self.socket.clone();
        let pane_id = self.pane_id;
        let reader_wake = wake.clone();
        let reader_tx = events_tx.clone();
        let reader_budget = event_budget.clone();
        thread::Builder::new()
            .name(format!("prism-log-{pane_id}-read"))
            .spawn(move || {
                let reason = reader_loop(
                    &reader_socket,
                    pane_id,
                    &reader_tx,
                    reader_wake.as_ref(),
                    &reader_stop,
                    &reader_budget,
                );
                let _ = reader_tx.send(LogMessage::Ended { reason });
                if let Some(wake) = reader_wake.as_ref() {
                    wake();
                }
            })?;

        let writer_socket = self.socket.clone();
        let window_id = self.window_id;
        let writer_session = self.session_key.clone();
        let writer_space = self.space_id.clone();
        let writer_resize = Arc::clone(&resize);
        let writer_wake = wake;
        thread::Builder::new()
            .name(format!("prism-log-{pane_id}-write"))
            .spawn(move || {
                writer_loop(
                    &writer_socket,
                    pane_id,
                    window_id,
                    &writer_session,
                    writer_space.as_deref(),
                    &to_child_rx,
                    &writer_resize,
                    &events_tx,
                    writer_wake.as_ref(),
                );
            })?;

        Ok(LogPane {
            pane_id,
            events_rx,
            event_budget,
            resize,
            stop,
            session_key: self.session_key,
        })
    }
}

/// Host-side handle on a log-backed pane.
pub(crate) struct LogPane {
    pub(crate) pane_id: u64,
    events_rx: mpsc::Receiver<LogMessage>,
    event_budget: HostByteBudget,
    resize: Arc<Mutex<Option<PendingResize>>>,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    session_key: String,
}

impl Drop for LogPane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl LogPane {
    pub(crate) fn try_recv(&self) -> Result<LogMessage, mpsc::TryRecvError> {
        self.events_rx.try_recv()
    }

    pub(crate) fn queue_stats(&self) -> HostQueueStats {
        self.event_budget.stats()
    }

    /// AC4: host geometry change goes to the server, which logs a `Resize`
    /// at its byte position; the replica follows when that event arrives.
    pub(crate) fn request_resize(&self, cols: usize, rows: usize, cell_w: usize, cell_h: usize) {
        let pending = PendingResize {
            cols: u32::try_from(cols).unwrap_or(u32::MAX),
            rows: u32::try_from(rows).unwrap_or(u32::MAX),
            cell_w: u32::try_from(cell_w).unwrap_or(1).max(1),
            cell_h: u32::try_from(cell_h).unwrap_or(1).max(1),
        };
        if let Ok(mut slot) = self.resize.lock() {
            *slot = Some(pending);
        }
    }
}

/// First window/pane of the session named or identified by `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPaneRef {
    size: (usize, usize),
    session_id: String,
    space_id: Option<String>,
    pane_id: u64,
    window_id: u64,
    title: String,
    title_pinned: bool,
}

fn session_pane(snapshot: &Snapshot, key: &str) -> Result<SessionPaneRef> {
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id.to_string() == key || session.name == key)
        .with_context(|| format!("no mux session matching {key:?}"))?;
    let window = session
        .windows
        .first()
        .context("selected session has no window")?;
    let pane = window
        .panes
        .first()
        .context("selected window has no pane")?;
    Ok(SessionPaneRef {
        size: (pane.geometry.cols as usize, pane.geometry.rows as usize),
        session_id: session.id.to_string(),
        space_id: session.space_id.clone(),
        pane_id: pane.id,
        window_id: window.id,
        title: pane.title.clone(),
        title_pinned: pane.title_pinned,
    })
}

/// Server title and pin for a session key (PT-230).
pub(crate) fn session_title_pin(snapshot: &Snapshot, key: &str) -> Option<(String, bool)> {
    session_pane(snapshot, key)
        .ok()
        .map(|found| (found.title, found.title_pinned))
}

/// Park in `SubscribePane`, forwarding every frame. Returns the reason the
/// stream ended, or `None` when the pane simply went away.
fn reader_loop(
    socket: &Path,
    pane_id: u64,
    events_tx: &mpsc::Sender<LogMessage>,
    wake: Option<&crate::mux::Wake>,
    stop: &AtomicBool,
    budget: &HostByteBudget,
) -> Option<String> {
    let mut client = match Client::connect(socket, SUBSCRIBE_READ_TIMEOUT) {
        Ok(client) => client,
        Err(error) => return Some(format!("subscribe connect failed: {error}")),
    };
    let mut replay_through = match pane_replay_boundary(&mut client, pane_id) {
        Ok(sequence) => sequence,
        Err(error) => return Some(format!("pane replay boundary failed: {error}")),
    };
    let mut from_seq = 0u64;
    let mut retries = 0u32;
    'subscribe: loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let request_id = match client.next_id() {
            Ok(id) => id,
            Err(error) => return Some(error.to_string()),
        };
        let request = ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client.client_id,
            pane_id,
            from_seq,
            timeout_ms: SUBSCRIBE_TIMEOUT_MS,
        };
        if let Err(error) = client.send(&request) {
            return Some(format!("subscribe send failed: {error}"));
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let (frame, reservation) = match client.read_frame_budgeted(request_id, budget) {
                Ok(frame) => frame,
                Err(error) => match error_code(&error) {
                    // Ahead of the server: the log was rebuilt. Restart.
                    Some(ControlErrorCode::StaleSequence) => {
                        let Some(current) = error
                            .downcast_ref::<ControlError>()
                            .and_then(|error| error.current_sequence)
                        else {
                            return Some("pane reset omitted its current sequence".into());
                        };
                        if events_tx.send(LogMessage::Reset).is_err() {
                            return None;
                        }
                        from_seq = 0;
                        replay_through = current;
                        continue 'subscribe;
                    }
                    // The pane is gone. PT-68 placeholder takes over.
                    Some(ControlErrorCode::StaleId) => return None,
                    _ if transient_read_error(&error) && retries < SUBSCRIBE_RETRIES => {
                        // A timed-out read may have left a partial frame in
                        // the buffer: reconnect and resume from the last seq
                        // the server acknowledged (its log fills the gap).
                        retries += 1;
                        std::thread::sleep(SUBSCRIBE_RETRY_STEP * retries);
                        if stop.load(Ordering::Relaxed) {
                            return None;
                        }
                        match Client::connect(socket, SUBSCRIBE_READ_TIMEOUT) {
                            Ok(fresh) => client = fresh,
                            Err(error) => {
                                return Some(format!("subscribe reconnect failed: {error}"))
                            }
                        }
                        continue 'subscribe;
                    }
                    _ => return Some(format!("subscribe stream ended: {error}")),
                },
            };
            retries = 0;
            let ControlResponseData::PaneSubscribe {
                gap,
                snapshot,
                events,
                through_seq,
                done,
                ..
            } = frame
            else {
                drop(reservation);
                return Some("unexpected subscribe frame".into());
            };
            from_seq = through_seq;
            let (events, counters) = coalesce_events(events);
            if gap || !events.is_empty() {
                if events_tx
                    .send(LogMessage::Batch {
                        snapshot: gap.then_some(snapshot).flatten().map(Box::new),
                        events,
                        replay_through,
                        reservation,
                        counters,
                    })
                    .is_err()
                {
                    return None;
                }
            } else {
                drop(reservation);
            }
            if let Some(wake) = wake {
                wake();
            }
            if done {
                break;
            }
        }
    }
}

/// Capture the existing log's end before replay starts. The existing protocol
/// reports its current sequence when a subscriber asks beyond the log's end.
/// This avoids changing pmuxd or guessing that the first batch is all history.
fn pane_replay_boundary(client: &mut Client, pane_id: u64) -> Result<u64> {
    let client_id = client.client_id;
    match client.request(|request_id| ControlRequest::SubscribePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        from_seq: u64::MAX,
        timeout_ms: 0,
    }) {
        Err(error) if error_code(&error) == Some(ControlErrorCode::StaleSequence) => error
            .downcast_ref::<ControlError>()
            .and_then(|error| error.current_sequence)
            .context("pane replay boundary omitted its current sequence"),
        // A log whose sequence has reached u64::MAX is already at the probe.
        Ok(ControlResponseData::PaneSubscribe { through_seq, .. }) => Ok(through_seq),
        Ok(_) => bail!("unexpected pane replay boundary response"),
        Err(error) => Err(error),
    }
}

fn coalesce_events(events: Vec<PaneLogFrame>) -> (Vec<PaneLogFrame>, PolicyCounters) {
    let mut pending = Vec::with_capacity(events.len());
    let mut counters = PolicyCounters::default();
    for frame in events {
        let policy = frame.event.frame_policy();
        match policy {
            PaneFramePolicy::Backpressure => pending.push(frame),
            PaneFramePolicy::KeepNewest => {
                let key = std::mem::discriminant(&frame.event);
                let boundary = pending
                    .iter()
                    .rposition(|queued| {
                        queued.event.frame_policy() == PaneFramePolicy::Backpressure
                    })
                    .map_or(0, |index| index + 1);
                if let Some(index) = pending[boundary..]
                    .iter()
                    .rposition(|queued| std::mem::discriminant(&queued.event) == key)
                    .map(|index| index + boundary)
                {
                    counters.superseded_frames = counters.superseded_frames.saturating_add(1);
                    counters.superseded_bytes = counters
                        .superseded_bytes
                        .saturating_add(frame_size(&pending[index]));
                    pending[index] = frame;
                } else {
                    pending.push(frame);
                }
            }
            PaneFramePolicy::ReplaceableInRun => {
                let replace = matches!(frame.event, PaneEvent::Resize { .. })
                    && pending
                        .last()
                        .is_some_and(|queued| matches!(queued.event, PaneEvent::Resize { .. }));
                if replace {
                    counters.superseded_frames = counters.superseded_frames.saturating_add(1);
                    counters.superseded_bytes = counters
                        .superseded_bytes
                        .saturating_add(frame_size(pending.last().unwrap()));
                    *pending.last_mut().unwrap() = frame;
                } else {
                    pending.push(frame);
                }
            }
        }
    }
    (pending, counters)
}

fn frame_size(frame: &PaneLogFrame) -> usize {
    serde_json::to_vec(&frame.event).map_or(0, |encoded| encoded.len())
}

/// pending host geometry as `Resize`. Ends when the pane runtime drops
/// `to_child_tx`.
#[allow(clippy::too_many_arguments)]
fn writer_loop(
    socket: &Path,
    pane_id: u64,
    window_id: u64,
    session: &str,
    space_id: Option<&str>,
    to_child_rx: &mpsc::Receiver<ChildWrite>,
    resize: &Arc<Mutex<Option<PendingResize>>>,
    events_tx: &mpsc::Sender<LogMessage>,
    wake: Option<&crate::mux::Wake>,
) {
    let Ok(mut client) = Client::connect(socket, REQUEST_TIMEOUT) else {
        // Drain so senders never block on a dead writer.
        while to_child_rx.recv().is_ok() {}
        return;
    };
    let client_id = client.client_id;
    if let Some(space_id) = space_id {
        if let Err(error) = client.request(|request_id| ControlRequest::SetClientSpace {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            space_id: space_id.to_string(),
        }) {
            let _ = events_tx.send(LogMessage::WriteFailed {
                reason: format!("Space ownership guard: {error}"),
            });
            if let Some(wake) = wake {
                wake();
            }
            // An older server must never get an unscoped fallback writer.
            while to_child_rx.recv().is_ok() {}
            return;
        }
    }
    let mut controller = false;
    let mut last_typed: Option<Instant> = None;
    let mut last_host_size: Option<PendingResize> = None;
    let mut refit_after_input = false;
    let mut pending: Vec<u8> = Vec::new();
    'outer: loop {
        match to_child_rx.recv_timeout(WRITER_TICK) {
            Ok(msg) => {
                pending.extend_from_slice(&msg.bytes);
                // Coalesce a paste burst before touching the lease.
                while let Ok(more) = to_child_rx.try_recv() {
                    pending.extend_from_slice(&more.bytes);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !pending.is_empty() {
            if !controller && acquire_lease(&mut client, client_id, pane_id) {
                controller = true;
            }
            if controller {
                while let Some(chunk) = take_utf8_prefix(&mut pending) {
                    let mut result = write_pane(&mut client, client_id, pane_id, chunk.clone());
                    if result
                        .as_ref()
                        .is_err_and(|error| error_code(error) == Some(ControlErrorCode::InputDirty))
                    {
                        // A `pmux send --force` took and released the lease
                        // while this writer slept: the flag is stale and the
                        // write went lease-free into the forced partial line
                        // (PT-140). Re-acquire and retry once.
                        controller = acquire_lease(&mut client, client_id, pane_id);
                        if !controller {
                            pending.clear();
                            break;
                        }
                        result = write_pane(&mut client, client_id, pane_id, chunk);
                    }
                    if result
                        .as_ref()
                        .is_err_and(|error| error_code(error) == Some(ControlErrorCode::InputDirty))
                    {
                        // Still dirty after a fresh lease: keep the writer up
                        // (same as pmux-attach write_as_controller).
                        controller = false;
                        pending.clear();
                        break;
                    }
                    if let Err(error) = result {
                        let _ = events_tx.send(LogMessage::WriteFailed {
                            reason: error.to_string(),
                        });
                        if let Some(wake) = wake {
                            wake();
                        }
                        break 'outer;
                    }
                }
                last_typed = Some(Instant::now());
                refit_after_input = true;
            } else {
                // No lease: drop the keys rather than queue them forever.
                pending.clear();
            }
        }
        apply_writer_resize(
            &mut client,
            client_id,
            pane_id,
            window_id,
            session,
            &mut last_host_size,
            &mut refit_after_input,
            resize,
        );
        if controller
            && pending.is_empty()
            && last_typed.is_some_and(|at| at.elapsed() >= LEASE_IDLE)
            && send_release_lease(&mut client, client_id, pane_id, session)
        {
            controller = false;
            last_typed = None;
        }
    }
    if controller {
        let _ = send_release_lease(&mut client, client_id, pane_id, session);
    }
}

fn acquire_lease(client: &mut Client, client_id: u64, pane_id: u64) -> bool {
    client
        .request(|request_id| ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        })
        .is_ok()
}

#[cfg(test)]
static REQUEST_ERRS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Log line for a failed host attach control request (PT-288).
/// `session` is `-` when the writer has no session key (tests that
/// exercise the helper in isolation).
fn request_err_line(
    op: &str,
    pane_id: u64,
    session: Option<&str>,
    err: &dyn std::fmt::Display,
) -> String {
    format!(
        "prismattyc-host: {op} failed pane={pane_id} session={}: {err}",
        session.unwrap_or("-")
    )
}

/// Keep the `Ok` payload. Log `Err` and return `None` so the writer does
/// not treat a timed-out request as success (PT-288).
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

fn send_host_resize(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: &str,
    size: PendingResize,
) -> bool {
    keep_request_ok(
        "Resize",
        pane_id,
        Some(session),
        client.request(|request_id| ControlRequest::Resize {
            version: PROTOCOL_VERSION,
            request_id,
            window_id,
            cols: size.cols,
            rows: size.rows,
            cell_width_px: Some(size.cell_w),
            cell_height_px: Some(size.cell_h),
            client_id: Some(client_id),
            fit: false,
            host: true,
        }),
    )
    .is_some()
}

fn send_release_lease(client: &mut Client, client_id: u64, pane_id: u64, session: &str) -> bool {
    keep_request_ok(
        "ReleaseLease",
        pane_id,
        Some(session),
        client.request(|request_id| ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        }),
    )
    .is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterResizeAction {
    Skip,
    SkipClearPending,
    SkipClearRefit,
    SendNew(PendingResize),
    SendRefit(PendingResize),
}

fn writer_resize_action(
    last_host_size: Option<PendingResize>,
    wanted: Option<PendingResize>,
    refit_after_input: bool,
) -> WriterResizeAction {
    if let Some(size) = wanted {
        if last_host_size != Some(size) {
            WriterResizeAction::SendNew(size)
        } else {
            WriterResizeAction::SkipClearPending
        }
    } else if refit_after_input {
        match last_host_size {
            Some(size) => WriterResizeAction::SendRefit(size),
            None => WriterResizeAction::SkipClearRefit,
        }
    } else {
        WriterResizeAction::Skip
    }
}

fn writer_resize_commit(
    last_host_size: &mut Option<PendingResize>,
    refit_after_input: &mut bool,
    pending: &mut Option<PendingResize>,
    action: WriterResizeAction,
    ok: bool,
) {
    match action {
        WriterResizeAction::Skip => {}
        WriterResizeAction::SkipClearPending => *pending = None,
        WriterResizeAction::SkipClearRefit => *refit_after_input = false,
        WriterResizeAction::SendNew(size) => {
            if ok {
                *last_host_size = Some(size);
                *pending = None;
            }
        }
        WriterResizeAction::SendRefit(_) => {
            if ok {
                *refit_after_input = false;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_writer_resize(
    client: &mut Client,
    client_id: u64,
    pane_id: u64,
    window_id: u64,
    session: &str,
    last_host_size: &mut Option<PendingResize>,
    refit_after_input: &mut bool,
    resize: &Arc<Mutex<Option<PendingResize>>>,
) {
    let wanted = resize.lock().ok().and_then(|mut slot| slot.take());
    let action = writer_resize_action(*last_host_size, wanted, *refit_after_input);
    let ok = match action {
        WriterResizeAction::SendNew(size) | WriterResizeAction::SendRefit(size) => {
            send_host_resize(client, client_id, pane_id, window_id, session, size)
        }
        WriterResizeAction::Skip
        | WriterResizeAction::SkipClearPending
        | WriterResizeAction::SkipClearRefit => true,
    };
    let mut pending = wanted;
    writer_resize_commit(last_host_size, refit_after_input, &mut pending, action, ok);
    if !ok {
        if let Ok(mut slot) = resize.lock() {
            if slot.is_none() {
                *slot = pending;
            }
        }
    }
}

fn write_pane(client: &mut Client, client_id: u64, pane_id: u64, data: String) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    for _ in 0..64 {
        match client.request(|request_id| ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            data: data.clone(),
        }) {
            Ok(_) => return Ok(()),
            Err(error) if error_code(&error) == Some(ControlErrorCode::Backpressure) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    bail!("pane {pane_id} stayed under backpressure")
}

/// Longest valid UTF-8 prefix of `buf`, removed from it. `WritePane` takes a
/// `String`, so a chunk split inside a multi-byte character waits for the
/// rest.
fn take_utf8_prefix(buf: &mut Vec<u8>) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    let valid = match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(error) => error.valid_up_to(),
    };
    if valid == 0 {
        // Incomplete multi-byte sequence: wait for the rest. A lone
        // invalid byte (X10 mouse 0x80, stray continuation) is dropped
        // so `pending` cannot stall the controller lease.
        if utf8_incomplete_prefix_len(buf[0]).is_some_and(|need| buf.len() < need) {
            return None;
        }
        buf.remove(0);
        return take_utf8_prefix(buf);
    }
    let rest = buf.split_off(valid);
    let text = String::from_utf8_lossy(buf).into_owned();
    *buf = rest;
    Some(text)
}

fn utf8_incomplete_prefix_len(first: u8) -> Option<usize> {
    if (0xC2..0xE0).contains(&first) {
        Some(2)
    } else if (0xE0..0xF0).contains(&first) {
        Some(3)
    } else if (0xF0..0xF5).contains(&first) {
        Some(4)
    } else {
        None
    }
}

/// Repaint a ring-overrun snapshot into the replica emulator.
///
/// Cursor addressing only: no line ever scrolls off, so existing replica
/// scrollback survives the repaint.
pub(crate) fn snapshot_to_ansi(styled: &PaneStyled) -> Vec<u8> {
    use std::fmt::Write as _;
    let content = &styled.content;
    let mut out = String::new();
    // Match the snapshot's screen. The server reports the alternate screen
    // separately; entering it keeps primary scrollback intact.
    if content.alt_active {
        out.push_str("\x1b[?1049h");
    } else {
        out.push_str("\x1b[?1049l");
    }
    out.push_str("\x1b[0m\x1b[2J");
    for (index, line) in content.lines.iter().enumerate() {
        let row = index + 1;
        let _ = write!(out, "\x1b[{row};1H\x1b[0m");
        match styled.runs.get(index) {
            Some(runs) if !runs.is_empty() => {
                for run in runs {
                    push_sgr(&mut out, run);
                    out.push_str(&run.text);
                }
            }
            _ => out.push_str(line),
        }
        out.push_str("\x1b[0m");
    }
    let _ = write!(
        out,
        "\x1b[{};{}H",
        content.cursor_row.saturating_add(1),
        content.cursor_col.saturating_add(1)
    );
    out.push_str(if content.cursor_visible {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    });
    out.into_bytes()
}

fn push_sgr(out: &mut String, run: &prismattyc_mux::StyleRun) {
    use std::fmt::Write as _;
    out.push_str("\x1b[0m");
    if run.bold {
        out.push_str("\x1b[1m");
    }
    if run.italic {
        out.push_str("\x1b[3m");
    }
    if run.underline {
        out.push_str("\x1b[4m");
    }
    if run.inverse {
        out.push_str("\x1b[7m");
    }
    push_color(out, run.fg, true);
    push_color(out, run.bg, false);
    let _ = write!(out, "");
}

fn push_color(out: &mut String, color: prismattyc_mux::ColorWire, foreground: bool) {
    use std::fmt::Write as _;
    let base = if foreground { 30 } else { 40 };
    let extended = if foreground { 38 } else { 48 };
    match color {
        prismattyc_mux::ColorWire::Default => {}
        prismattyc_mux::ColorWire::Ansi { n } if n < 8 => {
            let _ = write!(out, "\x1b[{}m", base + u32::from(n));
        }
        prismattyc_mux::ColorWire::Ansi { n } => {
            let _ = write!(out, "\x1b[{extended};5;{n}m");
        }
        prismattyc_mux::ColorWire::Indexed { n } => {
            let _ = write!(out, "\x1b[{extended};5;{n}m");
        }
        prismattyc_mux::ColorWire::Rgb { r, g, b } => {
            let _ = write!(out, "\x1b[{extended};2;{r};{g};{b}m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_emulator::Emulator;
    use prismattyc_mux::PaneEvent;
    use std::sync::atomic::AtomicU64;

    #[test]
    #[cfg(target_os = "linux")]
    fn exact_pane_checks_session_pane_and_child_identity() {
        const TEST: &str = "attach_log::tests::exact_pane_checks_session_pane_and_child_identity";
        if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
            crate::render_window_tests::run_in_private_display(TEST);
            return;
        }
        // The display wrapper also isolates configuration, the socket, and view files.
        // Use a real daemon so this checks the request and lookup together.
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let binaries = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let socket = mux_socket().unwrap();
        let _daemon = Daemon(
            std::process::Command::new(binaries.join("pmuxd"))
                .arg("--socket")
                .arg(&socket)
                .args(["--", "/bin/sh"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        let initial = loop {
            if let Some(snapshot) = live_snapshot() {
                break snapshot;
            }
            assert!(started.elapsed() < prismattyc_core::test_time_budget(Duration::from_secs(3)));
            thread::sleep(Duration::from_millis(20));
        };
        let session = &initial.sessions[0];
        let mut client = Client::connect(&socket, REQUEST_TIMEOUT).unwrap();
        client
            .request(|request_id| ControlRequest::Split {
                version: PROTOCOL_VERSION,
                request_id,
                window_id: session.windows[0].id,
                target_pane_id: session.windows[0].panes[0].id,
                axis: prismattyc_mux::AxisWire::Horizontal,
                ratio: 0.5,
                spawn: prismattyc_mux::SpawnSpec {
                    program: "/bin/sh".into(),
                    argv: vec![],
                    cwd: None,
                    env: Default::default(),
                },
                client_id: None,
            })
            .unwrap();
        let created = std::process::Command::new(binaries.join("pmux"))
            .args(["new", "identity-peer", "--no-attach"])
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        let snapshot = live_snapshot().unwrap();
        let session = snapshot
            .sessions
            .iter()
            .find(|s| s.id == session.id)
            .unwrap();
        let window = &session.windows[0];
        assert_eq!(window.panes.len(), 2);
        let first = &window.panes[0];
        let target = &window.panes[1];
        let pid = target.child_pid.unwrap();
        let key = session.id.to_string();
        let connection = LogConnection::exact_pane(&key, target.id, pid).unwrap();
        assert_eq!(connection.session_key, key);
        assert_eq!(connection.pane_id, target.id);
        assert_eq!(connection.window_id, window.id);
        assert_eq!(connection.pane_title, target.title);
        assert_eq!(connection.space_id, session.space_id);
        assert_eq!(
            connection.initial_size,
            (target.geometry.cols as usize, target.geometry.rows as usize)
        );

        // Neither half of a stale pane/PID pair is enough to identify a child.
        assert!(LogConnection::exact_pane(&key, first.id, pid).is_err());
        assert!(LogConnection::exact_pane(&key, target.id, first.child_pid.unwrap()).is_err());
        assert!(LogConnection::exact_pane(&key, target.id, 0).is_err());
        assert!(LogConnection::exact_pane(&key, u64::MAX, pid).is_err());
        let other = snapshot
            .sessions
            .iter()
            .find(|s| s.id != session.id)
            .unwrap();
        assert!(LogConnection::exact_pane(&other.id.to_string(), target.id, pid).is_err());
        assert!(LogConnection::exact_pane(&u64::MAX.to_string(), target.id, pid).is_err());
        std::fs::write(
            std::env::var_os("PRISMATTYC_RENDER_TEST_RESULT").unwrap(),
            "complete",
        )
        .unwrap();
    }

    /// The same 12 KB PTY recording `replay_determinism` guards.
    const FIXTURE: &[u8] =
        include_bytes!("../../prismattyc-emulator/tests/fixtures/pt72-session.bin");
    const COLS: usize = 80;
    const ROWS: usize = 24;
    const SCROLLBACK: usize = 10_000;

    #[test]
    fn host_queue_stats_track_high_water_and_blocked_reader_time() {
        let budget = HostByteBudget::new(4);
        let held = budget.reserve(4).expect("initial reservation");
        let waiter_budget = budget.clone();
        let waiter = thread::spawn(move || waiter_budget.reserve(1).expect("waiter reservation"));
        thread::sleep(Duration::from_millis(20));
        drop(held);
        let waiter_reservation = waiter.join().expect("waiter thread");
        let stats = budget.stats();
        assert_eq!(stats.budget_bytes, 4);
        assert_eq!(stats.high_water_bytes, 4);
        assert!(stats.reader_blocked_ms > 0, "{stats:?}");
        drop(waiter_reservation);
        assert_eq!(budget.stats().queued_bytes, 0);
    }

    /// Build the log the server would write for `plan`, and the server
    /// emulator it wrote it from. `plan` is a list of (offset, resize?).
    fn record(chunks: &[usize], resizes: &[(usize, u16, u16)]) -> (Emulator, Vec<PaneLogFrame>) {
        let mut server = Emulator::new(COLS, ROWS, SCROLLBACK);
        let mut frames = Vec::new();
        let mut seq = 0u64;
        let mut offset = 0usize;
        let push = |frames: &mut Vec<PaneLogFrame>, seq: &mut u64, event: PaneEvent| {
            *seq += 1;
            frames.push(PaneLogFrame { seq: *seq, event });
        };
        for size in chunks {
            let end = (offset + size).min(FIXTURE.len());
            for (at, cols, rows) in resizes {
                if *at == offset {
                    server.resize(usize::from(*cols), usize::from(*rows));
                    push(
                        &mut frames,
                        &mut seq,
                        PaneEvent::Resize {
                            cols: *cols,
                            rows: *rows,
                            cell_px: (10, 20),
                            size_owner: None,
                            reflow: true,
                        },
                    );
                }
            }
            if offset < end {
                let bytes = &FIXTURE[offset..end];
                // live.rs logs the chunk, then feeds it.
                push(
                    &mut frames,
                    &mut seq,
                    PaneEvent::Output {
                        bytes: bytes.to_vec(),
                    },
                );
                let _ = server.feed(bytes);
                // The PTY owner answers DSR/CPR. A replica must not.
                let _ = server.take_pending_replies();
            }
            offset = end;
            if offset >= FIXTURE.len() {
                break;
            }
        }
        (server, frames)
    }

    /// The replica: what `PaneRuntime::drain_log` does to the emulator.
    fn replay(frames: &[PaneLogFrame]) -> Emulator {
        let mut replica = Emulator::new(COLS, ROWS, SCROLLBACK);
        for frame in frames {
            match &frame.event {
                PaneEvent::Output { bytes } => {
                    let _ = replica.feed(bytes);
                    // Dropped, never sent: DSR/CPR answers belong to the
                    // PTY owner (PT-72 §2, rule 1).
                    let _ = replica.take_pending_replies();
                }
                PaneEvent::Resize { cols, rows, .. } => {
                    replica.resize(usize::from(*cols), usize::from(*rows));
                }
                _ => {}
            }
        }
        replica
    }

    fn chunks(size: usize) -> Vec<usize> {
        vec![size; FIXTURE.len().div_ceil(size) + 4]
    }

    #[test]
    fn replica_screen_equals_server_screen_on_the_fixture() {
        let (server, frames) = record(&chunks(997), &[]);
        let replica = replay(&frames);
        assert_eq!(replica.screen(), server.screen());
    }

    #[test]
    fn replica_screen_equals_server_screen_across_resizes() {
        // Resizes land at chunk boundaries, i.e. at their byte position.
        let plan = chunks(1_000);
        let resizes = [
            (1_000usize, 100u16, 30u16),
            (5_000, 60, 20),
            (9_000, 90, 26),
        ];
        let (server, frames) = record(&plan, &resizes);
        assert!(
            frames
                .iter()
                .filter(|frame| matches!(frame.event, PaneEvent::Resize { .. }))
                .count()
                == 3
        );
        let replica = replay(&frames);
        assert_eq!(replica.screen(), server.screen());
        assert_eq!(replica.screen().columns(), 90);
    }

    /// AC3: the replica keeps scrollback, so the host scrollbar and find
    /// have history to work over. The nested `pmux attach` child painted
    /// only the visible grid, leaving `max_view_scroll` at 0.
    #[test]
    fn replica_keeps_scrollback_for_the_host_scrollbar() {
        let (_, frames) = record(&chunks(4_096), &[]);
        let replica = replay(&frames);
        assert!(
            replica.screen().max_view_scroll() > 0,
            "attached pane must expose scrollback"
        );
    }

    #[test]
    fn attach_target_matches_only_pmux_attach_session_id() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            attach_target("/usr/bin/pmux", &args(&["attach", "--session-id", "7"])),
            Some("7".into())
        );
        assert_eq!(
            attach_target("pmux", &args(&["attach", "--session-id", "7"])),
            Some("7".into())
        );
        assert_eq!(
            attach_target("pmux", &args(&["attach", "--session", "astra-pc"])),
            Some("astra-pc".into())
        );
        assert_eq!(
            attach_target("pmux", &args(&["attach", "astra-pc"])),
            Some("astra-pc".into())
        );
        assert_eq!(
            attach_target(
                "/usr/bin/pmux-attach",
                &args(&["--socket", "/tmp/pmux.sock", "--session", "astra-pc"])
            ),
            Some("astra-pc".into())
        );
        assert_eq!(
            attach_target("pmux-attach", &args(&["--session-id", "7"])),
            Some("7".into())
        );
        assert_eq!(attach_target("pmux", &args(&["attach", "--all"])), None);
        assert_eq!(attach_target("pmux", &args(&["ls"])), None);
        assert_eq!(
            attach_target("pmux-attach", &args(&["--json", "--session", "astra-pc"])),
            None
        );
        assert_eq!(
            attach_target("/bin/sh", &args(&["attach", "--session-id", "7"])),
            None
        );
        assert_eq!(
            attach_target("pmux", &args(&["attach", "--session-id", ""])),
            None
        );
    }

    fn attach_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn attach_session_key_prefers_id_then_name_then_positional() {
        let check = |argv: &[&str], expected: Option<&str>| {
            assert_eq!(
                attach_session_key(&attach_args(argv)),
                expected.map(str::to_string),
                "{argv:?}"
            );
        };
        check(&["--session-id", "7", "--session", "astra-pc"], Some("7"));
        check(&["--session", "astra-pc", "ignored"], Some("astra-pc"));
        check(&["astra-pc", "second"], Some("astra-pc"));
        check(
            &["--session-id", "", "--session", "astra-pc"],
            Some("astra-pc"),
        );
        check(&["--session", ""], None);
        check(&["--session-id", ""], None);
        check(&["--socket", "/tmp/pmux.sock"], None);
        check(&["--space", "dev"], None);
        check(&["--create-session", "new"], None);
        check(
            &["--socket", "/tmp/pmux.sock", "--session", "astra-pc"],
            Some("astra-pc"),
        );
        check(&["--foo"], None);
        check(&["--all"], None);
        check(&["--json", "--session", "astra-pc"], None);
        check(&["--session-id", "7"], Some("7"));
        check(&["--session", "astra-pc"], Some("astra-pc"));
        check(&["astra-pc"], Some("astra-pc"));
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

    fn test_snapshot(
        session_id: u64,
        name: &str,
        window_id: u64,
        pane_id: u64,
        title: &str,
        title_pinned: bool,
    ) -> Snapshot {
        use prismattyc_mux::{
            LayoutSnapshot, PaneGeometry, PaneSnapshot, SessionSnapshot, WindowBounds,
            WindowSnapshot,
        };
        Snapshot {
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
                        title: title.into(),
                        title_pinned,
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

    #[test]
    fn session_pane_returns_every_field_for_id_or_name() {
        let snapshot = test_snapshot(7, "astra-pc", 99, 42, "seat-title", false);
        let expected = SessionPaneRef {
            size: (80, 24),
            session_id: "7".into(),
            space_id: None,
            pane_id: 42,
            window_id: 99,
            title: "seat-title".into(),
            title_pinned: false,
        };
        assert_eq!(session_pane(&snapshot, "7").unwrap(), expected);
        assert_eq!(session_pane(&snapshot, "astra-pc").unwrap(), expected);
        assert!(session_pane(&snapshot, "missing").is_err());
        let pinned = test_snapshot(7, "astra-pc", 99, 42, "seat-title", true);
        assert_eq!(
            session_pane(&pinned, "7").unwrap(),
            SessionPaneRef {
                title_pinned: true,
                ..expected.clone()
            }
        );
        assert_eq!(
            session_title_pin(&snapshot, "7"),
            Some(("seat-title".into(), false))
        );
        assert_eq!(
            session_title_pin(&pinned, "astra-pc"),
            Some(("seat-title".into(), true))
        );
        assert_eq!(session_title_pin(&snapshot, "missing"), None);
    }

    #[test]
    fn session_pane_rejects_empty_window_or_pane() {
        use prismattyc_mux::{LayoutSnapshot, SessionSnapshot, WindowBounds, WindowSnapshot};
        let no_window = Snapshot {
            sequence: 1,
            sessions: vec![SessionSnapshot {
                space_id: None,
                id: 7,
                name: "astra-pc".into(),
                agent_id: None,
                windows: vec![],
            }],
        };
        assert!(session_pane(&no_window, "7").is_err());
        let no_pane = Snapshot {
            sequence: 1,
            sessions: vec![SessionSnapshot {
                space_id: None,
                id: 7,
                name: "astra-pc".into(),
                agent_id: None,
                windows: vec![WindowSnapshot {
                    id: 99,
                    title: "main".into(),
                    bounds: WindowBounds {
                        window_id: 99,
                        cols: 80,
                        rows: 24,
                    },
                    layout: LayoutSnapshot::Leaf { pane_id: 42 },
                    panes: vec![],
                    sync_input: false,
                }],
            }],
        };
        assert!(session_pane(&no_pane, "7").is_err());
        assert_eq!(session_title_pin(&no_window, "7"), None);
        assert_eq!(session_title_pin(&no_pane, "astra-pc"), None);
    }

    #[test]
    fn utf8_prefix_holds_back_a_split_character() {
        let mut buf = "héllo".as_bytes().to_vec();
        let tail = buf.split_off(2);
        assert_eq!(take_utf8_prefix(&mut buf).as_deref(), Some("h"));
        assert!(take_utf8_prefix(&mut buf).is_none());
        buf.extend_from_slice(&tail);
        assert_eq!(take_utf8_prefix(&mut buf).as_deref(), Some("éllo"));
    }

    #[test]
    fn utf8_prefix_drops_a_lone_invalid_byte() {
        let mut buf = vec![0x80];
        assert!(take_utf8_prefix(&mut buf).is_none());
        assert!(buf.is_empty());
        let mut buf = vec![0x80, b'A'];
        assert_eq!(take_utf8_prefix(&mut buf).as_deref(), Some("A"));
        assert!(buf.is_empty());
    }

    #[test]
    fn snapshot_repaint_grows_rows_when_cols_match() {
        let mut source = Emulator::new(20, 6, SCROLLBACK);
        let _ = source.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        let styled = styled_from(&source, 20, 6);
        let mut replica = Emulator::new(20, 3, SCROLLBACK);
        let cols = 20usize;
        let rows = 6usize;
        if replica.screen().columns() != cols || replica.screen().rows() != rows {
            replica.resize(cols, rows);
        }
        let _ = replica.feed(&snapshot_to_ansi(&styled));
        assert_eq!(replica.screen().rows(), 6);
        assert_eq!(visible_row(replica.screen(), 5).trim_end(), "six");
    }

    #[test]
    fn styled_snapshot_repaint_preserves_colors_attributes_and_alternate_screen() {
        let mut source = Emulator::new(40, 3, SCROLLBACK);
        let _ = source.feed(b"\x1b[?1049h\x1b[1;3;4;7;31;44mA\x1b[0;91;102mB\x1b[0;38;5;123;48;5;45mC\x1b[0;38;2;11;22;33;48;2;44;55;66mD\x1b[0mE\x1b[?25l");
        let mut styled = styled_from(&source, 40, 3);
        styled.content.alt_active = true;
        styled.content.cursor_visible = false;
        styled.runs = (0..3)
            .map(|row| {
                prismattyc_mux::rle_style_runs(
                    (0..40).map(|col| source.screen().view_cell(0, row, col)),
                )
            })
            .collect();
        let mut replica = Emulator::new(40, 3, SCROLLBACK);
        let _ = replica.feed(&snapshot_to_ansi(&styled));
        assert!(replica.screen().alt_active());
        for row in 0..3 {
            for col in 0..40 {
                let actual = replica.screen().view_cell(0, row, col);
                let original = source.screen().view_cell(0, row, col);
                assert_eq!(actual.character, original.character);
                let mut expected = original.style;
                // Bright ANSI colors use the equivalent indexed SGR wire form.
                let canonical = |color| match color {
                    prismattyc_core::Color::Ansi(n) if n >= 8 => prismattyc_core::Color::Indexed(n),
                    color => color,
                };
                expected.foreground = canonical(expected.foreground);
                expected.background = canonical(expected.background);
                assert_eq!(actual.style, expected, "cell {row},{col}");
            }
        }
    }

    #[test]
    fn snapshot_repaint_reproduces_the_visible_grid() {
        let mut source = Emulator::new(20, 3, SCROLLBACK);
        let _ = source.feed(b"\x1b[1;31mred\x1b[0m plain\r\n\x1b[48;5;33msecond\x1b[0m");
        let styled = styled_from(&source, 20, 3);
        let mut replica = Emulator::new(20, 3, SCROLLBACK);
        let _ = replica.feed(&snapshot_to_ansi(&styled));
        let rendered: Vec<String> = (0..3)
            .map(|row| visible_row(replica.screen(), row))
            .collect();
        assert_eq!(rendered[0].trim_end(), "red plain");
        assert_eq!(rendered[1].trim_end(), "second");
    }

    fn visible_row(screen: &prismattyc_core::Screen, row: usize) -> String {
        (0..screen.columns())
            .map(|col| {
                screen
                    .row(row)
                    .and_then(|cells| cells.get(col))
                    .map_or(' ', |cell| cell.character)
            })
            .collect()
    }

    #[test]
    fn reader_keeps_replay_boundary_across_batches_and_resets() {
        for (name, boundary, reset) in [
            ("history", 600, false),
            ("empty", 0, false),
            ("reset", 600, true),
        ] {
            let socket = test_socket(name);
            let _guard = UnlinkOnDrop(socket.clone());
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                let (mut writer, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(writer.try_clone().unwrap());
                let mut read_request = || {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    serde_json::from_str::<ControlRequest>(&line).unwrap()
                };
                let ControlRequest::RegisterClient { request_id, .. } = read_request() else {
                    panic!("expected registration");
                };
                write_ok(
                    &mut writer,
                    request_id,
                    ControlResponseData::ClientRegistered { client_id: 1 },
                );
                let ControlRequest::SubscribePane {
                    request_id,
                    from_seq,
                    timeout_ms,
                    ..
                } = read_request()
                else {
                    panic!("expected boundary probe");
                };
                assert_eq!((from_seq, timeout_ms), (u64::MAX, 0));
                let send_boundary =
                    |writer: &mut std::os::unix::net::UnixStream, request_id, current| {
                        let response = ControlResponse {
                            version: PROTOCOL_VERSION,
                            request_id,
                            body: ControlResponseBody::Error {
                                error: ControlError {
                                    code: ControlErrorCode::StaleSequence,
                                    message: "ahead".into(),
                                    resnapshot_required: true,
                                    oldest_available_sequence: Some(1),
                                    current_sequence: Some(current),
                                    holder: None,
                                },
                            },
                        };
                        serde_json::to_writer(&mut *writer, &response).unwrap();
                        writer.write_all(b"\n").unwrap();
                    };
                send_boundary(&mut writer, request_id, boundary);
                let ControlRequest::SubscribePane {
                    mut request_id,
                    from_seq,
                    ..
                } = read_request()
                else {
                    panic!("expected replay");
                };
                assert_eq!(from_seq, 0);
                let effective_boundary = if reset {
                    send_boundary(&mut writer, request_id, 3);
                    let ControlRequest::SubscribePane {
                        request_id: next,
                        from_seq,
                        ..
                    } = read_request()
                    else {
                        panic!("expected reset replay");
                    };
                    assert_eq!(from_seq, 0);
                    request_id = next;
                    3
                } else {
                    boundary
                };
                for sequence in [1, effective_boundary.max(1), effective_boundary + 1] {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::PaneSubscribe {
                            pane_id: 1,
                            gap: false,
                            snapshot: None,
                            events: vec![PaneLogFrame {
                                seq: sequence,
                                event: PaneEvent::Output {
                                    bytes: b"\x07".to_vec(),
                                },
                            }],
                            through_seq: sequence,
                            done: false,
                        },
                    );
                }
                // Close the fixture connection after delivering all frames.
            });
            let (tx, rx) = mpsc::channel();
            let result = reader_loop(
                &socket,
                1,
                &tx,
                None,
                &AtomicBool::new(false),
                &HostByteBudget::new(HOST_EVENT_BUDGET_BYTES),
            );
            assert!(result.is_some(), "fixture closes its stream");
            drop(tx);
            let messages: Vec<_> = rx.into_iter().collect();
            let effective_boundary = if reset { 3 } else { boundary };
            assert_eq!(messages.len(), 3 + usize::from(reset));
            if reset {
                assert!(matches!(messages[0], LogMessage::Reset));
            }
            for message in &messages[usize::from(reset)..] {
                let LogMessage::Batch {
                    replay_through,
                    events,
                    ..
                } = message
                else {
                    panic!("expected output batch");
                };
                assert_eq!(*replay_through, effective_boundary);
                assert_eq!(events.len(), 1);
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn dropped_log_pane_stops_reader() {
        let socket = test_socket("reader-stop");
        let _guard = UnlinkOnDrop(socket.clone());
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || serve_subscribe_done(listener));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let reader_budget = HostByteBudget::new(HOST_EVENT_BUDGET_BYTES);
        let pane_budget = reader_budget.clone();
        let reader_stop = Arc::clone(&stop);
        let reader_socket = socket.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let reason = reader_loop(&reader_socket, 1, &tx, None, &reader_stop, &reader_budget);
            let _ = done_tx.send(reason);
        });
        let pane = LogPane {
            pane_id: 1,
            events_rx: rx,
            event_budget: pane_budget,
            resize: Arc::new(Mutex::new(None)),
            stop,
            session_key: "t".into(),
        };
        drop(pane);
        let reason = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("reader should stop after LogPane drop");
        assert!(reason.is_none(), "reader must exit on stop, got {reason:?}");
        let _ = reader.join();
        let _ = server.join();
    }

    #[test]
    fn writer_error_still_releases_lease() {
        let socket = test_socket("writer-lease");
        let _guard = UnlinkOnDrop(socket.clone());
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (released_tx, released_rx) = mpsc::channel();
        let server = thread::spawn(move || serve_write_then_fail(listener, released_tx));
        let (to_tx, to_rx) = mpsc::sync_channel(4);
        let (ev_tx, ev_rx) = mpsc::channel();
        let writer_socket = socket.clone();
        let writer = thread::spawn(move || {
            writer_loop(
                &writer_socket,
                1,
                1,
                "t",
                None,
                &to_rx,
                &Arc::new(Mutex::new(None)),
                &ev_tx,
                None,
            );
        });
        to_tx
            .send(ChildWrite::bytes(b"x".to_vec()))
            .expect("send key");
        released_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("ReleaseLease after WritePane error");
        match ev_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(LogMessage::WriteFailed { reason }) => {
                assert!(!reason.is_empty(), "write-fail reason");
            }
            other => panic!("expected WriteFailed, got {other:?}"),
        }
        drop(to_tx);
        writer.join().expect("writer thread");
        let _ = server.join();
    }

    #[test]
    fn keep_request_ok_per_verb_ok_timeout_transport() {
        let verbs = ["ReleaseLease", "Resize", "SwitchSession"];
        for op in verbs {
            assert_eq!(
                request_err_line(op, 7, Some("nexus"), &"timed out"),
                format!("prismattyc-host: {op} failed pane=7 session=nexus: timed out")
            );
            assert_eq!(
                request_err_line(op, 7, Some("nexus"), &"Broken pipe"),
                format!("prismattyc-host: {op} failed pane=7 session=nexus: Broken pipe")
            );
            assert_eq!(
                request_err_line(op, 7, None, &"timed out"),
                format!("prismattyc-host: {op} failed pane=7 session=-: timed out")
            );
            assert_eq!(
                keep_request_ok(op, 7, Some("nexus"), Result::<u8>::Ok(1)),
                Some(1)
            );
            REQUEST_ERRS.lock().unwrap().clear();
            assert_eq!(
                keep_request_ok::<u8>(op, 7, Some("nexus"), Err(anyhow::anyhow!("timed out"))),
                None
            );
            assert_eq!(
                keep_request_ok::<u8>(op, 7, Some("nexus"), Err(anyhow::anyhow!("Broken pipe"))),
                None
            );
            let lines = REQUEST_ERRS.lock().unwrap().clone();
            assert_eq!(
                lines,
                [
                    format!("prismattyc-host: {op} failed pane=7 session=nexus: timed out"),
                    format!("prismattyc-host: {op} failed pane=7 session=nexus: Broken pipe"),
                ]
            );
        }
    }

    fn pending(cols: u32, rows: u32) -> PendingResize {
        PendingResize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 16,
        }
    }

    #[test]
    fn writer_resize_action_table() {
        let a = pending(80, 24);
        let b = pending(100, 30);
        let cases = [
            (None, None, false, WriterResizeAction::Skip),
            (None, None, true, WriterResizeAction::SkipClearRefit),
            (Some(a), None, true, WriterResizeAction::SendRefit(a)),
            (None, Some(a), false, WriterResizeAction::SendNew(a)),
            (
                Some(a),
                Some(a),
                false,
                WriterResizeAction::SkipClearPending,
            ),
            (Some(a), Some(b), false, WriterResizeAction::SendNew(b)),
        ];
        for (last, wanted, refit, want) in cases {
            assert_eq!(
                writer_resize_action(last, wanted, refit),
                want,
                "{last:?} {wanted:?} {refit}"
            );
        }
    }

    #[test]
    fn writer_resize_commit_keeps_local_state_on_err() {
        let a = pending(80, 24);
        let b = pending(100, 30);
        let mut last = Some(a);
        let mut refit = false;
        let mut slot = Some(b);
        writer_resize_commit(
            &mut last,
            &mut refit,
            &mut slot,
            WriterResizeAction::SendNew(b),
            false,
        );
        assert_eq!(
            last,
            Some(a),
            "failed Resize must not record last_host_size"
        );
        assert_eq!(slot, Some(b), "failed Resize must keep the pending size");

        writer_resize_commit(
            &mut last,
            &mut refit,
            &mut slot,
            WriterResizeAction::SendNew(b),
            true,
        );
        assert_eq!(last, Some(b));
        assert!(slot.is_none());

        refit = true;
        writer_resize_commit(
            &mut last,
            &mut refit,
            &mut slot,
            WriterResizeAction::SendRefit(b),
            false,
        );
        assert!(refit, "failed refit Resize must retry");
        writer_resize_commit(
            &mut last,
            &mut refit,
            &mut slot,
            WriterResizeAction::SendRefit(b),
            true,
        );
        assert!(!refit);
    }

    #[test]
    fn writer_resize_after_transport_err_logs_and_keeps_last_size() {
        REQUEST_ERRS.lock().unwrap().clear();
        let socket = test_socket("writer-resize-err");
        let _guard = UnlinkOnDrop(socket.clone());
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        let server = thread::spawn(move || serve_resize_ok_then_err(listener, seen_tx));
        let first = pending(80, 24);
        let second = pending(100, 30);
        let resize = Arc::new(Mutex::new(Some(first)));
        let (to_tx, to_rx) = mpsc::sync_channel(4);
        let (ev_tx, _ev_rx) = mpsc::channel();
        let writer_socket = socket.clone();
        let writer_resize = Arc::clone(&resize);
        let writer = thread::spawn(move || {
            writer_loop(
                &writer_socket,
                7,
                1,
                "nexus",
                None,
                &to_rx,
                &writer_resize,
                &ev_tx,
                None,
            );
        });

        let first_seen = seen_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("first Resize");
        assert_eq!(first_seen, (80, 24));

        {
            let mut slot = resize.lock().unwrap();
            *slot = Some(second);
        }
        let failed = seen_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("failed Resize");
        assert_eq!(failed, (100, 30));

        let start = Instant::now();
        loop {
            let lines = REQUEST_ERRS.lock().unwrap().clone();
            if lines.iter().any(|line| {
                line == "prismattyc-host: Resize failed pane=7 session=nexus: StaleId: Broken pipe"
            }) {
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("missing host attach_log line, got {lines:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }

        {
            let mut slot = resize.lock().unwrap();
            *slot = Some(first);
        }
        thread::sleep(WRITER_TICK * 3);
        assert!(
            seen_rx.try_recv().is_err(),
            "a later 80x24 Resize means last_host_size lied and became 100x30"
        );

        drop(to_tx);
        writer.join().expect("writer thread");
        let _ = server.join();
    }

    #[test]
    fn writer_survives_repeated_input_dirty() {
        let socket = test_socket("writer-dirty");
        let _guard = UnlinkOnDrop(socket.clone());
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (writes_tx, writes_rx) = mpsc::channel();
        let server = thread::spawn(move || serve_repeated_input_dirty(listener, writes_tx));
        let (to_tx, to_rx) = mpsc::sync_channel(4);
        let (ev_tx, ev_rx) = mpsc::channel();
        let writer_socket = socket.clone();
        let writer = thread::spawn(move || {
            writer_loop(
                &writer_socket,
                1,
                1,
                "t",
                None,
                &to_rx,
                &Arc::new(Mutex::new(None)),
                &ev_tx,
                None,
            );
        });
        to_tx
            .send(ChildWrite::bytes(b"x".to_vec()))
            .expect("send key");
        writes_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("first InputDirty");
        writes_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("second InputDirty after re-acquire");
        match ev_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            other => panic!("writer must stay up on repeated InputDirty, got {other:?}"),
        }
        drop(to_tx);
        writer.join().expect("writer thread");
        let _ = server.join();
    }

    struct UnlinkOnDrop(PathBuf);
    impl Drop for UnlinkOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn test_socket(name: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pt111-{name}-{}-{n}.sock", std::process::id()))
    }

    fn write_frame(
        writer: &mut std::os::unix::net::UnixStream,
        request_id: u64,
        body: ControlResponseBody,
    ) {
        let frame = ControlResponse {
            version: PROTOCOL_VERSION,
            request_id,
            body,
        };
        serde_json::to_writer(&mut *writer, &frame).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    }

    fn write_ok(
        writer: &mut std::os::unix::net::UnixStream,
        request_id: u64,
        response: ControlResponseData,
    ) {
        write_frame(writer, request_id, ControlResponseBody::Ok { response });
    }

    #[test]
    fn request_drains_two_stale_ids_and_the_next_request_succeeds() {
        let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let registered = ControlResponseData::ClientRegistered { client_id: 9 };
        write_ok(&mut peer, 1, registered.clone());
        write_ok(&mut peer, 2, registered.clone());
        let mut client = Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            next_request_id: 2,
            client_id: 1,
        };
        let first = client
            .request(|request_id| ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id,
            })
            .expect("drain stale id 1 and hit 2");
        assert_eq!(first, registered);
        write_ok(&mut peer, 3, registered.clone());
        let second = client
            .request(|request_id| ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id,
            })
            .expect("next request after drain");
        assert_eq!(second, registered);
    }

    fn serve_subscribe_done(listener: std::os::unix::net::UnixListener) {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&line) else {
                break;
            };
            match request {
                ControlRequest::RegisterClient { request_id, .. } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::ClientRegistered { client_id: 1 },
                    );
                }
                ControlRequest::SubscribePane {
                    request_id,
                    pane_id,
                    ..
                } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::PaneSubscribe {
                            pane_id,
                            gap: false,
                            snapshot: None,
                            events: Vec::new(),
                            through_seq: 0,
                            done: true,
                        },
                    );
                }
                _ => break,
            }
        }
    }

    fn serve_write_then_fail(
        listener: std::os::unix::net::UnixListener,
        released: mpsc::Sender<()>,
    ) {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&line) else {
                break;
            };
            match request {
                ControlRequest::RegisterClient { request_id, .. } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::ClientRegistered { client_id: 1 },
                    );
                }
                ControlRequest::AcquireLease {
                    request_id,
                    pane_id,
                    ..
                } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::Lease {
                            pane_id,
                            controller_id: Some(1),
                            previous_controller_id: None,
                        },
                    );
                }
                ControlRequest::WritePane { request_id, .. } => {
                    write_frame(
                        &mut writer,
                        request_id,
                        ControlResponseBody::Error {
                            error: ControlError {
                                code: ControlErrorCode::StaleId,
                                message: "pane gone".into(),
                                resnapshot_required: false,
                                oldest_available_sequence: None,
                                current_sequence: None,
                                holder: None,
                            },
                        },
                    );
                }
                ControlRequest::ReleaseLease {
                    request_id,
                    pane_id,
                    ..
                } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::Lease {
                            pane_id,
                            controller_id: None,
                            previous_controller_id: Some(1),
                        },
                    );
                    let _ = released.send(());
                    break;
                }
                _ => break,
            }
        }
    }

    fn serve_resize_ok_then_err(
        listener: std::os::unix::net::UnixListener,
        seen: mpsc::Sender<(u32, u32)>,
    ) {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut resizes = 0u8;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&line) else {
                break;
            };
            match request {
                ControlRequest::RegisterClient { request_id, .. } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::ClientRegistered { client_id: 1 },
                    );
                }
                ControlRequest::Resize {
                    request_id,
                    cols,
                    rows,
                    ..
                } => {
                    let _ = seen.send((cols, rows));
                    resizes = resizes.saturating_add(1);
                    if resizes == 1 {
                        write_ok(&mut writer, request_id, ControlResponseData::Pong);
                    } else {
                        write_frame(
                            &mut writer,
                            request_id,
                            ControlResponseBody::Error {
                                error: ControlError {
                                    code: ControlErrorCode::StaleId,
                                    message: "Broken pipe".into(),
                                    resnapshot_required: false,
                                    oldest_available_sequence: None,
                                    current_sequence: None,
                                    holder: None,
                                },
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn serve_repeated_input_dirty(
        listener: std::os::unix::net::UnixListener,
        writes: mpsc::Sender<()>,
    ) {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(request) = serde_json::from_str::<ControlRequest>(&line) else {
                break;
            };
            match request {
                ControlRequest::RegisterClient { request_id, .. } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::ClientRegistered { client_id: 1 },
                    );
                }
                ControlRequest::AcquireLease {
                    request_id,
                    pane_id,
                    ..
                } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::Lease {
                            pane_id,
                            controller_id: Some(1),
                            previous_controller_id: None,
                        },
                    );
                }
                ControlRequest::WritePane { request_id, .. } => {
                    let _ = writes.send(());
                    write_frame(
                        &mut writer,
                        request_id,
                        ControlResponseBody::Error {
                            error: ControlError {
                                code: ControlErrorCode::InputDirty,
                                message: "ledger dirty".into(),
                                resnapshot_required: false,
                                oldest_available_sequence: None,
                                current_sequence: None,
                                holder: None,
                            },
                        },
                    );
                }
                ControlRequest::ReleaseLease {
                    request_id,
                    pane_id,
                    ..
                } => {
                    write_ok(
                        &mut writer,
                        request_id,
                        ControlResponseData::Lease {
                            pane_id,
                            controller_id: None,
                            previous_controller_id: Some(1),
                        },
                    );
                }
                _ => break,
            }
        }
    }

    /// Minimal stand-in for the server's `PaneStyled` builder.
    fn styled_from(emulator: &Emulator, cols: u32, rows: u32) -> PaneStyled {
        let screen = emulator.screen();
        let lines: Vec<String> = (0..rows as usize)
            .map(|row| visible_row(screen, row))
            .collect();
        PaneStyled {
            content: prismattyc_mux::PaneContent {
                pane_id: 1,
                revision: 1,
                cols,
                rows,
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                alt_active: false,
                child_alive: true,
                child_pid: None,
                lines,
                cursor_shape: None,
            },
            runs: Vec::new(),
            workspace: Vec::new(),
            workspace_styles: Vec::new(),
            workspace_inverse: Vec::new(),
            view_offset: None,
            max_view_scroll: None,
            child_mouse_tracking: None,
            child_mouse_sgr: None,
            overlays: Vec::new(),
            experimental_rich: false,
            rich_focus_id: None,
            structured_focus: false,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
        }
    }

    #[test]
    fn transient_read_errors_are_timeouts_and_interrupts_only() {
        let io = |kind| anyhow::Error::new(std::io::Error::from(kind));
        assert!(transient_read_error(&io(std::io::ErrorKind::WouldBlock)));
        assert!(transient_read_error(&io(std::io::ErrorKind::TimedOut)));
        assert!(transient_read_error(&io(std::io::ErrorKind::Interrupted)));
        assert!(!transient_read_error(&io(
            std::io::ErrorKind::UnexpectedEof
        )));
        assert!(!transient_read_error(&io(std::io::ErrorKind::BrokenPipe)));
        assert!(!transient_read_error(&anyhow::anyhow!(
            "server closed the pane connection"
        )));
        // Wrapped with context, the io kind is still visible.
        let wrapped = io(std::io::ErrorKind::WouldBlock).context("read frame");
        assert!(
            transient_read_error(&wrapped) || wrapped.downcast_ref::<std::io::Error>().is_none()
        );
    }
}

#[cfg(test)]
mod pt268_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn host_budget_admits_before_allocation_and_releases() {
        let budget = HostByteBudget::new(4);
        let allocated = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&allocated);
        assert!(budget
            .admit(5, || {
                flag.store(true, Ordering::SeqCst);
                vec![0u8; 5]
            })
            .is_none());
        assert!(!allocated.load(Ordering::SeqCst));

        let (payload, reservation) = budget.admit(4, || vec![1u8; 4]).unwrap();
        assert_eq!(payload.len(), 4);
        assert_eq!(budget.reserved(), 4);
        drop(payload);
        assert_eq!(budget.reserved(), 4);
        drop(reservation);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn host_budget_blocks_before_the_next_decode_allocation() {
        let budget = HostByteBudget::new(4);
        let held = budget.admit(4, || vec![0u8; 4]).unwrap();
        let allocated = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&allocated);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiting_budget = budget.clone();
        let waiter = std::thread::spawn(move || {
            let admitted = waiting_budget.admit(1, || {
                flag.store(true, Ordering::SeqCst);
                vec![1u8]
            });
            done_tx.send(admitted.is_some()).unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(!allocated.load(Ordering::SeqCst));
        drop(held);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiter.join().unwrap();
        assert!(allocated.load(Ordering::SeqCst));
        assert_eq!(budget.reserved(), 0);
    }
    #[test]
    fn coalesce_events_keeps_policy_boundaries_and_counts_superseded_frames() {
        let (events, counters) = coalesce_events(vec![
            PaneLogFrame {
                seq: 1,
                event: PaneEvent::Title { text: "old".into() },
            },
            PaneLogFrame {
                seq: 2,
                event: PaneEvent::Title { text: "new".into() },
            },
            PaneLogFrame {
                seq: 3,
                event: PaneEvent::Resize {
                    cols: 80,
                    rows: 24,
                    cell_px: (8, 16),
                    size_owner: None,
                    reflow: true,
                },
            },
            PaneLogFrame {
                seq: 4,
                event: PaneEvent::Resize {
                    cols: 100,
                    rows: 30,
                    cell_px: (8, 16),
                    size_owner: None,
                    reflow: true,
                },
            },
            PaneLogFrame {
                seq: 5,
                event: PaneEvent::Output {
                    bytes: b"out".to_vec(),
                },
            },
            PaneLogFrame {
                seq: 6,
                event: PaneEvent::Status {
                    text: Some("old".into()),
                },
            },
            PaneLogFrame {
                seq: 7,
                event: PaneEvent::Status {
                    text: Some("new".into()),
                },
            },
            PaneLogFrame {
                seq: 8,
                event: PaneEvent::Title {
                    text: "after".into(),
                },
            },
        ]);

        assert_eq!(counters.superseded_frames, 3);
        assert_eq!(events.len(), 5);
        assert!(matches!(
            &events[0].event,
            PaneEvent::Title { text } if text == "new"
        ));
        assert!(matches!(
            &events[1].event,
            PaneEvent::Resize {
                cols: 100,
                rows: 30,
                ..
            }
        ));
        assert!(matches!(&events[2].event, PaneEvent::Output { bytes } if bytes == b"out"));
        assert!(matches!(
            &events[3].event,
            PaneEvent::Status { text } if text.as_deref() == Some("new")
        ));
        assert!(matches!(
            &events[4].event,
            PaneEvent::Title { text } if text == "after"
        ));
        assert!(counters.superseded_bytes > 0);
    }
}
