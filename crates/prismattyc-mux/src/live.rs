//! Server-owned PTY/emulator runtimes for Phase 2B.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
#[cfg(all(test, windows))]
use std::{sync::atomic::AtomicU64, sync::Arc};

use anyhow::{Context, Result};
use prismattyc_emulator::{
    pty_size_with_cell_pixels, Emulator, EmulatorStateV1, PtySession, StateError,
    NOMINAL_CELL_H_PX, NOMINAL_CELL_W_PX,
};

use crate::control::{
    rle_style_runs, CursorShapeWire, PaneContent, PaneGeometry, PaneStyled, SpawnSpec,
};
use crate::pane_log::{
    ByteBudget, ByteReservation, CatchUp, PaneEvent, PaneLog, PaneLogWatch, DEFAULT_PANE_LOG_BYTES,
    PANE_FRAME_OVERHEAD, PTY_OUTPUT_BUDGET_BYTES,
};
use crate::pane_log_persist::{
    replay_event, reset_output_event, restore_emulator, CaptureMarks, PendingPane, PersistPane,
    ScreenCodec,
};

const FROM_PTY_CAP: usize = 64;
const TO_CHILD_CAP: usize = 32;
const MAX_DRAIN_PER_PANE: usize = 16;
const MAX_SCROLLBACK: usize = 10_000;
#[cfg(windows)]
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(windows)]
const EXIT_OUTPUT_QUIET: Duration = Duration::from_millis(50);

struct PtyChunk {
    bytes: Vec<u8>,
    _reservation: Option<ByteReservation>,
}

impl PtyChunk {
    fn eof() -> Self {
        Self {
            bytes: Vec::new(),
            _reservation: None,
        }
    }
}

/// Hive env key folded into a pane at spawn (ADR-0037). Captured once; never
/// re-read from the live process environment after death.
const HIVE_CELL_ENV: &str = "HIVE_CELL";

/// Decimal pane id. Never reused for the Domain lifetime.
pub const PRISMATTYC_PANE_ID: &str = "PRISMATTYC_PANE_ID";
/// Absolute Unix-socket path of this mux.
pub const PMUX_SOCKET: &str = "PMUX_SOCKET";
/// Bound agent id of the pane's session, when the session was created with
/// `--agent`. Lets any in-pane tool derive its fabric identity from the seat
/// instead of a separate registration.
pub const PMUX_AGENT: &str = "PMUX_AGENT";
/// VectorVault tutorial pack manifest for this mux. Always stamped so an
/// agent that joins a session inherits the tutorial (`crates/prismattyc-mux/tutorial.md`).
pub const PMUX_TUTORIAL_PACK: &str = "PMUX_TUTORIAL_PACK";
/// Manifest task id the tutorial pack resolves through.
pub(crate) const TUTORIAL_PACK_MANIFEST: &str = "prismattyc-tutorial-pack";
/// Not stamped. MovePane can change session without respawn, so a session
/// key would go stale. Still stripped so a caller cannot inject one.
const PRISMATTYC_SESSION_ID: &str = "PRISMATTYC_SESSION_ID";

/// Stamp or clear guest discovery keys. Caller-supplied values lose.
pub(crate) fn stamp_prism_guest_env(
    env: &mut BTreeMap<String, String>,
    pane_id: u64,
    mux_socket: Option<&Path>,
    agent_id: Option<&str>,
) {
    env.insert(PRISMATTYC_PANE_ID.to_string(), pane_id.to_string());
    env.insert(
        PMUX_TUTORIAL_PACK.to_string(),
        TUTORIAL_PACK_MANIFEST.to_string(),
    );
    match mux_socket
        .and_then(Path::to_str)
        .filter(|path| !path.is_empty())
    {
        Some(path) => {
            env.insert(PMUX_SOCKET.to_string(), path.to_string());
        }
        None => {
            env.remove(PMUX_SOCKET);
        }
    }
    match agent_id.map(str::trim).filter(|agent| !agent.is_empty()) {
        Some(agent) => {
            env.insert(PMUX_AGENT.to_string(), agent.to_string());
        }
        None => {
            env.remove(PMUX_AGENT);
        }
    }
    env.remove(PRISMATTYC_SESSION_ID);
}

/// A fresh program resume token for a pane running `program`, or None when
/// the server does not take reports or the program reaches another machine
/// (the token must not leave this one).
fn resume_token_for(program: &str, policy: &crate::program_resume::ResumePolicy) -> Option<String> {
    if !policy.enabled() || crate::program_resume::reaches_another_machine(program) {
        return None;
    }
    crate::program_resume::new_token()
        .inspect_err(|error| eprintln!("pmuxd: {error:#}"))
        .ok()
}

#[derive(Debug)]
pub(crate) enum LiveWriteError {
    UnknownPane,
    Backpressure,
    Disconnected,
}

/// One drain-tick of visible output or liveness change for a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputActivityTick {
    pub pane_id: u64,
    pub revision: u64,
    pub child_alive: bool,
    /// Latest validated agent-attention message drained this pass.
    pub attention: Option<String>,
    /// PTY payload bytes drained this pass. Spinner ticks are small;
    /// streamed replies are not.
    pub nbytes: usize,
}

/// One liveness-edge notice for a Hive-seated pane (ADR-0037 §3 / wire contract B).
///
/// Produced exactly once per pane when `child_alive` flips true→false. `pid` and
/// `status` are diagnostic only — Hive must not authenticate on them. Empty status
/// (`exit_code` and `signal` both `None`) is valid when try_wait has not yet
/// returned (reader EOF before the wait status is ready).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellExitNotice {
    pub pane_id: u64,
    /// `HIVE_CELL` as injected at spawn (carries generation).
    pub cell: String,
    /// Last known OS pid; correlation / logging only.
    pub pid: Option<u32>,
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

enum ChildExitState {
    Running {
        #[cfg(windows)]
        next_poll: Instant,
    },
    #[cfg(windows)]
    Draining {
        status: portable_pty::ExitStatus,
        quiet_since: Option<Instant>,
        cancelling_reader: bool,
    },
    Exited,
}

struct LivePane {
    emulator: Emulator,
    session: PtySession,
    from_pty_rx: mpsc::Receiver<std::io::Result<PtyChunk>>,
    #[cfg(windows)]
    reader_thread: thread::JoinHandle<()>,
    #[cfg(all(test, windows))]
    reader_publish_delay_ms: Arc<AtomicU64>,
    /// The reader and writer threads, which a live update can stop.
    #[cfg(unix)]
    io: PaneIo,
    /// The pane holder that owns this pane's console, when there is one.
    #[cfg(windows)]
    holder: Option<crate::pty_holder::HolderHandle>,
    to_child_tx: mpsc::SyncSender<Vec<u8>>,
    cols: usize,
    outer_rows: usize,
    rows: usize,
    /// Cell pixels for TIOCGWINSZ / XTWINOPS. Nominal until a viewer reports
    /// its font (mux-server has none of its own).
    cell_w: u32,
    cell_h: u32,
    revision: u64,
    exit: ChildExitState,
    /// `HIVE_CELL` from `SpawnSpec.env` at spawn, if present. Absent ⇒ not a Hive seat.
    hive_cell: Option<String>,
    /// PID captured at spawn (and kept for the exit notice). Diagnostic only.
    child_pid: Option<u32>,
    controller_id: Option<u64>,
    /// Ordered PTY/side-channel log. Seq is a real index, not `revision`.
    log: PaneLog,
    /// Last OSC 7 path already written to `log`.
    logged_cwd: Option<PathBuf>,
    watch: std::sync::Arc<PaneLogWatch>,
    /// Copied onto the next logged Resize (PT-202).
    size_owner: Option<crate::remote_size::SizeOwner>,
    /// OSC 7577 token and record for this pane.
    resume: crate::program_resume::PaneResume,
    /// Last time the resume record's owner was checked.
    resume_checked: Option<Instant>,
}

#[cfg(windows)]
impl Drop for LivePane {
    fn drop(&mut self) {
        // Dropping `session` ends the child and closes the pseudoconsole.
        // The close waits until the console host exits, so keep the reader's
        // channel drained until then. A host that has just started first asks
        // for the cursor position and does nothing else until it gets an
        // answer, so answer it here. The reader thread then sees end of file
        // and this thread ends with it.
        let (_, closed) = mpsc::sync_channel(0);
        let output = std::mem::replace(&mut self.from_pty_rx, closed);
        let input = self.to_child_tx.clone();
        let _ = thread::Builder::new()
            .name("prism-server-pane-close".into())
            .spawn(move || {
                let mut request = CursorRequest::default();
                while let Ok(chunk) = output.recv() {
                    let Ok(chunk) = chunk else { continue };
                    if request.seen_in(&chunk.bytes) {
                        let _ = input.send(CURSOR_POSITION_REPLY.to_vec());
                    }
                }
            });
    }
}

/// What a console host asks for when it starts.
#[cfg(any(windows, test))]
const CURSOR_POSITION_REQUEST: &[u8] = b"\x1b[6n";
/// Any position will do: the host is about to close.
#[cfg(windows)]
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";

/// Finds cursor position requests in output that arrives in chunks.
#[cfg(any(windows, test))]
#[derive(Default)]
struct CursorRequest {
    tail: Vec<u8>,
}

#[cfg(any(windows, test))]
impl CursorRequest {
    /// True if a request ends in `bytes`, counting one split across chunks.
    fn seen_in(&mut self, bytes: &[u8]) -> bool {
        let keep = CURSOR_POSITION_REQUEST.len() - 1;
        self.tail.extend_from_slice(bytes);
        let seen = self
            .tail
            .windows(CURSOR_POSITION_REQUEST.len())
            .any(|window| window == CURSOR_POSITION_REQUEST);
        let drop = self.tail.len().saturating_sub(keep);
        self.tail.drain(..drop);
        seen
    }
}

/// A pane's reader and writer threads on unix.
///
/// The reader waits on the PTY and on a wake socket together, so a live update
/// can stop it without reading more: output it has not read stays in the PTY
/// for the next pmux server. Both threads own duplicates of the master, so
/// stopping them never closes the pane's PTY or sends its program end of file.
#[cfg(unix)]
struct PaneIo {
    wake: std::os::unix::net::UnixStream,
    reader: thread::JoinHandle<()>,
    writer: thread::JoinHandle<()>,
}

/// A pane's output receiver, input sender and the threads behind them.
#[cfg(unix)]
type PaneIoParts = (
    mpsc::Receiver<std::io::Result<PtyChunk>>,
    mpsc::SyncSender<Vec<u8>>,
    PaneIo,
);

#[cfg(unix)]
impl PaneIo {
    fn start(session: &PtySession, pane_id: u64) -> Result<PaneIoParts> {
        let mut child_writer = std::fs::File::from(session.dup_master()?);
        let pty_reader = std::fs::File::from(session.dup_master()?);
        let (wake, wake_reader) = std::os::unix::net::UnixStream::pair()?;

        let (to_child_tx, to_child_rx) = mpsc::sync_channel::<Vec<u8>>(TO_CHILD_CAP);
        let writer = thread::Builder::new()
            .name(format!("prism-server-pane-{pane_id}-write"))
            .spawn(move || {
                while let Ok(bytes) = to_child_rx.recv() {
                    if child_writer.write_all(&bytes).is_err() {
                        break;
                    }
                    if child_writer.flush().is_err() {
                        break;
                    }
                }
            })?;

        let (from_pty_tx, from_pty_rx) =
            mpsc::sync_channel::<std::io::Result<PtyChunk>>(FROM_PTY_CAP);
        let budget = ByteBudget::new(PTY_OUTPUT_BUDGET_BYTES);
        let reader = thread::Builder::new()
            .name(format!("prism-server-pane-{pane_id}-read"))
            .spawn(move || read_pty(pty_reader, &wake_reader, &from_pty_tx, &budget))?;
        Ok((
            from_pty_rx,
            to_child_tx,
            Self {
                wake,
                reader,
                writer,
            },
        ))
    }

    /// Ask both threads to stop. The writer finishes the input already queued
    /// once its sender is gone; the reader stops at its next wait.
    fn request_stop(&mut self) {
        let _ = (&self.wake).write_all(b"s");
    }

    fn stopped(&self) -> bool {
        self.reader.is_finished() && self.writer.is_finished()
    }
}

/// Read PTY output until end of file, an error, or a byte on `wake`.
#[cfg(unix)]
fn read_pty(
    mut pty: std::fs::File,
    wake: &std::os::unix::net::UnixStream,
    tx: &mpsc::SyncSender<std::io::Result<PtyChunk>>,
    budget: &ByteBudget,
) {
    use std::os::fd::AsRawFd;

    let mut buffer = vec![0u8; 8192];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: pty.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd entries.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = tx.send(Err(error));
            return;
        }
        if fds[1].revents != 0 {
            // Stopped for a live update: leave unread output in the PTY.
            return;
        }
        if fds[0].revents == 0 {
            continue;
        }
        let count = match pty.read(&mut buffer) {
            Ok(count) => count,
            // Every slave is closed.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => 0,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = tx.send(Err(error));
                return;
            }
        };
        if count == 0 {
            let _ = tx.send(Ok(PtyChunk::eof()));
            return;
        }
        let Some((bytes, reservation)) = budget
            .admit(PANE_FRAME_OVERHEAD.saturating_add(count), || {
                buffer[..count].to_vec()
            })
        else {
            let _ = tx.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PTY output chunk exceeds byte budget",
            )));
            return;
        };
        if tx
            .send(Ok(PtyChunk {
                bytes,
                _reservation: Some(reservation),
            }))
            .is_err()
        {
            return;
        }
    }
}

/// Discovery values stamped into the pane child environment.
#[derive(Clone, Copy, Default)]
pub(crate) struct GuestStamp<'a> {
    pub mux_socket: Option<&'a Path>,
    pub agent_id: Option<&'a str>,
}

/// Test hook that delays publishing each read chunk (Windows tests only).
#[cfg(all(test, windows))]
type ReaderDelay = Arc<AtomicU64>;
#[cfg(all(windows, not(test)))]
type ReaderDelay = ();

/// A Windows pane's reader and writer threads over its session.
#[cfg(windows)]
#[allow(clippy::type_complexity)]
fn start_windows_io(
    session: &mut PtySession,
    pane_id: u64,
) -> Result<(
    mpsc::Receiver<std::io::Result<PtyChunk>>,
    mpsc::SyncSender<Vec<u8>>,
    thread::JoinHandle<()>,
    ReaderDelay,
)> {
    let mut child_writer = session.take_input_writer()?;
    let mut pty_reader = session.take_reader()?;

    let (to_child_tx, to_child_rx) = mpsc::sync_channel::<Vec<u8>>(TO_CHILD_CAP);
    thread::Builder::new()
        .name(format!("prism-server-pane-{pane_id}-write"))
        .spawn(move || {
            while let Ok(bytes) = to_child_rx.recv() {
                if child_writer.write_all(&bytes).is_err() {
                    break;
                }
                if child_writer.flush().is_err() {
                    break;
                }
            }
        })?;

    let (from_pty_tx, from_pty_rx) = mpsc::sync_channel::<std::io::Result<PtyChunk>>(FROM_PTY_CAP);
    let output_budget = ByteBudget::new(PTY_OUTPUT_BUDGET_BYTES);
    let reader_budget = output_budget.clone();
    #[cfg(all(test, windows))]
    let reader_publish_delay_ms = Arc::new(AtomicU64::new(0));
    #[cfg(all(test, windows))]
    let reader_publish_delay_thread = Arc::clone(&reader_publish_delay_ms);
    let reader_thread = thread::Builder::new()
        .name(format!("prism-server-pane-{pane_id}-read"))
        .spawn(move || {
            let mut buffer = vec![0u8; 8192];
            loop {
                match pty_reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = from_pty_tx.send(Ok(PtyChunk::eof()));
                        break;
                    }
                    Ok(count) => {
                        #[cfg(all(test, windows))]
                        {
                            use std::sync::atomic::Ordering;
                            let delay_ms = reader_publish_delay_thread.load(Ordering::Acquire);
                            if delay_ms > 0 {
                                thread::sleep(Duration::from_millis(delay_ms));
                            }
                        }
                        let Some((bytes, reservation)) = reader_budget
                            .admit(PANE_FRAME_OVERHEAD.saturating_add(count), || {
                                buffer[..count].to_vec()
                            })
                        else {
                            let _ = from_pty_tx.send(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "PTY output chunk exceeds byte budget",
                            )));
                            break;
                        };
                        if from_pty_tx
                            .send(Ok(PtyChunk {
                                bytes,
                                _reservation: Some(reservation),
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = from_pty_tx.send(Err(error));
                        break;
                    }
                }
            }
        })?;
    #[cfg(not(all(test, windows)))]
    let reader_publish_delay_ms = ();
    Ok((
        from_pty_rx,
        to_child_tx,
        reader_thread,
        reader_publish_delay_ms,
    ))
}

/// The screen a live-update record describes: its snapshot when it fits the
/// pane, with the log frames after it replayed. Returns the emulator, the
/// pane's columns and rows, and its cell pixels.
#[cfg(any(unix, windows))]
fn restored_screen(record: &crate::handoff::PaneHandoff) -> (Emulator, usize, usize, u32, u32) {
    let cols = usize::from(record.cols).max(1);
    let rows = usize::from(record.rows).max(1);
    let cell_w = record.cell_px.0.max(1);
    let cell_h = record.cell_px.1.max(1);
    let (mut emulator, replay_from) = match record.snapshot.clone().map(Emulator::import_state) {
        Some(Ok(emulator))
            if emulator.screen().columns() == cols && emulator.screen().rows() == rows =>
        {
            (emulator, record.snapshot_seq)
        }
        _ => (Emulator::new(cols, rows, MAX_SCROLLBACK), 0),
    };
    emulator.set_retain_alt_history(true);
    emulator.set_cell_pixels(cell_w, cell_h);
    for frame in record.tail.iter().filter(|frame| frame.seq > replay_from) {
        replay_event(&mut emulator, &frame.event);
    }
    (emulator, cols, rows, cell_w, cell_h)
}

/// A pane's holder process on Windows, when it has one.
#[cfg(windows)]
type PaneHolder = Option<crate::pty_holder::HolderHandle>;
#[cfg(not(windows))]
type PaneHolder = ();

/// Start a pane's program. On Windows it runs in a pane holder process when
/// holders are on, so a server update can hand it over (see
/// [`crate::pty_holder`]). Elsewhere, and with holders off, the program's
/// PTY belongs to this process.
fn spawn_pane_session(
    program: &str,
    argv: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    size: portable_pty::PtySize,
) -> Result<(PtySession, PaneHolder)> {
    #[cfg(windows)]
    if pane_holders_enabled() {
        let (session, holder) = crate::pty_holder::spawn_holder(program, argv, cwd, env, size)?;
        return Ok((session, Some(holder)));
    }
    Ok((
        PtySession::spawn_config(program, argv, cwd, env, size)?,
        PaneHolder::default(),
    ))
}

/// Holders run this executable with `--pty-holder`, so only pmuxd itself
/// can start them; a test binary that spawns panes keeps them in process.
#[cfg(windows)]
fn pane_holders_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let is_server = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                exe.file_stem()
                    .map(|stem| stem.eq_ignore_ascii_case("pmuxd"))
            })
            .unwrap_or(false);
        is_server
            && crate::load_mux_section(&crate::prism_config_path())
                .map(|section| section.pty_holder())
                .unwrap_or(true)
    })
}

impl LivePane {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        pane_id: u64,
        spawn: &SpawnSpec,
        cols: usize,
        rows: usize,
        cell_w: u32,
        cell_h: u32,
        stamp: GuestStamp<'_>,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> Result<Self> {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        let size = pty_size_with_cell_pixels(cols, rows, cell_w, cell_h);
        let mut env = spawn.env.clone();
        stamp_prism_guest_env(&mut env, pane_id, stamp.mux_socket, stamp.agent_id);
        let resume_token = resume_token_for(&spawn.program, crate::program_resume::policy());
        match &resume_token {
            Some(token) => {
                env.insert(crate::program_resume::TOKEN_ENV.into(), token.clone());
            }
            None => {
                env.remove(crate::program_resume::TOKEN_ENV);
            }
        }
        let (session, holder) = spawn_pane_session(
            &spawn.program,
            &spawn.argv,
            spawn.cwd.as_deref(),
            &env,
            size,
        )
        .with_context(|| format!("spawn {:?} for pane {pane_id}", spawn.program))?;
        #[cfg(not(windows))]
        let () = holder;
        #[cfg(windows)]
        let mut session = session;
        // Capture seat identity at spawn from the inject map — never re-read after death
        // (ADR-0037 §3 / wire contract B). Empty string is treated as absent (not a seat).
        let hive_cell = spawn
            .env
            .get(HIVE_CELL_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let child_pid = session.process_id();
        #[cfg(unix)]
        let (from_pty_rx, to_child_tx, io) = PaneIo::start(&session, pane_id)?;
        #[cfg(windows)]
        let (from_pty_rx, to_child_tx, reader_thread, reader_publish_delay_ms) =
            start_windows_io(&mut session, pane_id)?;
        #[cfg(all(windows, not(test)))]
        let () = reader_publish_delay_ms;

        // server panes retain alt-screen history so attach scroll
        // mode can page through TUI output. Classic hosts never opt in.
        let mut emulator = Emulator::new(cols, rows, MAX_SCROLLBACK);
        emulator.set_retain_alt_history(true);
        emulator.set_cell_pixels(cell_w, cell_h);
        Ok(Self {
            #[cfg(windows)]
            holder,
            emulator,
            session,
            from_pty_rx,
            #[cfg(windows)]
            reader_thread,
            #[cfg(all(test, windows))]
            reader_publish_delay_ms,
            #[cfg(unix)]
            io,
            to_child_tx,
            cols,
            outer_rows: rows,
            rows,
            cell_w,
            cell_h,
            revision: 0,
            exit: ChildExitState::Running {
                #[cfg(windows)]
                next_poll: Instant::now() + EXIT_POLL_INTERVAL,
            },
            hive_cell,
            child_pid,
            controller_id: None,
            log: PaneLog::new(DEFAULT_PANE_LOG_BYTES),
            logged_cwd: None,
            watch,
            size_owner: None,
            resume: crate::program_resume::PaneResume::new(resume_token, None),
            resume_checked: None,
        })
    }

    /// Stop the reader and writer for a live update. Output not yet read stays
    /// in the PTY; input already queued is still written. Writes after this
    /// fail as disconnected.
    #[cfg(unix)]
    fn stop_io(&mut self) {
        self.io.request_stop();
        let (closed, _) = mpsc::sync_channel(0);
        self.to_child_tx = closed;
    }

    /// Apply output a stopping reader already queued. Unlike [`LivePane::drain`]
    /// a closed channel is not a death: the reader closes it when it stops.
    /// End of file and read errors still are. Returns any exit notice and the
    /// bytes applied. On Windows a paused holder's reader waits instead of
    /// stopping, so the same applies.
    #[cfg(any(unix, windows))]
    fn drain_stopping(&mut self, pane_id: u64) -> (Option<CellExitNotice>, usize) {
        if !self.child_alive() {
            return (None, 0);
        }
        let mut bytes = 0usize;
        loop {
            match self.from_pty_rx.try_recv() {
                Ok(Ok(chunk)) if chunk.bytes.is_empty() => {
                    return (self.observe_death(pane_id), bytes);
                }
                Ok(Ok(chunk)) => {
                    bytes = bytes.saturating_add(chunk.bytes.len());
                    let _ = self.apply_pty_chunk(chunk);
                }
                Ok(Err(_)) => return (self.observe_death(pane_id), bytes),
                Err(_) => return (None, bytes),
            }
        }
    }

    /// Start new reader and writer threads after a live update was abandoned.
    #[cfg(unix)]
    fn restart_io(&mut self, pane_id: u64) -> Result<()> {
        let (from_pty_rx, to_child_tx, io) = PaneIo::start(&self.session, pane_id)?;
        self.from_pty_rx = from_pty_rx;
        self.to_child_tx = to_child_tx;
        self.io = io;
        Ok(())
    }

    /// The pane as a live-update record, reached through `transport`.
    #[cfg(any(unix, windows))]
    fn to_handoff(
        &self,
        pane_id: u64,
        transport: crate::handoff::PaneTransport,
    ) -> crate::handoff::PaneHandoff {
        let snapshot = self.emulator.export_state().ok();
        let snapshot_seq = if snapshot.is_some() {
            self.log.current_seq()
        } else {
            self.log.oldest_seq().unwrap_or(1).saturating_sub(1)
        };
        let (resume_token, resume_record, resume_times) = self.resume.handoff_parts();
        crate::handoff::PaneHandoff {
            pane_id,
            #[cfg(unix)]
            fd: transport,
            #[cfg(windows)]
            holder: transport,
            pid: self.child_pid,
            exited: !self.child_alive(),
            cols: u16::try_from(self.cols).unwrap_or(u16::MAX),
            rows: u16::try_from(self.rows).unwrap_or(u16::MAX),
            outer_rows: u16::try_from(self.outer_rows).unwrap_or(u16::MAX),
            cell_px: (self.cell_w, self.cell_h),
            revision: self.revision,
            hive_cell: self.hive_cell.clone(),
            snapshot,
            snapshot_seq,
            tail: self.log.frames(),
            logged_cwd: self.logged_cwd.clone(),
            size_owner: self.size_owner,
            resume_token,
            resume_record,
            resume_times,
        }
    }

    /// Take over a pane from a live-update record and its kept master.
    #[cfg(unix)]
    fn adopt(
        record: crate::handoff::PaneHandoff,
        master: std::os::fd::OwnedFd,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> Result<Self> {
        let pid = record
            .pid
            .with_context(|| format!("pane {} has no program pid", record.pane_id))?;
        let session = PtySession::adopt(master, pid, record.exited)?;
        let (emulator, cols, rows, cell_w, cell_h) = restored_screen(&record);
        let (from_pty_rx, to_child_tx, io) = PaneIo::start(&session, record.pane_id)?;
        Ok(Self {
            emulator,
            session,
            from_pty_rx,
            io,
            to_child_tx,
            cols,
            outer_rows: usize::from(record.outer_rows).max(rows),
            rows,
            cell_w,
            cell_h,
            revision: record.revision.saturating_add(1),
            exit: if record.exited {
                ChildExitState::Exited
            } else {
                ChildExitState::Running {}
            },
            hive_cell: record.hive_cell,
            child_pid: Some(pid),
            controller_id: None,
            log: PaneLog::from_frames(record.tail, DEFAULT_PANE_LOG_BYTES),
            logged_cwd: record.logged_cwd,
            watch,
            size_owner: record.size_owner,
            resume: crate::program_resume::PaneResume::from_handoff(
                record.resume_token,
                record.resume_record,
                record.resume_times,
            ),
            resume_checked: None,
        })
    }

    /// Take over a pane from a Windows live-update record by reconnecting to
    /// its holder. The holder's program keeps running.
    #[cfg(windows)]
    fn adopt(
        record: crate::handoff::PaneHandoff,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> Result<Self> {
        let (emulator, cols, rows, cell_w, cell_h) = restored_screen(&record);
        let size = pty_size_with_cell_pixels(cols, rows, cell_w, cell_h);
        let (mut session, holder) = crate::pty_holder::adopt_holder(&record.holder, size)?;
        let child_pid = session.process_id().or(record.pid);
        let (from_pty_rx, to_child_tx, reader_thread, reader_publish_delay_ms) =
            start_windows_io(&mut session, record.pane_id)?;
        #[cfg(not(test))]
        let () = reader_publish_delay_ms;
        Ok(Self {
            holder: Some(holder),
            emulator,
            session,
            from_pty_rx,
            reader_thread,
            #[cfg(test)]
            reader_publish_delay_ms,
            to_child_tx,
            cols,
            outer_rows: usize::from(record.outer_rows).max(rows),
            rows,
            cell_w,
            cell_h,
            revision: record.revision.saturating_add(1),
            exit: if record.exited {
                ChildExitState::Exited
            } else {
                ChildExitState::Running {
                    next_poll: Instant::now() + EXIT_POLL_INTERVAL,
                }
            },
            hive_cell: record.hive_cell,
            child_pid,
            controller_id: None,
            log: PaneLog::from_frames(record.tail, DEFAULT_PANE_LOG_BYTES),
            logged_cwd: record.logged_cwd,
            watch,
            size_owner: record.size_owner,
            resume: crate::program_resume::PaneResume::from_handoff(
                record.resume_token,
                record.resume_record,
                record.resume_times,
            ),
            resume_checked: None,
        })
    }

    /// At most once a second: forget a resume record whose owner has exited.
    fn refresh_resume_owner(&mut self) {
        if self.resume.record().is_none() {
            return;
        }
        let now = Instant::now();
        if self
            .resume_checked
            .is_some_and(|checked| now.duration_since(checked) < Duration::from_secs(1))
        {
            return;
        }
        self.resume_checked = Some(now);
        let lookup = crate::program_resume::SystemLookup {
            root: self.child_pid,
        };
        self.resume
            .refresh(crate::program_resume::unix_now(), &lookup);
    }

    fn record(&mut self, event: PaneEvent) {
        self.log.append(event);
        self.watch.note();
    }

    fn child_alive(&self) -> bool {
        !matches!(self.exit, ChildExitState::Exited)
    }

    #[cfg(windows)]
    fn cancel_reader_io(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::IO::CancelSynchronousIo;

        // ERROR_NOT_FOUND is expected if the read already completed. The next
        // drain tick retries after the reader publishes and blocks again.
        let _ = unsafe { CancelSynchronousIo(self.reader_thread.as_raw_handle()) };
    }

    /// Flip `child_alive` true→false and, if this pane is a Hive seat, produce at most
    /// one [`CellExitNotice`]. Subsequent calls are no-ops (idempotent on the edge).
    fn observe_death(&mut self, pane_id: u64) -> Option<CellExitNotice> {
        if !self.child_alive() {
            return None;
        }
        let status = match std::mem::replace(&mut self.exit, ChildExitState::Exited) {
            ChildExitState::Running { .. } => self.session.try_wait().ok().flatten(),
            #[cfg(windows)]
            ChildExitState::Draining { status, .. } => Some(status),
            ChildExitState::Exited => unreachable!("exited pane returned as alive"),
        };
        self.emulator.reset_input_reporting();
        self.record(PaneEvent::Output {
            bytes: prismattyc_emulator::RESET_INPUT_REPORTING.to_vec(),
        });
        self.revision = self.revision.saturating_add(1);
        // Non-blocking: never wait on the child here — drain must not stall the mux loop.
        let (exit_code, signal) = status.as_ref().map_or((None, None), |status| {
            (Some(status.exit_code()), status.signal().map(str::to_owned))
        });
        self.record(PaneEvent::Exited {
            code: exit_code,
            signal: signal.clone(),
        });
        let cell = self.hive_cell.clone()?;
        Some(CellExitNotice {
            pane_id,
            cell,
            pid: self.child_pid,
            exit_code,
            signal,
        })
    }

    fn resize(&mut self, cols: usize, rows: usize, cell_w: u32, cell_h: u32) -> Result<()> {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        if self.cols == cols
            && self.outer_rows == rows
            && self.cell_w == cell_w
            && self.cell_h == cell_h
        {
            return Ok(());
        }
        self.cols = cols;
        self.outer_rows = rows;
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.resize_guest(self.cols, self.outer_rows.max(1))
    }

    fn resize_guest(&mut self, cols: usize, rows: usize) -> Result<()> {
        let size = pty_size_with_cell_pixels(cols, rows, self.cell_w, self.cell_h);
        let cells_changed = self.emulator.screen().columns() != cols || self.rows != rows;
        self.session.resize(size)?;
        let pixels_changed = self.emulator.set_cell_pixels(self.cell_w, self.cell_h);
        if !cells_changed && !pixels_changed {
            return Ok(());
        }
        if cells_changed {
            self.emulator.resize(cols, rows);
        }
        self.record(PaneEvent::Resize {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: u16::try_from(rows).unwrap_or(u16::MAX),
            cell_px: (self.cell_w, self.cell_h),
            size_owner: self.size_owner,
            reflow: true,
        });
        self.cols = cols;
        self.rows = rows;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    #[cfg(test)]
    fn apply_pty_bytes(&mut self, bytes: &[u8]) -> Option<String> {
        self.apply_pty_chunk(PtyChunk {
            bytes: bytes.to_vec(),
            _reservation: None,
        })
    }

    /// Feed one admitted PTY chunk, then log its output and derived side-channel events.
    fn apply_pty_chunk(&mut self, chunk: PtyChunk) -> Option<String> {
        let PtyChunk {
            bytes,
            _reservation: reservation,
        } = chunk;
        let _ = self.emulator.feed(&bytes);
        for reply in self.emulator.take_pending_replies() {
            let _ = self.to_child_tx.try_send(reply);
        }
        self.revision = self.revision.saturating_add(1);
        let title = self.emulator.take_pending_title();
        let cwd = self.emulator.cwd().and_then(|path| {
            if self.logged_cwd.as_deref() == Some(path) {
                None
            } else {
                self.logged_cwd = Some(path.to_path_buf());
                Some(path.to_path_buf())
            }
        });
        let attention = self.emulator.take_pending_attention();
        let reports = self.emulator.take_resume_reports();
        if !reports.is_empty() {
            let lookup = crate::program_resume::SystemLookup {
                root: self.child_pid,
            };
            let now = crate::program_resume::unix_now();
            for body in reports {
                match self.resume.receive(&body, now, &lookup) {
                    crate::program_resume::Outcome::Rejected(reason) => {
                        eprintln!("pmuxd: program resume report refused: {reason}");
                    }
                    crate::program_resume::Outcome::PaneNotTrusted {
                        reason,
                        first: true,
                    } => {
                        eprintln!(
                            "pmuxd: program resume reports from a pane are ignored while {reason}"
                        );
                    }
                    _ => {}
                }
            }
        }

        // Move the one admitted allocation into the ordered output frame.
        self.record(PaneEvent::Output { bytes });
        if let Some(text) = title {
            self.record(PaneEvent::Title { text });
        }
        if let Some(path) = cwd {
            self.record(PaneEvent::Cwd { path });
        }
        if let Some(ref text) = attention {
            self.record(PaneEvent::Attention { text: text.clone() });
        }
        drop(reservation);
        attention
    }

    /// Drain PTY output. Returns a cell-exit notice if this tick observed the
    /// liveness edge for a Hive-seated pane, plus bytes and attention drained.
    fn drain(&mut self, pane_id: u64) -> (Option<CellExitNotice>, usize, Option<String>) {
        if !self.child_alive() {
            return (None, 0, None);
        }
        self.refresh_resume_owner();
        let mut died = false;
        let mut nbytes = 0usize;
        let mut attention = None;
        for _ in 0..MAX_DRAIN_PER_PANE {
            match self.from_pty_rx.try_recv() {
                Ok(Ok(chunk)) if chunk.bytes.is_empty() => {
                    died = true;
                    break;
                }
                Ok(Ok(chunk)) => {
                    nbytes = nbytes.saturating_add(chunk.bytes.len());
                    if let Some(message) = self.apply_pty_chunk(chunk) {
                        attention = Some(message);
                    }
                }
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    died = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        #[cfg(windows)]
        let exit_ready = {
            // ConPTY can leave the output pipe open after cmd.exe or
            // PowerShell exits, so also poll the child handle after consuming
            // all output currently available.
            let now = Instant::now();
            if !died {
                if let ChildExitState::Running { next_poll } = &mut self.exit {
                    if now >= *next_poll {
                        *next_poll = now + EXIT_POLL_INTERVAL;
                        if let Some(status) = self.session.try_wait().ok().flatten() {
                            self.exit = ChildExitState::Draining {
                                status,
                                quiet_since: None,
                                cancelling_reader: false,
                            };
                        }
                    }
                }
            }
            let mut cancel_reader = false;
            if !died {
                match &mut self.exit {
                    ChildExitState::Draining {
                        cancelling_reader: true,
                        ..
                    } => cancel_reader = true,
                    ChildExitState::Draining {
                        quiet_since,
                        cancelling_reader: false,
                        ..
                    } if nbytes > 0 => *quiet_since = None,
                    ChildExitState::Draining {
                        quiet_since,
                        cancelling_reader,
                        ..
                    } => {
                        let quiet_since = quiet_since.get_or_insert(now);
                        if now.duration_since(*quiet_since) >= EXIT_OUTPUT_QUIET {
                            *cancelling_reader = true;
                            cancel_reader = true;
                        }
                    }
                    ChildExitState::Running { .. } => {}
                    ChildExitState::Exited => unreachable!("exited pane reached drain"),
                }
            }
            if cancel_reader {
                self.cancel_reader_io();
            }
            // Once cancellation starts, the reader publishes any completed
            // read before its sender disconnects. FIFO channel order makes
            // `died` the producer-completion edge.
            died
        };
        #[cfg(not(windows))]
        let exit_ready = died;
        let notice = if exit_ready {
            self.observe_death(pane_id)
        } else {
            None
        };
        (notice, nbytes, attention)
    }

    fn content(&self, pane_id: u64) -> PaneContent {
        let screen = self.emulator.screen();
        let mut lines = Vec::with_capacity(screen.rows());
        for row in 0..screen.rows() {
            let mut line = String::new();
            if let Some(cells) = screen.view_row(row) {
                for cell in cells {
                    cell.write_grapheme_into(&mut line);
                }
            }
            lines.push(line);
        }
        let cursor = screen.cursor();
        PaneContent {
            pane_id,
            revision: self.revision,
            cols: self.cols as u32,
            rows: self.rows as u32,
            cursor_row: cursor.row as u32,
            cursor_col: cursor.column as u32,
            cursor_visible: self.emulator.cursor_visible(),
            alt_active: screen.alt_active(),
            child_alive: self.child_alive(),
            child_pid: self.session.process_id(),
            lines,
            cursor_shape: Some(CursorShapeWire::from_emulator(self.emulator.cursor_shape())),
        }
    }

    fn styled(&self, pane_id: u64, view_offset: Option<u32>) -> PaneStyled {
        let screen = self.emulator.screen();
        // history depth is mode-agnostic so alt-screen TUI output
        // stays scrollable through attach; classic max_view_scroll would
        // report 0 on the alt screen.
        let max = u32::try_from(screen.history_len()).unwrap_or(u32::MAX);
        let offset = view_offset.unwrap_or(0).min(max);
        let child_mouse_tracking = Some(self.emulator.mouse_tracking().is_on());
        let child_mouse_sgr = Some(self.emulator.mouse_sgr());
        if offset == 0 {
            return PaneStyled {
                content: self.content(pane_id),
                runs: (0..screen.rows())
                    .map(|row| screen.view_row(row).map(rle_style_runs).unwrap_or_default())
                    .collect(),
                view_offset: Some(0),
                max_view_scroll: Some(max),
                child_mouse_tracking,
                child_mouse_sgr,
            };
        }

        let cols = screen.columns();
        let rows = screen.rows();
        let mut lines = Vec::with_capacity(rows);
        let mut runs = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut cells = Vec::with_capacity(cols);
            let mut line = String::new();
            for col in 0..cols {
                let cell = screen.history_view_cell(offset as usize, row, col);
                cell.write_grapheme_into(&mut line);
                cells.push(cell);
            }
            lines.push(line);
            runs.push(rle_style_runs(cells));
        }
        PaneStyled {
            content: PaneContent {
                pane_id,
                revision: self.revision,
                cols: self.cols as u32,
                rows: self.rows as u32,
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: false,
                alt_active: screen.alt_active(),
                child_alive: self.child_alive(),
                child_pid: self.session.process_id(),
                lines,
                cursor_shape: Some(CursorShapeWire::from_emulator(self.emulator.cursor_shape())),
            },
            runs,
            view_offset: Some(offset),
            max_view_scroll: Some(max),
            child_mouse_tracking,
            child_mouse_sgr,
        }
    }
}

pub(crate) struct LiveRuntime {
    panes: HashMap<u64, LivePane>,
    mux_socket: Option<PathBuf>,
    watch: std::sync::Arc<PaneLogWatch>,
    pending_size_owner: Option<crate::remote_size::SizeOwner>,
}

impl LiveRuntime {
    pub(crate) fn set_pending_size_owner(&mut self, owner: Option<crate::remote_size::SizeOwner>) {
        self.pending_size_owner = owner;
    }

    pub(crate) fn record_size_owner(
        &mut self,
        pane_id: u64,
        owner: Option<crate::remote_size::SizeOwner>,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.size_owner = owner;
            pane.record(PaneEvent::SizeOwnerChanged { owner });
        }
    }

    #[cfg(test)]
    pub(crate) fn spawn(
        geometry: &[PaneGeometry],
        spawns: &HashMap<u64, SpawnSpec>,
        mux_socket: Option<PathBuf>,
    ) -> Result<Self> {
        Self::spawn_with_watch(geometry, spawns, mux_socket, PaneLogWatch::new())
    }

    pub(crate) fn spawn_with_watch(
        geometry: &[PaneGeometry],
        spawns: &HashMap<u64, SpawnSpec>,
        mux_socket: Option<PathBuf>,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> Result<Self> {
        let mut panes = HashMap::new();
        for pane in geometry {
            let spawn = spawns
                .get(&pane.pane_id)
                .with_context(|| format!("missing spawn spec for pane {}", pane.pane_id))?;
            // Bootstrap panes belong to the unbound default session: no agent.
            let runtime = LivePane::spawn(
                pane.pane_id,
                spawn,
                pane.cols as usize,
                pane.rows as usize,
                NOMINAL_CELL_W_PX,
                NOMINAL_CELL_H_PX,
                GuestStamp {
                    mux_socket: mux_socket.as_deref(),
                    agent_id: None,
                },
                std::sync::Arc::clone(&watch),
            )?;
            panes.insert(pane.pane_id, runtime);
        }
        Ok(Self {
            panes,
            mux_socket,
            watch,
            pending_size_owner: None,
        })
    }

    pub(crate) fn spawn_pane(
        &mut self,
        pane: PaneGeometry,
        spawn: &SpawnSpec,
        sibling_id: Option<u64>,
        agent_id: Option<&str>,
    ) -> Result<()> {
        if self.panes.contains_key(&pane.pane_id) {
            anyhow::bail!("runtime already exists for pane {}", pane.pane_id);
        }
        let (cell_w, cell_h) = sibling_id
            .and_then(|id| self.panes.get(&id).map(|pane| (pane.cell_w, pane.cell_h)))
            .unwrap_or((NOMINAL_CELL_W_PX, NOMINAL_CELL_H_PX));
        let runtime = LivePane::spawn(
            pane.pane_id,
            spawn,
            pane.cols as usize,
            pane.rows as usize,
            cell_w,
            cell_h,
            GuestStamp {
                mux_socket: self.mux_socket.as_deref(),
                agent_id,
            },
            std::sync::Arc::clone(&self.watch),
        )?;
        self.panes.insert(pane.pane_id, runtime);
        Ok(())
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn has_pane(&self, pane_id: u64) -> bool {
        self.panes.contains_key(&pane_id)
    }

    pub(crate) fn remove_pane(&mut self, pane_id: u64) {
        if self.panes.remove(&pane_id).is_some() {
            self.watch.note();
        }
    }

    /// Stop every pane's reader and writer for a live update, and apply all
    /// output they already read. Returns the exit notices seen while draining.
    /// On an error some panes may still be running their threads: call
    /// [`LiveRuntime::resume_after_handoff`] before using the runtime again.
    #[cfg(unix)]
    pub(crate) fn quiesce_for_handoff(&mut self, timeout: Duration) -> Result<Vec<CellExitNotice>> {
        let deadline = Instant::now() + timeout;
        for pane in self.panes.values_mut() {
            pane.stop_io();
        }
        let mut notices = Vec::new();
        loop {
            let mut running = None;
            for (&id, pane) in &mut self.panes {
                // A reader blocked on a full channel stops only once drained.
                let (notice, _) = pane.drain_stopping(id);
                notices.extend(notice);
                if !pane.io.stopped() {
                    running = Some(id);
                }
            }
            let Some(id) = running else {
                // Both threads have ended: anything they sent is now queued.
                for (&id, pane) in &mut self.panes {
                    let (notice, _) = pane.drain_stopping(id);
                    notices.extend(notice);
                }
                return Ok(notices);
            };
            if Instant::now() >= deadline {
                anyhow::bail!("pane {id} did not stop reading and writing in {timeout:?}");
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Pause every pane's holder for a live update, and apply all output it
    /// sent before pausing. A pane without a holder cannot be handed over.
    /// On an error some holders may be paused: call
    /// [`LiveRuntime::resume_after_handoff`] before using the runtime again.
    #[cfg(windows)]
    pub(crate) fn quiesce_for_handoff(&mut self, timeout: Duration) -> Result<Vec<CellExitNotice>> {
        let deadline = Instant::now() + timeout;
        for (&id, pane) in &self.panes {
            let holder = pane
                .holder
                .as_ref()
                .with_context(|| format!("pane {id} runs without a pane holder"))?;
            holder
                .pause()
                .with_context(|| format!("pause the holder of pane {id}"))?;
        }
        let mut notices = Vec::new();
        loop {
            let mut waiting = None;
            for (&id, pane) in &mut self.panes {
                let (notice, _) = pane.drain_stopping(id);
                notices.extend(notice);
                let paused = pane.holder.as_ref().is_some_and(|holder| holder.paused());
                if !paused && pane.child_alive() {
                    waiting = Some(id);
                }
            }
            let Some(id) = waiting else {
                // Every reader has returned all output before its pause.
                for (&id, pane) in &mut self.panes {
                    let (notice, _) = pane.drain_stopping(id);
                    notices.extend(notice);
                }
                return Ok(notices);
            };
            if Instant::now() >= deadline {
                anyhow::bail!("the holder of pane {id} did not pause in {timeout:?}");
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Let every holder send output again after a live update was abandoned.
    #[cfg(windows)]
    pub(crate) fn resume_after_handoff(&mut self) -> Result<()> {
        for (&id, pane) in &self.panes {
            if let Some(holder) = &pane.holder {
                holder
                    .resume()
                    .with_context(|| format!("resume the holder of pane {id}"))?;
            }
        }
        Ok(())
    }

    /// Restart every pane's reader and writer after a live update was
    /// abandoned, so the server carries on as before.
    #[cfg(unix)]
    pub(crate) fn resume_after_handoff(&mut self) -> Result<()> {
        for (&id, pane) in &mut self.panes {
            if pane.io.reader.is_finished() {
                pane.restart_io(id)?;
            }
        }
        Ok(())
    }

    /// Give every pane up for a live update, after
    /// [`LiveRuntime::quiesce_for_handoff`]. Each record comes with a duplicate
    /// of its PTY master; the programs keep running. On an error nothing has
    /// been given up.
    #[cfg(unix)]
    pub(crate) fn export_handoff(
        &mut self,
    ) -> Result<Vec<(crate::handoff::PaneHandoff, std::os::fd::OwnedFd)>> {
        use std::os::fd::AsRawFd;

        let mut ids: Vec<u64> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let pane = &self.panes[id];
            let master = pane
                .session
                .dup_master()
                .with_context(|| format!("keep the PTY of pane {id}"))?;
            out.push((pane.to_handoff(*id, master.as_raw_fd()), master));
        }
        for id in ids {
            if let Some(pane) = self.panes.remove(&id) {
                pane.session.abandon();
            }
        }
        self.watch.note();
        Ok(out)
    }

    /// Give every Windows pane up for a live update, after
    /// [`LiveRuntime::quiesce_for_handoff`]: each record names the pane's
    /// holder, and each holder is told to keep its program for the next
    /// server. On an error nothing has been given up.
    #[cfg(windows)]
    pub(crate) fn export_handoff(&mut self) -> Result<Vec<crate::handoff::KeptPane>> {
        let mut ids: Vec<u64> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let pane = &self.panes[id];
            let holder = pane
                .holder
                .as_ref()
                .with_context(|| format!("pane {id} runs without a pane holder"))?;
            out.push(pane.to_handoff(*id, holder.endpoint()));
        }
        for id in ids {
            if let Some(mut pane) = self.panes.remove(&id) {
                if let Some(holder) = &pane.holder {
                    if let Err(error) = holder.release(crate::handoff::HANDOFF_GRACE_MS) {
                        eprintln!(
                            "pmuxd: pane {id} holder did not take the handoff notice: {error}"
                        );
                    }
                }
                pane.session.release_program();
            }
        }
        self.watch.note();
        Ok(out)
    }

    /// Take over Windows panes from a live update by reconnecting to their
    /// holders. A pane that cannot be adopted is returned with the reason;
    /// its holder ends its program when the grace runs out.
    #[cfg(windows)]
    pub(crate) fn adopt_handoff(
        panes: Vec<crate::handoff::KeptPane>,
        mux_socket: Option<PathBuf>,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> (Self, Vec<(u64, anyhow::Error)>) {
        let mut runtime = Self {
            panes: HashMap::new(),
            mux_socket,
            watch,
            pending_size_owner: None,
        };
        let mut failed = Vec::new();
        for record in panes {
            let id = record.pane_id;
            match LivePane::adopt(record, std::sync::Arc::clone(&runtime.watch)) {
                Ok(pane) => {
                    runtime.panes.insert(id, pane);
                }
                Err(error) => failed.push((id, error)),
            }
        }
        runtime.watch.note();
        (runtime, failed)
    }

    /// Take over panes from a live update. A pane that cannot be adopted is
    /// returned with the reason; its master was closed, so its program gets a
    /// hangup, as in a normal restart.
    #[cfg(unix)]
    pub(crate) fn adopt_handoff(
        panes: Vec<(crate::handoff::PaneHandoff, std::os::fd::OwnedFd)>,
        mux_socket: Option<PathBuf>,
        watch: std::sync::Arc<PaneLogWatch>,
    ) -> (Self, Vec<(u64, anyhow::Error)>) {
        let mut runtime = Self {
            panes: HashMap::new(),
            mux_socket,
            watch,
            pending_size_owner: None,
        };
        let mut failed = Vec::new();
        for (record, master) in panes {
            let id = record.pane_id;
            match LivePane::adopt(record, master, std::sync::Arc::clone(&runtime.watch)) {
                Ok(pane) => {
                    runtime.panes.insert(id, pane);
                }
                Err(error) => failed.push((id, error)),
            }
        }
        runtime.watch.note();
        (runtime, failed)
    }

    /// End every pane: stop its program and close its pseudoconsole, so its
    /// console host exits with the daemon instead of outliving it.
    #[cfg(windows)]
    pub(crate) fn close_all(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.panes.clear();
        self.watch.note();
    }

    pub(crate) fn apply_geometry(
        &mut self,
        geometry: &[PaneGeometry],
        cell_w: Option<u32>,
        cell_h: Option<u32>,
    ) -> Result<()> {
        let mut resized = Vec::with_capacity(geometry.len());
        for pane in geometry {
            let runtime = self
                .panes
                .get_mut(&pane.pane_id)
                .with_context(|| format!("missing runtime for pane {}", pane.pane_id))?;
            runtime.size_owner = self.pending_size_owner;
            let prior = (
                runtime.cols,
                runtime.outer_rows,
                runtime.cell_w,
                runtime.cell_h,
            );
            let next_w = cell_w.filter(|w| *w > 0).unwrap_or(runtime.cell_w);
            let next_h = cell_h.filter(|h| *h > 0).unwrap_or(runtime.cell_h);
            if let Err(error) =
                runtime.resize(pane.cols as usize, pane.rows as usize, next_w, next_h)
            {
                for (pane_id, cols, rows, cw, ch) in resized.into_iter().rev() {
                    if let Some(runtime) = self.panes.get_mut(&pane_id) {
                        let _ = runtime.resize(cols, rows, cw, ch);
                    }
                }
                return Err(error).with_context(|| format!("resize pane {}", pane.pane_id));
            }
            resized.push((pane.pane_id, prior.0, prior.1, prior.2, prior.3));
        }
        Ok(())
    }

    /// The pane's program resume record, for snapshots and the saved workspace.
    pub(crate) fn resume_record(
        &self,
        pane_id: u64,
    ) -> Option<crate::program_resume::ResumeRecord> {
        let pane = self.panes.get(&pane_id)?;
        pane.resume
            .current_record(&crate::program_resume::SystemLookup {
                root: pane.child_pid,
            })
    }

    /// Give a restored pane its saved record, marked as being resumed when
    /// `resuming` so a failure is not retried.
    pub(crate) fn restore_resume_record(
        &mut self,
        pane_id: u64,
        record: crate::program_resume::ResumeRecord,
        resuming: bool,
        not_resumed: Option<&str>,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.resume.restore(record);
            if resuming {
                pane.resume.mark_attempted();
            }
            if let Some(reason) = not_resumed {
                pane.resume.note_not_resumed(reason);
            }
        }
    }

    /// The resumed program started as `pid`. Only a process inside the pane
    /// can claim the record.
    pub(crate) fn resume_started(&mut self, pane_id: u64, pid: u32) -> bool {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return false;
        };
        let lookup = crate::program_resume::SystemLookup {
            root: pane.child_pid,
        };
        use crate::program_resume::OwnerLookup as _;
        let Some(identity) = lookup.identity(pid) else {
            return false;
        };
        if !lookup.in_pane(pid) || lookup.server_uid() != Some(identity.uid) {
            return false;
        }
        pane.resume
            .resumed(pid, crate::program_resume::unix_now(), &lookup)
    }

    /// The resume could not start; drop the record.
    pub(crate) fn resume_failed(&mut self, pane_id: u64) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.resume.resume_failed();
        }
    }

    pub(crate) fn child_pid(&self, pane_id: u64) -> Option<u32> {
        self.panes
            .get(&pane_id)
            .filter(|pane| pane.child_alive())
            .and_then(|pane| pane.child_pid)
    }

    /// Active Kitty keyboard flags reported by the guest in this pane.
    pub(crate) fn keyboard_flags(&self, pane_id: u64) -> Option<u16> {
        self.panes
            .get(&pane_id)
            .map(|pane| pane.emulator.keyboard_flags())
    }

    #[cfg(test)]
    pub(crate) fn force_write_backpressure_for_test(&mut self, pane_id: u64) {
        let Some(runtime) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let (tx, rx) = mpsc::sync_channel(1);
        tx.try_send(vec![b'x']).expect("seed backpressure slot");
        runtime.to_child_tx = tx;
        std::mem::forget(rx);
    }

    #[cfg(all(test, windows))]
    fn set_reader_publish_delay_for_test(&self, pane_id: u64, delay: Duration) {
        use std::sync::atomic::Ordering;

        self.panes
            .get(&pane_id)
            .expect("pane for reader publication delay")
            .reader_publish_delay_ms
            .store(
                u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                Ordering::Release,
            );
    }

    pub(crate) fn write(&self, pane_id: u64, data: Vec<u8>) -> Result<(), LiveWriteError> {
        let runtime = self
            .panes
            .get(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        match runtime.to_child_tx.try_send(data) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(LiveWriteError::Backpressure),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(LiveWriteError::Disconnected),
        }
    }

    pub(crate) fn note_controller(&mut self, pane_id: u64, controller: Option<u64>) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.controller_id = controller;
        }
    }

    pub(crate) fn content(&self, pane_id: u64) -> Option<PaneContent> {
        self.panes
            .get(&pane_id)
            .map(|runtime| runtime.content(pane_id))
    }

    pub(crate) fn pane_state_snapshot(
        &self,
        pane_id: u64,
    ) -> Option<(u64, Result<EmulatorStateV1, StateError>)> {
        self.panes
            .get(&pane_id)
            .map(|runtime| (runtime.log.current_seq(), runtime.emulator.export_state()))
    }

    pub(crate) fn styled(&self, pane_id: u64, view_offset: Option<u32>) -> Option<PaneStyled> {
        self.panes
            .get(&pane_id)
            .map(|runtime| runtime.styled(pane_id, view_offset))
    }

    /// Drain all panes. Returns Hive death notices plus per-pane output ticks
    /// (revision or child-alive change this pass).
    pub(crate) fn drain(&mut self) -> (Vec<CellExitNotice>, Vec<OutputActivityTick>) {
        let mut notices = Vec::new();
        let mut activity = Vec::new();
        for (&pane_id, runtime) in self.panes.iter_mut() {
            let before_revision = runtime.revision;
            let before_alive = runtime.child_alive();
            let (notice, nbytes, attention) = runtime.drain(pane_id);
            if let Some(notice) = notice {
                notices.push(notice);
            }
            if runtime.revision != before_revision
                || runtime.child_alive() != before_alive
                || attention.is_some()
            {
                activity.push(OutputActivityTick {
                    pane_id,
                    revision: runtime.revision,
                    child_alive: runtime.child_alive(),
                    attention,
                    nbytes,
                });
            }
        }
        (notices, activity)
    }

    pub(crate) fn log_status(&mut self, pane_id: u64, text: Option<String>) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.record(PaneEvent::Status { text });
        }
    }

    pub(crate) fn log_mail_depth(&mut self, pane_id: u64, depth: u32) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.record(PaneEvent::MailDepth { depth });
        }
    }

    pub(crate) fn pane_log_watch(&self) -> std::sync::Arc<PaneLogWatch> {
        std::sync::Arc::clone(&self.watch)
    }

    pub(crate) fn subscribe_catch_up(&self, pane_id: u64, from_seq: u64) -> Option<CatchUp> {
        self.panes
            .get(&pane_id)
            .map(|pane| pane.log.catch_up(from_seq))
    }

    /// Stable persist fingerprint: current restore keys and pane log sequences.
    pub(crate) fn persist_mark(&self, keys: &[(u64, String, usize)]) -> u64 {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (id, session, pane_index) in keys {
            id.hash(&mut hasher);
            session.hash(&mut hasher);
            pane_index.hash(&mut hasher);
            self.panes
                .get(id)
                .map(|pane| pane.log.current_seq())
                .unwrap_or(0)
                .hash(&mut hasher);
        }
        hasher.finish()
    }

    pub(crate) fn capture_persist(
        &self,
        keys: &[(u64, String, usize)],
        previous: &CaptureMarks,
    ) -> (Vec<PendingPane>, CaptureMarks) {
        let mut captured = HashMap::new();
        let pending = keys
            .iter()
            .filter_map(|(id, session, pane_index)| {
                let pane = self.panes.get(id)?;
                let signature = (pane.log.current_seq(), session.clone(), *pane_index);
                let unchanged = previous.get(id) == Some(&signature);
                captured.insert(*id, signature);
                if unchanged {
                    return Some(PendingPane {
                        delta: Vec::new(),
                        id: *id,
                        record: None,
                        state: None,
                    });
                }
                if let Some((seq, prior_session, prior_index)) = previous.get(id) {
                    if prior_session == session && prior_index == pane_index {
                        if let CatchUp::Events(delta) = pane.log.catch_up(*seq) {
                            // Resize changes the snapshot's dimensions. Capture a new
                            // base for it; ordinary output needs only the event tail.
                            if !delta
                                .iter()
                                .any(|f| matches!(f.event, PaneEvent::Resize { .. }))
                            {
                                return Some(PendingPane {
                                    id: *id,
                                    record: None,
                                    state: None,
                                    delta,
                                });
                            }
                        }
                    }
                }
                let state = pane.emulator.export_state().ok();
                let snapshot_seq = if state.is_some() {
                    pane.log.current_seq()
                } else {
                    pane.log.oldest_seq().unwrap_or(1).saturating_sub(1)
                };
                Some(PendingPane {
                    delta: Vec::new(),
                    id: *id,
                    state,
                    record: Some(PersistPane {
                        session: session.clone(),
                        pane_index: *pane_index,
                        snapshot_seq,
                        snapshot: None,
                        cols: u16::try_from(pane.cols).unwrap_or(u16::MAX),
                        rows: u16::try_from(pane.rows).unwrap_or(u16::MAX),
                        cell_px: (pane.cell_w, pane.cell_h),
                        tail: pane.log.frames(),
                    }),
                })
            })
            .collect();
        (pending, captured)
    }

    pub(crate) fn export_persist(
        &self,
        keys: &[(u64, String, usize)],
        codec: &dyn ScreenCodec,
    ) -> Vec<PersistPane> {
        keys.iter()
            .filter_map(|(id, session, pane_index)| {
                let pane = self.panes.get(id)?;
                let snapshot = codec
                    .export(&pane.emulator)
                    .ok()
                    .filter(|bytes| !bytes.is_empty());
                let snapshot_seq = if snapshot.is_some() {
                    pane.log.current_seq()
                } else {
                    pane.log.oldest_seq().unwrap_or(1).saturating_sub(1)
                };
                Some(PersistPane {
                    session: session.clone(),
                    pane_index: *pane_index,
                    snapshot_seq,
                    snapshot,
                    cols: u16::try_from(pane.cols).unwrap_or(u16::MAX),
                    rows: u16::try_from(pane.rows).unwrap_or(u16::MAX),
                    cell_px: (pane.cell_w, pane.cell_h),
                    tail: pane.log.frames(),
                })
            })
            .collect()
    }

    pub(crate) fn restore_persist(
        &mut self,
        recs: &[PersistPane],
        keys: &[(u64, String, usize)],
        codec: &dyn ScreenCodec,
    ) {
        for rec in recs {
            let Some(&(id, _, _)) = keys
                .iter()
                .find(|(_, session, index)| session == &rec.session && *index == rec.pane_index)
            else {
                continue;
            };
            let Some(pane) = self.panes.get_mut(&id) else {
                continue;
            };
            let Ok(emulator) = restore_emulator(rec, codec) else {
                continue;
            };
            let cols = rec.cols.max(1) as usize;
            let rows = rec.rows.max(1) as usize;
            let cell_w = rec.cell_px.0.max(1);
            let cell_h = rec.cell_px.1.max(1);
            pane.emulator = emulator;
            pane.cols = cols;
            pane.outer_rows = rows;
            pane.rows = rows;
            pane.cell_w = cell_w;
            pane.cell_h = cell_h;
            pane.log = PaneLog::from_frames(rec.tail.clone(), DEFAULT_PANE_LOG_BYTES);
            // The screen belongs to the old child; the newly spawned child owns
            // input modes. Its output starts on a new line, not after the old
            // child's last prompt.
            let restart = [prismattyc_emulator::RESET_INPUT_REPORTING, b"\r\n"].concat();
            let _ = pane.emulator.feed(&restart);
            pane.record(PaneEvent::Output { bytes: restart });
            pane.revision = pane.revision.saturating_add(1);
            let size = pty_size_with_cell_pixels(cols, rows, cell_w, cell_h);
            let _ = pane.session.resize(size);
        }
    }

    pub(crate) fn reset_first_pane_log(&mut self, reason: &str) {
        let mut ids: Vec<u64> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        let Some(&id) = ids.first() else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        pane.log = PaneLog::new(DEFAULT_PANE_LOG_BYTES);
        pane.emulator = crate::pane_log_persist::fresh_emulator(pane.cols, pane.rows);
        pane.emulator.set_cell_pixels(pane.cell_w, pane.cell_h);
        let event = reset_output_event(reason);
        replay_event(&mut pane.emulator, &event);
        pane.record(event);
        pane.revision = pane.revision.saturating_add(1);
    }

    pub(crate) fn pane_gone(&self, pane_id: u64) -> bool {
        !self.panes.contains_key(&pane_id)
    }

    #[cfg(test)]
    pub(crate) fn shrink_pane_log(&mut self, pane_id: u64, cap: usize) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.log = PaneLog::new(cap);
        }
    }

    #[cfg(test)]
    pub(crate) fn pane_log(&self, pane_id: u64) -> Option<&PaneLog> {
        self.panes.get(&pane_id).map(|pane| &pane.log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn cursor_request_is_found_whole_and_split_across_chunks() {
        let mut request = CursorRequest::default();
        assert!(request.seen_in(b"\x1b[6n"));
        assert!(!request.seen_in(b"hello"));
        assert!(!request.seen_in(b"text\x1b["));
        assert!(request.seen_in(b"6n more"));
        assert!(!request.seen_in(b"\x1b[6"));
        assert!(!request.seen_in(b"m"));
    }

    #[test]
    fn cursor_request_is_counted_once() {
        let mut request = CursorRequest::default();
        assert!(request.seen_in(b"a\x1b[6n"));
        assert!(!request.seen_in(b""));
        assert!(!request.seen_in(b"b"));
    }

    #[cfg(windows)]
    fn windows_system(name: &str) -> String {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        PathBuf::from(root)
            .join("System32")
            .join(name)
            .display()
            .to_string()
    }

    /// `cmd /d /c` plus a quiet ping. PowerShell startup is slow enough that a
    /// full default-thread run leaves the pane blank for the whole wait.
    #[cfg(windows)]
    fn cmd_for(command: &str, env: BTreeMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            program: windows_system("cmd.exe"),
            argv: vec![
                "/d".into(),
                "/c".into(),
                format!("{command}& ping -n 31 127.0.0.1 >nul"),
            ],
            cwd: Some(std::env::temp_dir()),
            env,
        }
    }

    fn sleep_spawn(env: BTreeMap<String, String>) -> SpawnSpec {
        #[cfg(unix)]
        {
            SpawnSpec {
                program: "/bin/sleep".into(),
                argv: vec!["30".into()],
                cwd: Some(PathBuf::from("/tmp")),
                env,
            }
        }
        #[cfg(windows)]
        {
            // `rem` comments out the rest of the line, including the ping that
            // keeps the ConPTY child alive.
            cmd_for("cd .", env)
        }
    }

    fn exiting_spawn(env: BTreeMap<String, String>) -> SpawnSpec {
        #[cfg(unix)]
        {
            SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "printf EXIT_MARKER".into()],
                cwd: Some(PathBuf::from("/tmp")),
                env,
            }
        }
        #[cfg(windows)]
        {
            SpawnSpec {
                program: windows_system("cmd.exe"),
                argv: vec!["/d".into(), "/c".into(), "echo EXIT_MARKER".into()],
                cwd: Some(std::env::temp_dir()),
                env,
            }
        }
    }

    #[cfg(windows)]
    fn input_gated_exiting_spawn(env: BTreeMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            program: windows_system("cmd.exe"),
            argv: vec![
                "/d".into(),
                "/q".into(),
                "/c".into(),
                "set /p READY=& echo EXIT_MARKER".into(),
            ],
            cwd: Some(std::env::temp_dir()),
            env,
        }
    }

    fn spawn_test_pane(pane_id: u64, spawn: &SpawnSpec, mux_socket: Option<&Path>) -> LivePane {
        LivePane::spawn(
            pane_id,
            spawn,
            80,
            24,
            NOMINAL_CELL_W_PX,
            NOMINAL_CELL_H_PX,
            GuestStamp {
                mux_socket,
                agent_id: None,
            },
            PaneLogWatch::new(),
        )
        .expect("spawn")
    }

    fn spawn_seated_test_pane(
        pane_id: u64,
        spawn: &SpawnSpec,
        mux_socket: Option<&Path>,
        agent_id: Option<&str>,
    ) -> LivePane {
        LivePane::spawn(
            pane_id,
            spawn,
            80,
            24,
            NOMINAL_CELL_W_PX,
            NOMINAL_CELL_H_PX,
            GuestStamp {
                mux_socket,
                agent_id,
            },
            PaneLogWatch::new(),
        )
        .expect("spawn")
    }

    #[test]
    fn spawn_uses_nominal_cell_pixels_for_xtwinops() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        assert_eq!(pane.cell_w, NOMINAL_CELL_W_PX);
        assert_eq!(pane.cell_h, NOMINAL_CELL_H_PX);
        let _ = pane.emulator.feed(b"\x1b[16t\x1b[14t");
        let replies = pane.emulator.take_pending_replies();
        assert_eq!(
            replies,
            vec![
                format!("\x1b[6;{NOMINAL_CELL_H_PX};{NOMINAL_CELL_W_PX}t").into_bytes(),
                format!(
                    "\x1b[4;{};{}t",
                    24u32 * NOMINAL_CELL_H_PX,
                    80u32 * NOMINAL_CELL_W_PX
                )
                .into_bytes(),
            ]
        );
    }

    #[test]
    fn resize_applies_viewer_cell_pixels_to_xtwinops() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        pane.resize(80, 24, 10, 23).expect("resize");
        let _ = pane.emulator.feed(b"\x1b[16t");
        assert_eq!(
            pane.emulator.take_pending_replies(),
            vec![b"\x1b[6;23;10t".to_vec()]
        );
    }

    fn env_report_spawn(env: BTreeMap<String, String>) -> SpawnSpec {
        #[cfg(unix)]
        {
            SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec![
                    "-c".into(),
                    "printf 'PANE=%s\\nSOCK=%s\\nAGENT=%s\\nPACK=%s\\n' \"$PRISMATTYC_PANE_ID\" \"$PMUX_SOCKET\" \"$PMUX_AGENT\" \"$PMUX_TUTORIAL_PACK\"; if env | grep -q '^PRISMATTYC_SESSION_ID='; then printf 'HAS_SESSION=1\\n'; else printf 'HAS_SESSION=0\\n'; fi; if env | grep -q '^PMUX_AGENT='; then printf 'HAS_AGENT=1\\n'; else printf 'HAS_AGENT=0\\n'; fi; exec sleep 30".into(),
                ],
                cwd: Some(PathBuf::from("/tmp")),
                env,
            }
        }
        #[cfg(windows)]
        {
            cmd_for(
                "echo PANE=%PRISMATTYC_PANE_ID%& echo SOCK=%PMUX_SOCKET%& echo AGENT=%PMUX_AGENT%& echo PACK=%PMUX_TUTORIAL_PACK%& if defined PRISMATTYC_SESSION_ID (echo HAS_SESSION=1) else (echo HAS_SESSION=0)& if defined PMUX_AGENT (echo HAS_AGENT=1) else (echo HAS_AGENT=0)",
                env,
            )
        }
    }

    fn wait_for_pane_text(pane: &mut LivePane, pane_id: u64, needle: &str) -> String {
        // cmd prints immediately. The longer Windows budget covers a full
        // default-thread run, where many ConPTY guests start together.
        let attempts = if cfg!(windows) { 600 } else { 200 };
        for _ in 0..attempts {
            let _ = pane.drain(pane_id);
            let text = pane.content(pane_id).lines.join("\n");
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("never saw {needle:?} in {:?}", pane.content(pane_id).lines);
    }

    /// A shell script that signs an OSC 7577 report with `$OSC7577_TOKEN`
    /// (or `key` when set) using the system SHA-256 tool, as a program would.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn resume_report_script(argv: &[&str], cwd: &str, key: Option<&str>) -> String {
        let argv = argv
            .iter()
            .map(|arg| format!("$(printf '%s' '{arg}' | base64)"))
            .collect::<Vec<_>>()
            .join(",");
        let key = key.map_or("$OSC7577_TOKEN".to_string(), |key| key.to_string());
        format!(
            "if command -v sha256sum >/dev/null; then h=sha256sum; else h='shasum -a 256'; fi; \
             msg=\"pid=$$:time=$(date +%s):argv={argv}:cwd=$(printf '%s' '{cwd}' | base64)\"; \
             mac=$(printf '%s:%s' \"$msg\" \"{key}\" | $h | cut -c1-64); \
             printf '\\033]7577;%s:mac=%s\\007' \"$msg\" \"$mac\"; "
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_report_signed_with_the_pane_token_becomes_the_pane_record() {
        let script = format!(
            "{} {} printf 'SENT\\n'; exec sleep 30",
            resume_report_script(&["vim"], "/", Some("not-this-panes-token")),
            resume_report_script(&["nvim", "-S", "a b"], "/tmp", None),
        );
        let spawn = SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        };
        let mut pane = spawn_test_pane(3, &spawn, None);
        let text = wait_for_pane_text(&mut pane, 3, "SENT");
        assert!(!text.contains("7577"), "the report must not paint: {text}");
        let record = pane.resume.record().expect("the signed report is recorded");
        assert_eq!(record.argv, ["nvim", "-S", "a b"]);
        assert_eq!(record.cwd.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(record.owner.map(|owner| owner.pid), pane.child_pid);
        assert!(
            record.owner_exe.is_some(),
            "the owner's executable is recorded"
        );
    }

    /// A process in the pane holds the token, so it can sign a report naming
    /// any trusted program. The record is kept, but it is bound to what the
    /// owner really runs, so it is never resumed.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_shell_claiming_a_trusted_editor_is_recorded_but_not_resumed() {
        let script = format!(
            "{} printf 'SENT\\n'; exec sleep 30",
            resume_report_script(&["nvim", "-c", "!touch /tmp/owned"], "/tmp", None),
        );
        let spawn = SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        };
        let mut pane = spawn_test_pane(6, &spawn, None);
        wait_for_pane_text(&mut pane, 6, "SENT");
        let lookup = crate::program_resume::SystemLookup {
            root: pane.child_pid,
        };
        pane.resume
            .refresh(crate::program_resume::unix_now(), &lookup);
        let record = pane.resume.record().expect("the signed report is recorded");
        assert_eq!(record.argv[0], "nvim");
        // A real `nvim` on the resume PATH, so only the binding can refuse it.
        let bin = std::env::temp_dir().join(format!("resume-bin-{}", std::process::id()));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("nvim"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.join("nvim"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let path = Some(bin.clone().into_os_string());
        let decision = crate::program_resume::restore_decision(
            record,
            &crate::program_resume::ResumePolicy::default(),
            true,
            crate::program_resume::server_uid(),
            |program| program.canonicalize().ok(),
            |name| crate::program_resume::resolve_on_path(name, path.as_deref()),
            |_| true,
        );
        assert_eq!(decision, Err(crate::program_resume::UNBOUND));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn each_pane_gets_its_own_token_and_never_the_one_it_was_given() {
        let mut env = BTreeMap::new();
        env.insert(
            crate::program_resume::TOKEN_ENV.into(),
            "inherited-token-must-not-pass".into(),
        );
        let spawn = SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                "printf 'TOKEN=%s\\n' \"${OSC7577_TOKEN:-none}\"; exec sleep 30".into(),
            ],
            cwd: None,
            env,
        };
        let token = |pane_id| {
            let mut pane = spawn_test_pane(pane_id, &spawn, None);
            let text = wait_for_pane_text(&mut pane, pane_id, "TOKEN=");
            text.lines()
                .find_map(|line| line.trim().strip_prefix("TOKEN="))
                .unwrap()
                .to_string()
        };
        let (first, second) = (token(4), token(5));
        assert_ne!(first, "inherited-token-must-not-pass");
        assert_ne!(first, "none");
        assert_ne!(first, second);
        assert_eq!(first.len(), 43);
    }

    #[test]
    fn panes_that_reach_another_machine_get_no_token() {
        let policy = crate::program_resume::ResumePolicy::default();
        for program in ["ssh", "/usr/bin/ssh", "mosh", "rsh", "SSH.EXE"] {
            assert_eq!(resume_token_for(program, &policy), None, "{program}");
        }
        assert_eq!(
            resume_token_for("/bin/zsh", &policy).is_some(),
            policy.enabled()
        );
    }

    #[test]
    fn hive_cell_is_captured_from_spawn_env_at_spawn() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:1@1".into());
        let pane = spawn_test_pane(7, &sleep_spawn(env), None);
        assert_eq!(pane.hive_cell.as_deref(), Some("cell:1@1"));
        assert!(pane.child_pid.is_some(), "spawn should expose a child pid");
        assert!(pane.child_alive());
    }

    #[test]
    fn panes_without_hive_cell_are_not_seated() {
        let pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        assert_eq!(pane.hive_cell, None);
        // Observe death without a seat produces no notice.
        let mut pane = pane;
        // Force the death edge without waiting on a real PTY drain.
        let notice = pane.observe_death(1);
        assert!(
            notice.is_none(),
            "non-seated pane must not emit cell_exited"
        );
        assert!(!pane.child_alive());
    }

    #[test]
    fn seated_pane_emits_exactly_one_notice_on_death_edge() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:9@3".into());
        let mut pane = spawn_test_pane(9, &sleep_spawn(env), None);
        let first = pane.observe_death(9).expect("first death must emit");
        assert_eq!(first.cell, "cell:9@3");
        assert_eq!(first.pane_id, 9);
        assert_eq!(first.pid, pane.child_pid);
        // Second observation of the same edge is a no-op (already dead + notified).
        assert!(pane.observe_death(9).is_none());
    }

    #[test]
    fn exiting_child_preserves_output_and_records_one_exit_when_seated() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:1@1".into());
        let spawn = exiting_spawn(env);
        let mut runtime = LiveRuntime::spawn(
            &[PaneGeometry {
                pane_id: 1,
                col: 0,
                row: 0,
                cols: 80,
                rows: 24,
            }],
            &std::collections::HashMap::from([(1u64, spawn)]),
            None,
        )
        .expect("spawn");
        let mut notices = Vec::new();
        for _ in 0..200 {
            let (tick, _) = runtime.drain();
            notices.extend(tick);
            if !notices.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            notices.len(),
            1,
            "expected one cell exit notice, got {notices:?}"
        );
        assert_eq!(notices[0].cell, "cell:1@1");
        assert_eq!(notices[0].exit_code, Some(0));
        assert_eq!(notices[0].signal, None);

        let pane = runtime
            .panes
            .get(&1)
            .expect("exited pane remains available");
        assert!(!pane.child_alive());
        assert!(pane.content(1).lines[0].starts_with("EXIT_MARKER"));
        let events = runtime.pane_log(1).expect("pane log").events();
        let output_index = events
            .iter()
            .position(|event| {
                matches!(event, PaneEvent::Output { bytes } if bytes
                    .windows(b"EXIT_MARKER".len())
                    .any(|window| window == b"EXIT_MARKER"))
            })
            .expect("child output is recorded");
        let exit_indices = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| matches!(event, PaneEvent::Exited { .. }).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(exit_indices.len(), 1, "exit events: {events:?}");
        assert!(
            output_index < exit_indices[0],
            "output must precede exit: {events:?}"
        );

        let (later_notices, _) = runtime.drain();
        assert!(
            later_notices.is_empty(),
            "duplicate exit notice: {later_notices:?}"
        );
        assert_eq!(
            runtime
                .pane_log(1)
                .expect("pane log")
                .events()
                .iter()
                .filter(|event| matches!(event, PaneEvent::Exited { .. }))
                .count(),
            1,
            "a later drain must not append another exit event"
        );
    }

    #[cfg(windows)]
    #[test]
    fn exiting_child_waits_for_inflight_reader_publication() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:1@1".into());
        let mut runtime = LiveRuntime::spawn(
            &[PaneGeometry {
                pane_id: 1,
                col: 0,
                row: 0,
                cols: 80,
                rows: 24,
            }],
            &std::collections::HashMap::from([(1u64, input_gated_exiting_spawn(env))]),
            None,
        )
        .expect("spawn");
        runtime.set_reader_publish_delay_for_test(1, Duration::from_millis(150));
        runtime
            .write(1, b"ready\r".to_vec())
            .expect("release child");

        let mut notices = Vec::new();
        for _ in 0..400 {
            let (tick, _) = runtime.drain();
            notices.extend(tick);
            if !notices.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(notices.len(), 1, "exit notices: {notices:?}");
        assert_eq!(notices[0].exit_code, Some(0));
        let pane = runtime
            .panes
            .get(&1)
            .expect("exited pane remains available");
        assert!(
            pane.content(1)
                .lines
                .iter()
                .any(|line| line.contains("EXIT_MARKER")),
            "delayed child output was lost: {:?}",
            pane.content(1).lines
        );
        let events = runtime.pane_log(1).expect("pane log").events();
        let output_index = events
            .iter()
            .position(|event| {
                matches!(event, PaneEvent::Output { bytes } if bytes
                    .windows(b"EXIT_MARKER".len())
                    .any(|window| window == b"EXIT_MARKER"))
            })
            .expect("delayed child output is recorded");
        let exit_index = events
            .iter()
            .position(|event| matches!(event, PaneEvent::Exited { .. }))
            .expect("exit event is recorded");
        assert!(
            output_index < exit_index,
            "delayed output must precede exit: {events:?}"
        );
    }

    fn replay_log(pane: &LivePane) -> Emulator {
        let mut replica = Emulator::new(80, 24, MAX_SCROLLBACK);
        replica.set_retain_alt_history(true);
        replica.set_cell_pixels(pane.cell_w, pane.cell_h);
        for entry in pane.log.iter() {
            match &entry.event {
                PaneEvent::Output { bytes } => {
                    let _ = replica.feed(bytes);
                    let _ = replica.take_pending_replies();
                }
                PaneEvent::Resize {
                    cols,
                    rows,
                    cell_px,
                    size_owner: _,
                    reflow: _,
                } => {
                    replica.set_cell_pixels(cell_px.0, cell_px.1);
                    replica.resize(*cols as usize, *rows as usize);
                }
                _ => {}
            }
        }
        replica
    }

    #[test]
    fn pane_log_records_output_resize_and_replays_equal() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let _ = pane.apply_pty_bytes(b"hello");
        pane.resize_guest(100, 30).expect("resize");
        let _ = pane.apply_pty_bytes(b"\nworld");
        let kinds: Vec<&str> = pane
            .log
            .iter()
            .map(|e| match &e.event {
                PaneEvent::Output { .. } => "output",
                PaneEvent::Resize { .. } => "resize",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["output", "resize", "output"]);
        assert_eq!(pane.log.iter().next().map(|e| e.seq), Some(1));
        let mut replica = replay_log(&pane);
        assert_eq!(replica.screen(), pane.emulator.screen());
        assert_eq!(replica.take_pending_replies(), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn pane_log_pixel_resize_invalidates_checkpoint_cache() {
        let pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let mut runtime = LiveRuntime {
            panes: HashMap::from([(1, pane)]),
            mux_socket: None,
            watch: PaneLogWatch::new(),
            pending_size_owner: None,
        };
        let keys = [(1, "session".into(), 0)];
        let mark = runtime.persist_mark(&keys);
        let (_, captured) = runtime.capture_persist(&keys, &HashMap::new());
        let pane = runtime.panes.get_mut(&1).unwrap();
        pane.resize(80, 24, 13, 27).expect("pixel resize");
        assert!(matches!(
            &pane.log.iter().last().unwrap().event,
            PaneEvent::Resize {
                cols: 80,
                rows: 24,
                cell_px: (13, 27),
                reflow: true,
                ..
            }
        ));
        assert_ne!(runtime.persist_mark(&keys), mark);
        let (pending, captured) = runtime.capture_persist(&keys, &captured);
        assert_eq!(pending[0].record.as_ref().unwrap().cell_px, (13, 27));
        let state = pending[0].state.as_ref().unwrap();
        assert_eq!((state.cell_width_px, state.cell_height_px), (13, 27));
        let mark = runtime.persist_mark(&keys);
        runtime
            .panes
            .get_mut(&1)
            .unwrap()
            .resize_guest(80, 24)
            .unwrap();
        assert_eq!(runtime.persist_mark(&keys), mark);
        let (pending, _) = runtime.capture_persist(&keys, &captured);
        assert!(pending[0].record.is_none());
    }

    #[test]
    fn checkpoint_output_uses_deltas_and_log_gap_requires_fresh_snapshot() {
        let pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let mut runtime = LiveRuntime {
            panes: HashMap::from([(1, pane)]),
            mux_socket: None,
            watch: PaneLogWatch::new(),
            pending_size_owner: None,
        };
        let keys = [(1, "session".into(), 0)];
        let (initial, captured) = runtime.capture_persist(&keys, &HashMap::new());
        assert!(initial[0].state.is_some());
        runtime
            .panes
            .get_mut(&1)
            .unwrap()
            .apply_pty_bytes(b"new output");
        let (pending, _) = runtime.capture_persist(&keys, &captured);
        assert!(pending[0].record.is_none());
        assert!(pending[0].state.is_none());
        assert!(pending[0]
            .delta
            .iter()
            .any(|f| matches!(&f.event, PaneEvent::Output { bytes } if bytes == b"new output")));
        let renamed = [(1, "renamed".into(), 0)];
        assert!(runtime.capture_persist(&renamed, &captured).0[0]
            .record
            .is_some());
        let pane = runtime.panes.get_mut(&1).unwrap();
        // Shrink without resetting sequence numbers, then evict unpersisted events.
        pane.log = PaneLog::from_frames(pane.log.frames(), 16);
        pane.apply_pty_bytes(&[b'x'; 32]);
        pane.apply_pty_bytes(&[b'y'; 32]);
        let (pending, _) = runtime.capture_persist(&keys, &captured);
        assert!(pending[0].state.is_some());
        assert!(pending[0].delta.is_empty());
    }

    #[test]
    fn pane_log_pixel_resize_preserves_superseded_resize_reflow() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        pane.apply_pty_bytes(b"abcdefgh");
        let mut replica = replay_log(&pane);
        pane.resize(4, 24, 8, 16).expect("logical resize");
        pane.resize(4, 24, 13, 27).expect("pixel resize");
        crate::pane_log_persist::replay_event(&mut replica, &pane.log.iter().last().unwrap().event);
        assert_eq!(replica.screen().history_line_text(1), "efgh");
        assert_eq!(replica.screen(), pane.emulator.screen());
    }

    #[test]
    fn pane_log_records_title_cwd_attention_status_mail_and_exit() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let _ = pane
            .apply_pty_bytes(b"\x1b]0;win\x07\x1b]7;file:///tmp/work\x07\x1b]9;need review\x07hi");
        pane.log.append(PaneEvent::Status {
            text: Some("busy".into()),
        });
        pane.log.append(PaneEvent::MailDepth { depth: 2 });
        let _ = pane.observe_death(1);
        let mut saw_title = false;
        let mut saw_cwd = false;
        let mut saw_attention = false;
        let mut saw_status = false;
        let mut saw_mail = false;
        let mut saw_exit = false;
        let mut seqs = Vec::new();
        for entry in pane.log.iter() {
            seqs.push(entry.seq);
            match &entry.event {
                PaneEvent::Title { text } => {
                    assert_eq!(text, "win");
                    saw_title = true;
                }
                PaneEvent::Cwd { path } => {
                    assert_eq!(path, &PathBuf::from("/tmp/work"));
                    saw_cwd = true;
                }
                PaneEvent::Attention { text } => {
                    assert_eq!(text, "need review");
                    saw_attention = true;
                }
                PaneEvent::Status { text } => {
                    assert_eq!(text.as_deref(), Some("busy"));
                    saw_status = true;
                }
                PaneEvent::MailDepth { depth } => {
                    assert_eq!(*depth, 2);
                    saw_mail = true;
                }
                PaneEvent::Exited { .. } => saw_exit = true,
                PaneEvent::Output { .. } => {}
                PaneEvent::Resize { .. } => panic!("no resize in this test"),
                PaneEvent::SizeOwnerChanged { .. } => {}
            }
        }
        assert!(saw_title && saw_cwd && saw_attention && saw_status && saw_mail && saw_exit);
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
        let _ = pane.observe_death(1);
        let exit_count = pane
            .log
            .iter()
            .filter(|e| matches!(e.event, PaneEvent::Exited { .. }))
            .count();
        assert_eq!(exit_count, 1);
    }

    #[test]
    fn live_runtime_status_and_mail_depth_hit_the_pane_log() {
        let mut runtime = LiveRuntime::spawn(
            &[PaneGeometry {
                pane_id: 4,
                col: 0,
                row: 0,
                cols: 80,
                rows: 24,
            }],
            &std::collections::HashMap::from([(4u64, sleep_spawn(BTreeMap::new()))]),
            None,
        )
        .expect("spawn");
        runtime.log_status(4, Some("ok".into()));
        runtime.log_mail_depth(4, 3);
        let events = runtime.pane_log(4).expect("pane").events();
        assert!(events
            .iter()
            .any(|e| matches!(e, PaneEvent::Status { text } if text.as_deref() == Some("ok"))));
        assert!(events
            .iter()
            .any(|e| matches!(e, PaneEvent::MailDepth { depth: 3 })));
    }

    const REPORTING_APP: &[u8] = b"saved output\r\n\x1b[?1000;1002;1003;1006;1004;2004;7700h";

    fn assert_no_input_reporting(emulator: &Emulator) {
        assert_eq!(
            emulator.mouse_tracking(),
            prismattyc_emulator::MouseTracking::Off
        );
        assert!(!emulator.mouse_sgr());
        assert!(!emulator.mouse_wheel_only());
        assert!(!emulator.focus_report());
        assert!(!emulator.bracketed_paste());
    }

    #[test]
    fn restored_child_does_not_inherit_previous_input_reporting() {
        use crate::pane_log_persist::EmulatorStateCodec;
        for snapshot in [true, false] {
            let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
            pane.apply_pty_chunk(PtyChunk {
                bytes: REPORTING_APP.to_vec(),
                _reservation: None,
            });
            let mut runtime = LiveRuntime {
                panes: HashMap::from([(1, pane)]),
                mux_socket: None,
                watch: PaneLogWatch::new(),
                pending_size_owner: None,
            };
            let keys = [(1, "work".into(), 0)];
            let mut saved = runtime.export_persist(&keys, &EmulatorStateCodec);
            assert_eq!(saved.len(), 1);
            assert_eq!(
                runtime.panes[&1].emulator.mouse_tracking(),
                prismattyc_emulator::MouseTracking::Any
            );
            if !snapshot {
                saved[0].snapshot = None;
                saved[0].snapshot_seq = 0;
            }
            runtime
                .panes
                .insert(1, spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None));
            runtime.restore_persist(&saved, &keys, &EmulatorStateCodec);
            let pane = runtime.panes.get_mut(&1).unwrap();
            assert!(pane.content(1).lines[0].starts_with("saved output"));
            assert_no_input_reporting(&pane.emulator);
            assert_eq!(pane.styled(1, None).child_mouse_tracking, Some(false));
            assert_no_input_reporting(&replay_log(pane));
            // A replacement app may opt in again; ordinary reads must keep it enabled.
            pane.apply_pty_chunk(PtyChunk {
                bytes: REPORTING_APP.to_vec(),
                _reservation: None,
            });
            assert_eq!(pane.styled(1, None).child_mouse_tracking, Some(true));
            assert!(pane.emulator.focus_report());
            assert!(pane.emulator.bracketed_paste());
        }
    }

    #[test]
    fn a_restored_pane_starts_new_output_on_its_own_line() {
        use crate::pane_log_persist::EmulatorStateCodec;
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        // The old child left its cursor after a prompt, mid-line.
        pane.apply_pty_chunk(PtyChunk {
            bytes: b"old prompt> ".to_vec(),
            _reservation: None,
        });
        let mut runtime = LiveRuntime {
            panes: HashMap::from([(1, pane)]),
            mux_socket: None,
            watch: PaneLogWatch::new(),
            pending_size_owner: None,
        };
        let keys = [(1, "work".into(), 0)];
        let saved = runtime.export_persist(&keys, &EmulatorStateCodec);
        runtime
            .panes
            .insert(1, spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None));
        runtime.restore_persist(&saved, &keys, &EmulatorStateCodec);
        let pane = runtime.panes.get_mut(&1).unwrap();
        pane.apply_pty_chunk(PtyChunk {
            bytes: b"new child".to_vec(),
            _reservation: None,
        });
        let lines = pane.content(1).lines;
        assert_eq!(lines[0].trim_end(), "old prompt>", "{lines:?}");
        assert!(lines[1].starts_with("new child"), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_stops_input_reporting_and_keeps_output() {
        let spawn = SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                r"printf '\033[?1000;1002;1003;1006;1004;2004;7700hEXIT_MARKER'".into(),
            ],
            cwd: None,
            env: BTreeMap::new(),
        };
        let mut pane = spawn_test_pane(1, &spawn, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pane.child_alive() && std::time::Instant::now() < deadline {
            pane.drain(1);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!pane.child_alive(), "child did not exit");
        assert!(pane.content(1).lines[0].starts_with("EXIT_MARKER"));
        assert_no_input_reporting(&pane.emulator);
        assert_no_input_reporting(&replay_log(&pane));
    }

    #[test]
    fn styled_reports_child_mouse_modes() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let off = pane.styled(1, None);
        assert_eq!(off.child_mouse_tracking, Some(false));
        assert_eq!(off.child_mouse_sgr, Some(false));
        pane.emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let on = pane.styled(1, None);
        assert_eq!(on.child_mouse_tracking, Some(true));
        assert_eq!(on.child_mouse_sgr, Some(true));
    }

    #[test]
    fn image_survives_reattach() {
        // Image state lives on the server pane's Emulator, not on any viewer.
        // A fresh viewer attach (a new `styled()` projection) must still see it.
        const B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let seq = format!("\x1b_Ga=T,t=d,f=100,i=42;{B64}\x1b\\");
        pane.emulator.feed(seq.as_bytes());
        assert_eq!(pane.emulator.images().len(), 1);
        // Simulate a new viewer attaching: styled() is the per-viewer read path.
        let _ = pane.styled(1, None);
        // Image state persists on the server pane across a fresh viewer projection.
        assert_eq!(pane.emulator.images().len(), 1, "image survives reattach");
        assert_eq!(pane.emulator.images()[0].id, 42);
    }

    #[test]
    fn stamp_overwrites_caller_keys_and_clears_unknowns() {
        let mut env = BTreeMap::new();
        env.insert(PRISMATTYC_PANE_ID.into(), "999".into());
        env.insert(PMUX_SOCKET.into(), "/tmp/lie.sock".into());
        env.insert(PRISMATTYC_SESSION_ID.into(), "42".into());
        env.insert(PMUX_AGENT.into(), "mallory".into());
        env.insert(PMUX_TUTORIAL_PACK.into(), "bogus-pack".into());
        stamp_prism_guest_env(&mut env, 7, None, None);
        assert_eq!(env.get(PRISMATTYC_PANE_ID).map(String::as_str), Some("7"));
        assert!(!env.contains_key(PMUX_SOCKET));
        assert!(!env.contains_key(PRISMATTYC_SESSION_ID));
        assert!(!env.contains_key(PMUX_AGENT));
        assert_eq!(
            env.get(PMUX_TUTORIAL_PACK).map(String::as_str),
            Some(TUTORIAL_PACK_MANIFEST)
        );
        stamp_prism_guest_env(
            &mut env,
            7,
            Some(Path::new("/run/user/1000/prismattyc/pmux.sock")),
            Some("operator-a"),
        );
        assert_eq!(
            env.get(PMUX_SOCKET).map(String::as_str),
            Some("/run/user/1000/prismattyc/pmux.sock")
        );
        assert!(!env.contains_key(PRISMATTYC_SESSION_ID));
        assert_eq!(env.get(PMUX_AGENT).map(String::as_str), Some("operator-a"));
    }

    #[test]
    fn guest_environ_sees_stamped_prism_keys() {
        let mut lie = BTreeMap::new();
        lie.insert(PRISMATTYC_PANE_ID.into(), "0".into());
        lie.insert(PMUX_SOCKET.into(), "/tmp/wrong.sock".into());
        lie.insert(PRISMATTYC_SESSION_ID.into(), "0".into());
        let mut pane = spawn_test_pane(
            11,
            &env_report_spawn(lie),
            Some(Path::new("/tmp/prism-pm122.sock")),
        );
        let text = wait_for_pane_text(&mut pane, 11, "HAS_SESSION=0");
        assert!(text.contains("PANE=11"), "{text:?}");
        assert!(text.contains("SOCK=/tmp/prism-pm122.sock"), "{text:?}");
        assert!(text.contains("HAS_SESSION=0"), "{text:?}");
        assert!(text.contains("PACK=prismattyc-tutorial-pack"), "{text:?}");
    }

    #[test]
    fn guest_environ_sees_stamped_agent_id() {
        let mut lie = BTreeMap::new();
        lie.insert(PMUX_AGENT.into(), "mallory".into());
        let mut pane = spawn_seated_test_pane(
            12,
            &env_report_spawn(lie),
            Some(Path::new("/tmp/prism-pm122.sock")),
            Some("operator-a"),
        );
        let text = wait_for_pane_text(&mut pane, 12, "HAS_AGENT=1");
        assert!(text.contains("AGENT=operator-a"), "{text:?}");
    }

    #[test]
    fn guest_environ_drops_agent_id_when_session_unbound() {
        let mut lie = BTreeMap::new();
        lie.insert(PMUX_AGENT.into(), "mallory".into());
        let mut pane = spawn_test_pane(13, &env_report_spawn(lie), None);
        let text = wait_for_pane_text(&mut pane, 13, "HAS_AGENT=0");
        assert!(text.contains("HAS_AGENT=0"), "{text:?}");
    }
}

#[cfg(all(test, unix))]
mod handoff_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sh(script: &str) -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script.into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        }
    }

    fn one_pane(script: &str) -> LiveRuntime {
        LiveRuntime::spawn(
            &[PaneGeometry {
                pane_id: 1,
                col: 0,
                row: 0,
                cols: 60,
                rows: 10,
            }],
            &HashMap::from([(1u64, sh(script))]),
            None,
        )
        .expect("spawn")
    }

    fn text(runtime: &LiveRuntime) -> String {
        runtime.content(1).expect("pane").lines.join("\n")
    }

    /// Block until pane `id`'s reader hands over its next chunk, and apply it.
    /// False when nothing arrives before `deadline` or the reader is gone.
    fn await_output(runtime: &mut LiveRuntime, id: u64, deadline: Instant) -> bool {
        let pane = runtime.panes.get_mut(&id).expect("pane");
        let left = deadline.saturating_duration_since(Instant::now());
        match pane.from_pty_rx.recv_timeout(left) {
            Ok(Ok(chunk)) if chunk.bytes.is_empty() => {
                let _ = pane.observe_death(id);
                true
            }
            Ok(Ok(chunk)) => {
                let _ = pane.apply_pty_chunk(chunk);
                true
            }
            Ok(Err(_)) => {
                let _ = pane.observe_death(id);
                true
            }
            Err(_) => false,
        }
    }

    fn wait_for(runtime: &mut LiveRuntime, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let _ = runtime.drain();
            let text = text(runtime);
            if text.contains(needle) {
                return text;
            }
            assert!(
                await_output(runtime, 1, deadline),
                "never saw {needle:?} in {text:?}"
            );
        }
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn hand_over(mut old: LiveRuntime) -> LiveRuntime {
        old.quiesce_for_handoff(Duration::from_secs(5))
            .expect("quiesce");
        let panes = old.export_handoff().expect("export");
        drop(old);
        // The record survives a round trip through its wire form.
        let panes = panes
            .into_iter()
            .map(|(record, fd)| {
                let json = serde_json::to_vec(&record).expect("encode");
                (serde_json::from_slice(&json).expect("decode"), fd)
            })
            .collect();
        let (runtime, failed) = LiveRuntime::adopt_handoff(panes, None, PaneLogWatch::new());
        assert!(failed.is_empty(), "panes failed to adopt: {failed:?}");
        runtime
    }

    /// The program keeps running with the same pid, its screen comes along,
    /// and input typed after the handoff reaches it.
    #[test]
    fn a_running_program_survives_a_handoff_with_its_screen_and_input() {
        let mut old = one_pane("echo start; while read line; do echo got:$line; done");
        wait_for(&mut old, "start");
        old.write(1, b"one\n".to_vec()).expect("write");
        wait_for(&mut old, "got:one");
        let pid = old.child_pid(1).expect("pid");
        let revision = old.panes[&1].revision;

        let mut new = hand_over(old);
        assert!(alive(pid), "the program ended in the handoff");
        assert_eq!(new.child_pid(1), Some(pid));
        assert!(new.panes[&1].revision > revision);
        assert!(
            text(&new).contains("got:one"),
            "screen lost: {:?}",
            text(&new)
        );
        new.write(1, b"two\n".to_vec()).expect("write");
        wait_for(&mut new, "got:two");

        // Closing the adopted pane ends and reaps its program before it returns.
        drop(new);
        assert!(
            !alive(pid),
            "closing the adopted pane did not end its program"
        );
    }

    /// Output the program writes while no server is reading waits in the PTY
    /// and shows up after the handoff.
    #[test]
    fn output_written_during_the_handoff_is_not_lost() {
        let mut old = one_pane("echo ready; read x; sleep 0.3; echo written-in-the-gap; read y");
        wait_for(&mut old, "ready");
        old.write(1, b"go\n".to_vec()).expect("write");
        old.quiesce_for_handoff(Duration::from_secs(5))
            .expect("quiesce");
        let panes = old.export_handoff().expect("export");
        drop(old);
        // Wait, with no server reading, until the program's output is in the PTY.
        {
            use std::os::fd::AsRawFd;
            let mut ready = libc::pollfd {
                fd: panes[0].1.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd entry.
            assert_eq!(unsafe { libc::poll(&mut ready, 1, 10_000) }, 1, "no output");
        }
        let (mut new, failed) = LiveRuntime::adopt_handoff(panes, None, PaneLogWatch::new());
        assert!(failed.is_empty());
        wait_for(&mut new, "written-in-the-gap");
    }

    /// A pane whose program already ended keeps its last screen and stays
    /// ended.
    #[test]
    fn an_exited_pane_keeps_its_screen_and_stays_ended() {
        let mut old = one_pane("echo last-words");
        wait_for(&mut old, "last-words");
        let deadline = Instant::now() + Duration::from_secs(5);
        while old.panes[&1].child_alive() {
            assert!(
                await_output(&mut old, 1, deadline),
                "the program never ended"
            );
        }
        let new = hand_over(old);
        assert!(!new.panes[&1].child_alive());
        assert!(text(&new).contains("last-words"));
    }

    /// One pane that cannot be adopted is reported and closed; the others
    /// are taken over.
    #[test]
    fn a_pane_that_cannot_be_adopted_is_reported_and_the_rest_carry_over() {
        let mut old = LiveRuntime::spawn(
            &[
                PaneGeometry {
                    pane_id: 1,
                    col: 0,
                    row: 0,
                    cols: 60,
                    rows: 10,
                },
                PaneGeometry {
                    pane_id: 2,
                    col: 0,
                    row: 10,
                    cols: 60,
                    rows: 10,
                },
            ],
            &HashMap::from([
                (1u64, sh("echo one; exec sleep 30")),
                (2u64, sh("echo two; exec sleep 30")),
            ]),
            None,
        )
        .expect("spawn");
        wait_for(&mut old, "one");
        let second = old.child_pid(2).expect("pid");
        old.quiesce_for_handoff(Duration::from_secs(5))
            .expect("quiesce");
        let mut panes = old.export_handoff().expect("export");
        drop(old);
        // Damage pane 2's record: no program pid.
        for (record, _) in &mut panes {
            if record.pane_id == 2 {
                record.pid = None;
            }
        }
        let (new, failed) = LiveRuntime::adopt_handoff(panes, None, PaneLogWatch::new());
        assert_eq!(failed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [2]);
        assert!(new.has_pane(1) && !new.has_pane(2));
        assert!(text(&new).contains("one"));
        // Its master closed, so the kernel hangs the program up; wait for it.
        let mut status = 0;
        // SAFETY: reap the hung-up program, which is a child of this test.
        unsafe { libc::waitpid(second as libc::pid_t, &mut status, 0) };
        assert!(
            !alive(second),
            "the program of a pane that was not adopted kept running"
        );
    }

    /// A writer stuck on a program that never reads makes the handoff give up
    /// in time, and the server carries on reading the pane.
    #[test]
    fn a_stuck_writer_abandons_the_handoff_and_reading_resumes() {
        let mut old = one_pane("stty -icanon; echo ready; sleep 1; echo after-abort; sleep 30");
        wait_for(&mut old, "ready");
        // Fill the PTY input queue: the program never reads it.
        for _ in 0..TO_CHILD_CAP {
            let _ = old.write(1, vec![b'x'; 4096]);
        }
        let error = old
            .quiesce_for_handoff(Duration::from_millis(300))
            .expect_err("a stuck writer must fail the quiesce");
        assert!(error.to_string().contains("did not stop"), "{error}");
        old.resume_after_handoff().expect("resume");
        wait_for(&mut old, "after-abort");
    }
}
