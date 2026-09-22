//! Server-owned PTY/emulator runtimes for Phase 2B.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use prismattyc_emulator::{
    pty_size_with_cell_pixels, Emulator, PtySession, NOMINAL_CELL_H_PX, NOMINAL_CELL_W_PX,
};
use prismattyc_protocol::{InputModifiers, PointerPhase, ViewerId};

use crate::control::{
    rle_style_runs, ColorWire, CursorShapeWire, PaneContent, PaneGeometry, PaneStyled, SpawnSpec,
    WorkspaceStyleRun,
};
use crate::pane_log::{
    ByteBudget, ByteReservation, CatchUp, PaneEvent, PaneLog, PaneLogWatch, DEFAULT_PANE_LOG_BYTES,
    PANE_FRAME_OVERHEAD, PTY_OUTPUT_BUDGET_BYTES,
};
use crate::pane_log_persist::{
    replay_event, reset_output_event, restore_emulator, CaptureMarks, PendingPane, PersistPane,
    ScreenCodec,
};
use crate::rich::{self, RichSession};

const FROM_PTY_CAP: usize = 64;
const TO_CHILD_CAP: usize = 32;
const MAX_DRAIN_PER_PANE: usize = 16;
const MAX_SCROLLBACK: usize = 10_000;

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

struct LivePane {
    emulator: Emulator,
    session: PtySession,
    from_pty_rx: mpsc::Receiver<std::io::Result<PtyChunk>>,
    to_child_tx: mpsc::SyncSender<Vec<u8>>,
    rich: Option<RichSession>,
    cols: usize,
    outer_rows: usize,
    rows: usize,
    /// Cell pixels for TIOCGWINSZ / XTWINOPS. Nominal until a viewer reports
    /// its font (mux-server has none of its own).
    cell_w: u32,
    cell_h: u32,
    revision: u64,
    child_alive: bool,
    /// `HIVE_CELL` from `SpawnSpec.env` at spawn, if present. Absent ⇒ not a Hive seat.
    hive_cell: Option<String>,
    /// PID captured at spawn (and kept for the exit notice). Diagnostic only.
    child_pid: Option<u32>,
    /// True once a [`CellExitNotice`] has been produced for this pane's death edge.
    exit_notified: bool,
    controller_id: Option<u64>,
    /// Ordered PTY/side-channel log. Seq is a real index, not `revision`.
    log: PaneLog,
    /// Last OSC 7 path already written to `log`.
    logged_cwd: Option<PathBuf>,
    watch: std::sync::Arc<PaneLogWatch>,
    /// Copied onto the next logged Resize (PT-202).
    size_owner: Option<crate::remote_size::SizeOwner>,
}

/// Discovery values stamped into the pane child environment.
#[derive(Clone, Copy, Default)]
pub(crate) struct GuestStamp<'a> {
    pub mux_socket: Option<&'a Path>,
    pub agent_id: Option<&'a str>,
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
        let mut session = PtySession::spawn_config(
            &spawn.program,
            &spawn.argv,
            spawn.cwd.as_deref(),
            &env,
            size,
        )
        .with_context(|| format!("spawn {:?} for pane {pane_id}", spawn.program))?;
        // Capture seat identity at spawn from the inject map — never re-read after death
        // (ADR-0037 §3 / wire contract B). Empty string is treated as absent (not a seat).
        let hive_cell = spawn
            .env
            .get(HIVE_CELL_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let child_pid = session.process_id();
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

        let (from_pty_tx, from_pty_rx) =
            mpsc::sync_channel::<std::io::Result<PtyChunk>>(FROM_PTY_CAP);
        let output_budget = ByteBudget::new(PTY_OUTPUT_BUDGET_BYTES);
        let reader_budget = output_budget.clone();
        thread::Builder::new()
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

        // server panes retain alt-screen history so attach scroll
        // mode can page through TUI output. Classic hosts never opt in.
        let experimental_rich = rich::env_flag_enabled("PRISMATTYC_EXPERIMENTAL_RICH");
        let mut emulator = if experimental_rich {
            Emulator::new_experimental(cols, rows, MAX_SCROLLBACK)
        } else {
            Emulator::new(cols, rows, MAX_SCROLLBACK)
        };
        emulator.set_retain_alt_history(true);
        emulator.set_cell_pixels(cell_w, cell_h);
        Ok(Self {
            emulator,
            session,
            from_pty_rx,
            to_child_tx,
            rich: experimental_rich.then(RichSession::default),
            cols,
            outer_rows: rows,
            rows,
            cell_w,
            cell_h,
            revision: 0,
            child_alive: true,
            hive_cell,
            child_pid,
            exit_notified: false,
            controller_id: None,
            log: PaneLog::new(DEFAULT_PANE_LOG_BYTES),
            logged_cwd: None,
            watch,
            size_owner: None,
        })
    }

    fn record(&mut self, event: PaneEvent) {
        self.log.append(event);
        self.watch.note();
    }

    /// Flip `child_alive` true→false and, if this pane is a Hive seat, produce at most
    /// one [`CellExitNotice`]. Subsequent calls are no-ops (idempotent on the edge).
    fn observe_death(&mut self, pane_id: u64) -> Option<CellExitNotice> {
        if !self.child_alive {
            return None;
        }
        self.child_alive = false;
        self.revision = self.revision.saturating_add(1);
        // Non-blocking: never wait on the child here — drain must not stall the mux loop.
        let (exit_code, signal) = match self.session.try_wait() {
            Ok(Some(status)) => (Some(status.exit_code()), status.signal().map(str::to_owned)),
            _ => (None, None),
        };
        self.record(PaneEvent::Exited {
            code: exit_code,
            signal: signal.clone(),
        });
        if self.exit_notified {
            return None;
        }
        let cell = self.hive_cell.clone()?;
        self.exit_notified = true;
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
        self.reconcile_workspace_geometry()?;
        Ok(())
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

    fn reconcile_workspace_geometry(&mut self) -> Result<()> {
        let workspace_rows = match self.rich.as_ref().map(|rich| {
            rich.workspace_layout(
                u16::try_from(self.outer_rows).unwrap_or(u16::MAX),
                u16::try_from(self.cols).unwrap_or(u16::MAX),
            )
        }) {
            Some(Ok(layout)) => layout.map_or(0, |layout| usize::from(layout.rows)),
            Some(Err(_)) => {
                if let Some(rich) = self.rich.as_mut() {
                    rich.drop_workspace();
                }
                0
            }
            None => 0,
        };
        self.resize_guest(
            self.cols,
            self.outer_rows.saturating_sub(workspace_rows).max(1),
        )
    }

    fn workspace_projection(
        &self,
    ) -> (
        Vec<String>,
        Vec<crate::control::WorkspaceInverseRun>,
        Vec<WorkspaceStyleRun>,
    ) {
        let Some(layout) = self.workspace_layout_snapshot() else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let inverse = layout
            .selected_runs
            .iter()
            .map(|run| crate::control::WorkspaceInverseRun {
                row: u32::from(run.row),
                col: u32::from(run.col),
                cols: u32::from(run.cols),
            })
            .collect();
        let styles = layout
            .status_runs
            .iter()
            .map(|run| WorkspaceStyleRun {
                row: u32::from(run.row),
                col: u32::from(run.col),
                cols: u32::from(run.cols),
                fg: run
                    .tone
                    .ansi_index()
                    .map_or(ColorWire::Default, |n| ColorWire::Ansi { n }),
            })
            .collect();
        (layout.lines, inverse, styles)
    }

    fn workspace_layout_snapshot(&self) -> Option<prismattyc_render::WorkspaceLayout> {
        self.rich.as_ref().and_then(|rich| {
            rich.workspace_layout(
                u16::try_from(self.outer_rows).unwrap_or(u16::MAX),
                u16::try_from(self.cols).unwrap_or(u16::MAX),
            )
            .ok()
            .flatten()
        })
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
        if let Some(rich) = self.rich.as_mut() {
            rich::process_rich_chunk(&mut self.emulator, rich, &self.to_child_tx, &bytes);
            if self.reconcile_workspace_geometry().is_err() {
                if let Some(rich) = self.rich.as_mut() {
                    rich.drop_workspace();
                }
                let _ = self.resize_guest(self.cols, self.outer_rows.max(1));
            }
        } else {
            let _ = self.emulator.feed(&bytes);
            for reply in self.emulator.take_pending_replies() {
                let _ = self.to_child_tx.try_send(reply);
            }
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
        if !self.child_alive {
            return (None, 0, None);
        }
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
        let notice = if died {
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
            child_alive: self.child_alive,
            child_pid: self.session.process_id(),
            lines,
            cursor_shape: Some(CursorShapeWire::from_emulator(self.emulator.cursor_shape())),
        }
    }

    fn styled(
        &self,
        pane_id: u64,
        view_offset: Option<u32>,
        viewer_id: Option<ViewerId>,
    ) -> PaneStyled {
        let screen = self.emulator.screen();
        // history depth is mode-agnostic so alt-screen TUI output
        // stays scrollable through attach; classic max_view_scroll would
        // report 0 on the alt screen.
        let max = u32::try_from(screen.history_len()).unwrap_or(u32::MAX);
        let offset = view_offset.unwrap_or(0).min(max);
        let child_mouse_tracking = Some(self.emulator.mouse_tracking().is_on());
        let child_mouse_sgr = Some(self.emulator.mouse_sgr());
        let structured_focus_id = viewer_id.and_then(|viewer_id| {
            self.rich
                .as_ref()
                .and_then(|rich| rich.structured_focus_node(viewer_id))
        });
        let structured_focus = structured_focus_id.is_some();
        let rich_focus_id = structured_focus_id.or_else(|| {
            viewer_id.and_then(|viewer_id| {
                self.rich
                    .as_ref()
                    .and_then(|rich| rich.focus_id_for(viewer_id))
            })
        });
        // Styled reads are polled by attach while idle. Project the workspace
        // once, then derive text, selection, and status runs from that result.
        let (workspace, workspace_inverse, workspace_styles) = self.workspace_projection();
        if offset == 0 {
            return PaneStyled {
                content: self.content(pane_id),
                runs: (0..screen.rows())
                    .map(|row| screen.view_row(row).map(rle_style_runs).unwrap_or_default())
                    .collect(),
                workspace,
                view_offset: Some(0),
                max_view_scroll: Some(max),
                child_mouse_tracking,
                child_mouse_sgr,
                overlays: self
                    .rich
                    .as_ref()
                    .map(|rich| rich::snapshot_overlays(&self.emulator, rich, 0))
                    .unwrap_or_default(),
                experimental_rich: self.rich.is_some(),
                rich_focus_id,
                structured_focus,
                semantic_clipboard: None,
                semantic_clipboard_seq: None,
                workspace_inverse,
                workspace_styles,
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
                child_alive: self.child_alive,
                child_pid: self.session.process_id(),
                lines,
                cursor_shape: Some(CursorShapeWire::from_emulator(self.emulator.cursor_shape())),
            },
            runs,
            workspace,
            view_offset: Some(offset),
            max_view_scroll: Some(max),
            child_mouse_tracking,
            child_mouse_sgr,
            overlays: self
                .rich
                .as_ref()
                .map(|rich| rich::snapshot_overlays(&self.emulator, rich, offset))
                .unwrap_or_default(),
            experimental_rich: self.rich.is_some(),
            rich_focus_id,
            structured_focus,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
            workspace_inverse,
            workspace_styles,
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

    pub(crate) fn remove_pane(&mut self, pane_id: u64) {
        if self.panes.remove(&pane_id).is_some() {
            self.watch.note();
        }
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

    pub(crate) fn child_pid(&self, pane_id: u64) -> Option<u32> {
        self.panes
            .get(&pane_id)
            .filter(|pane| pane.child_alive)
            .and_then(|pane| pane.child_pid)
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

    pub(crate) fn queue_copy_request(&mut self, pane_id: u64, initiator: u64) {
        if let Some(rich) = self
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.rich.as_mut())
        {
            rich.queue_copy_request(initiator);
        }
    }

    /// Grant or revoke `input.rich_focus`. Flag-off is `(false, None)`.
    pub(crate) fn rich_focus(
        &mut self,
        pane_id: u64,
        viewer_id: ViewerId,
        revoke: bool,
    ) -> Result<(bool, Option<u32>, bool), LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        let Some(rich) = runtime.rich.as_mut() else {
            return Ok((false, None, false));
        };
        let (bytes, structured) = if revoke {
            if rich.structured_focus_node(viewer_id).is_some() {
                (rich.revoke_structured_focus(viewer_id), true)
            } else {
                (rich.revoke_focus(viewer_id), false)
            }
        } else {
            let structured_bytes = rich.toggle_structured_focus(viewer_id);
            if structured_bytes.is_some() {
                (structured_bytes, true)
            } else if rich.has_any_structured_focus() {
                (None, true)
            } else {
                (rich.toggle_focus(viewer_id), false)
            }
        };
        if let Some(bytes) = bytes {
            match runtime.to_child_tx.try_send(bytes) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => return Err(LiveWriteError::Backpressure),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(LiveWriteError::Disconnected)
                }
            }
        }
        let region_id = if structured {
            rich.structured_focus_node(viewer_id)
        } else {
            rich.focus_id_for(viewer_id)
        };
        Ok((region_id.is_some(), region_id, structured))
    }

    pub(crate) fn rich_key(
        &mut self,
        pane_id: u64,
        viewer_id: ViewerId,
        key: String,
        modifiers: InputModifiers,
    ) -> Result<Option<bool>, LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        let Some(rich) = runtime.rich.as_mut() else {
            return Ok(None);
        };
        if rich.structured_focus_node(viewer_id).is_none() {
            return Ok(None);
        }
        let Some(bytes) = rich.encode_structured_key(viewer_id, key, modifiers) else {
            return Ok(None);
        };
        queue_rich_bytes(runtime, bytes)?;
        Ok(Some(true))
    }

    pub(crate) fn rich_pointer(
        &mut self,
        pane_id: u64,
        viewer_id: ViewerId,
        phase: PointerPhase,
        row: u16,
        col: u16,
    ) -> Result<Option<bool>, LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        let Some(rich) = runtime.rich.as_mut() else {
            return Ok(None);
        };
        if rich.structured_focus_node(viewer_id).is_none() {
            return Ok(None);
        }
        let bytes = rich.handle_structured_pointer(
            viewer_id,
            u16::try_from(runtime.outer_rows).unwrap_or(u16::MAX),
            u16::try_from(runtime.cols).unwrap_or(u16::MAX),
            phase,
            row,
            col,
        );
        let delivered = bytes.is_some();
        if let Some(bytes) = bytes {
            queue_rich_bytes(runtime, bytes)?;
        }
        Ok(Some(delivered))
    }

    pub(crate) fn rich_scroll(
        &mut self,
        pane_id: u64,
        viewer_id: ViewerId,
        row: u16,
        col: u16,
        delta: i16,
    ) -> Result<Option<bool>, LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        let Some(rich) = runtime.rich.as_mut() else {
            return Ok(None);
        };
        if rich.structured_focus_node(viewer_id).is_none() {
            return Ok(None);
        }
        let bytes = rich.encode_structured_scroll(
            viewer_id,
            u16::try_from(runtime.outer_rows).unwrap_or(u16::MAX),
            u16::try_from(runtime.cols).unwrap_or(u16::MAX),
            row,
            col,
            delta,
        );
        let delivered = bytes.is_some();
        if let Some(bytes) = bytes {
            queue_rich_bytes(runtime, bytes)?;
        }
        Ok(Some(delivered))
    }

    pub(crate) fn peek_pending_semantic_copy(
        &self,
        pane_id: u64,
        client_id: u64,
    ) -> Option<(u64, String)> {
        self.panes
            .get(&pane_id)?
            .rich
            .as_ref()?
            .peek_copy_for(client_id)
    }

    pub(crate) fn note_controller(&mut self, pane_id: u64, controller: Option<u64>) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.controller_id = controller;
        }
    }

    pub(crate) fn copy_semantic_for(
        &mut self,
        pane_id: u64,
        client_id: u64,
        seq: Option<u64>,
    ) -> (Option<String>, Option<u64>) {
        let Some(rich) = self
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.rich.as_mut())
        else {
            return (None, None);
        };
        match seq {
            Some(seq) => match rich.ack_copy_for(client_id, seq) {
                Some(text) => (Some(text), Some(seq)),
                None => (None, None),
            },
            None => (rich.semantic_copy_text("diag"), None),
        }
    }

    pub(crate) fn revoke_viewer_on(
        &mut self,
        pane_id: u64,
        viewer_id: ViewerId,
    ) -> Result<bool, LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        let Some(rich) = runtime.rich.as_mut() else {
            return Ok(false);
        };
        let bytes = rich
            .revoke_structured_focus(viewer_id)
            .or_else(|| rich.revoke_focus(viewer_id));
        let revoked = bytes.is_some();
        if let Some(bytes) = bytes {
            queue_rich_bytes(runtime, bytes)?;
        }
        Ok(revoked)
    }

    pub(crate) fn content(&self, pane_id: u64) -> Option<PaneContent> {
        self.panes
            .get(&pane_id)
            .map(|runtime| runtime.content(pane_id))
    }

    pub(crate) fn styled(
        &self,
        pane_id: u64,
        view_offset: Option<u32>,
        viewer_id: ViewerId,
    ) -> Option<PaneStyled> {
        self.panes
            .get(&pane_id)
            .map(|runtime| runtime.styled(pane_id, view_offset, Some(viewer_id)))
    }

    #[cfg(test)]
    pub(crate) fn feed_rich_for_test(
        &mut self,
        pane_id: u64,
        bytes: &[u8],
    ) -> Result<(), LiveWriteError> {
        let runtime = self
            .panes
            .get_mut(&pane_id)
            .ok_or(LiveWriteError::UnknownPane)?;
        if runtime.rich.is_none() {
            runtime.emulator =
                Emulator::new_experimental(runtime.cols, runtime.rows, MAX_SCROLLBACK);
        }
        let rich = runtime.rich.get_or_insert_with(RichSession::default);
        rich::process_rich_chunk(&mut runtime.emulator, rich, &runtime.to_child_tx, bytes);
        runtime
            .reconcile_workspace_geometry()
            .map_err(|_| LiveWriteError::Disconnected)
    }

    /// Drain all panes. Returns Hive death notices plus per-pane output ticks
    /// (revision or child-alive change this pass).
    pub(crate) fn drain(&mut self) -> (Vec<CellExitNotice>, Vec<OutputActivityTick>) {
        let mut notices = Vec::new();
        let mut activity = Vec::new();
        for (&pane_id, runtime) in self.panes.iter_mut() {
            let before_revision = runtime.revision;
            let before_alive = runtime.child_alive;
            let (notice, nbytes, attention) = runtime.drain(pane_id);
            if let Some(notice) = notice {
                notices.push(notice);
            }
            if runtime.revision != before_revision
                || runtime.child_alive != before_alive
                || attention.is_some()
            {
                activity.push(OutputActivityTick {
                    pane_id,
                    revision: runtime.revision,
                    child_alive: runtime.child_alive,
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

    /// Stable persist fingerprint: pane id and current seq, ordered.
    pub(crate) fn persist_mark(&self) -> u64 {
        let mut ids: Vec<u64> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        let mut mark = 0u64;
        for id in ids {
            let seq = self
                .panes
                .get(&id)
                .map(|pane| pane.log.current_seq())
                .unwrap_or(0);
            mark = mark
                .wrapping_mul(1_000_003)
                .wrapping_add(id)
                .wrapping_mul(1_000_003)
                .wrapping_add(seq);
        }
        mark
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
                        id: *id,
                        record: None,
                        state: None,
                    });
                }
                let state = pane.emulator.export_state().ok();
                let snapshot_seq = if state.is_some() {
                    pane.log.current_seq()
                } else {
                    pane.log.oldest_seq().unwrap_or(1).saturating_sub(1)
                };
                Some(PendingPane {
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

fn queue_rich_bytes(runtime: &LivePane, bytes: Vec<u8>) -> Result<(), LiveWriteError> {
    match runtime.to_child_tx.try_send(bytes) {
        Ok(()) => Ok(()),
        Err(mpsc::TrySendError::Full(_)) => Err(LiveWriteError::Backpressure),
        Err(mpsc::TrySendError::Disconnected(_)) => Err(LiveWriteError::Disconnected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sleep_spawn(env: BTreeMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sleep".into(),
            argv: vec!["30".into()],
            cwd: Some(PathBuf::from("/tmp")),
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

    fn wait_for_pane_text(pane: &mut LivePane, pane_id: u64, needle: &str) -> String {
        for _ in 0..200 {
            let _ = pane.drain(pane_id);
            let text = pane.content(pane_id).lines.join("\n");
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("never saw {needle:?} in {:?}", pane.content(pane_id).lines);
    }

    #[test]
    fn hive_cell_is_captured_from_spawn_env_at_spawn() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:1@1".into());
        let pane = spawn_test_pane(7, &sleep_spawn(env), None);
        assert_eq!(pane.hive_cell.as_deref(), Some("cell:1@1"));
        assert!(pane.child_pid.is_some(), "spawn should expose a child pid");
        assert!(pane.child_alive);
        assert!(!pane.exit_notified);
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
        assert!(!pane.child_alive);
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
    fn true_exiting_emits_notice_when_seated() {
        let mut env = BTreeMap::new();
        env.insert(HIVE_CELL_ENV.into(), "cell:1@1".into());
        let spawn = SpawnSpec {
            // `/usr/bin/true` exists on both Linux and macOS; `/bin/true` is Linux-only.
            program: "/usr/bin/true".into(),
            argv: vec![],
            cwd: Some(PathBuf::from("/tmp")),
            env,
        };
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
        let mark = runtime.persist_mark();
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
        assert_ne!(runtime.persist_mark(), mark);
        let (pending, captured) = runtime.capture_persist(&keys, &captured);
        assert_eq!(pending[0].record.as_ref().unwrap().cell_px, (13, 27));
        let state = pending[0].state.as_ref().unwrap();
        assert_eq!((state.cell_width_px, state.cell_height_px), (13, 27));
        let mark = runtime.persist_mark();
        runtime
            .panes
            .get_mut(&1)
            .unwrap()
            .resize_guest(80, 24)
            .unwrap();
        assert_eq!(runtime.persist_mark(), mark);
        let (pending, _) = runtime.capture_persist(&keys, &captured);
        assert!(pending[0].record.is_none());
    }

    #[test]
    fn pane_log_pixel_resize_preserves_superseded_resize_reflow() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        pane.apply_pty_bytes(b"abcdefgh");
        let mut replica = replay_log(&pane);
        pane.resize(4, 24, 8, 16).expect("logical resize");
        pane.resize(4, 24, 13, 27).expect("pixel resize");
        crate::pane_log_persist::replay_event(
            &mut replica,
            &pane.log.iter().last().unwrap().event,
        );
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

    #[test]
    fn styled_reports_child_mouse_modes() {
        let mut pane = spawn_test_pane(1, &sleep_spawn(BTreeMap::new()), None);
        let off = pane.styled(1, None, None);
        assert_eq!(off.child_mouse_tracking, Some(false));
        assert_eq!(off.child_mouse_sgr, Some(false));
        pane.emulator.feed(b"\x1b[?1000h\x1b[?1006h");
        let on = pane.styled(1, None, None);
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
        let _ = pane.styled(1, None, None);
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
