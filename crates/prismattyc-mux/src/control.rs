//! Versioned local control plane for Prismattyc's in-process mux (PRD §2.8.7).
//!
//! The wire format is one bounded JSON object per line. Each connection owns a
//! monotonically increasing request-id space. State-changing requests append to
//! one domain-wide sequence so clients can take a snapshot and replay ordered
//! events, or explicitly resnapshot when the bounded replay window has a gap.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};

use crate::local_socket::{UnixListener, UnixStream};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prismattyc_protocol::{InputModifiers, PointerPhase, ViewerId};
use serde::{Deserialize, Serialize};

use crate::live::{LiveRuntime, LiveWriteError};
use crate::pane_log::{CatchUp, PaneLogWatch};
pub use crate::pane_log::{PaneEvent, PaneLogFrame};
use crate::pane_log_persist::{
    read_persist, write_persist, EmulatorStateCodec, PendingPane, PersistFile, PersistWorker,
    ScreenCodec, PERSIST_CADENCE,
};
use crate::remote_size::{
    disconnect_decision, record_host_chosen, resize_decision, ClientRole, RemoteSizePolicy,
    SizeOwner, SizeOwnerKind, Viewport,
};
use crate::supervisor::{read_operator_lease, SupervisorSink};
use crate::{
    apply_arrangement, layout_to_rects, Arrangement, Axis, CellRect, ClientId, Domain, DomainError,
    GeometryError, PaneId, PaneLayout, SessionId, WindowId, DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS,
};

/// First public control-plane protocol version.
pub const PROTOCOL_VERSION: u16 = 1;
const DEFAULT_EVENT_CAPACITY: usize = 256;
const MAX_EVENT_CAPACITY: usize = 4096;
const MAX_EVENT_BATCH: usize = 256;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
mod pane_write;
pub use pane_write::PaneWriteSubmit;

const MAX_SPAWN_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Wire-safe split orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisWire {
    Horizontal,
    Vertical,
}

impl From<AxisWire> for Axis {
    fn from(axis: AxisWire) -> Self {
        match axis {
            AxisWire::Horizontal => Self::Horizontal,
            AxisWire::Vertical => Self::Vertical,
        }
    }
}

/// Wire-safe named arrangement (PT-132).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrangementWire {
    #[serde(rename = "main-vertical")]
    MainVertical,
    #[serde(rename = "main-horizontal")]
    MainHorizontal,
    #[serde(rename = "even-h")]
    EvenH,
    #[serde(rename = "even-v")]
    EvenV,
    #[serde(rename = "grid")]
    Grid,
}

impl ArrangementWire {
    pub fn parse_name(name: &str) -> Option<Self> {
        match name {
            "main-vertical" => Some(Self::MainVertical),
            "main-horizontal" => Some(Self::MainHorizontal),
            "even-h" => Some(Self::EvenH),
            "even-v" => Some(Self::EvenV),
            "grid" => Some(Self::Grid),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainVertical => "main-vertical",
            Self::MainHorizontal => "main-horizontal",
            Self::EvenH => "even-h",
            Self::EvenV => "even-v",
            Self::Grid => "grid",
        }
    }

    pub const ALL: [Self; 5] = [
        Self::EvenH,
        Self::EvenV,
        Self::Grid,
        Self::MainVertical,
        Self::MainHorizontal,
    ];
}

impl From<ArrangementWire> for Arrangement {
    fn from(kind: ArrangementWire) -> Self {
        match kind {
            ArrangementWire::MainVertical => Self::MainVertical,
            ArrangementWire::MainHorizontal => Self::MainHorizontal,
            ArrangementWire::EvenH => Self::EvenHorizontal,
            ArrangementWire::EvenV => Self::EvenVertical,
            ArrangementWire::Grid => Self::Grid,
        }
    }
}

/// Structured child launch metadata. There is intentionally no shell-command field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub program: String,
    #[serde(default)]
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl SpawnSpec {
    fn validate(&self) -> Result<(), ControlError> {
        if self.program.trim().is_empty() || self.program.contains('\0') {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "spawn program must be non-empty and contain no NUL",
            ));
        }
        if self.argv.len() > 4096
            || self
                .argv
                .iter()
                .any(|arg| arg.len() > 64 * 1024 || arg.contains('\0'))
        {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "spawn argv exceeds limits or contains NUL",
            ));
        }
        if self.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "spawn cwd must be absolute",
            ));
        }
        if self.cwd.as_ref().is_some_and(|cwd| {
            cwd.to_str().is_none() || cwd.as_os_str().as_encoded_bytes().contains(&0)
        }) {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "spawn cwd must be UTF-8 and contain no NUL",
            ));
        }
        if self.env.len() > 4096
            || self.env.iter().any(|(key, value)| {
                key.is_empty()
                    || key.contains('=')
                    || key.contains('\0')
                    || value.contains('\0')
                    || key.len() > 64 * 1024
                    || value.len() > 64 * 1024
            })
        {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "spawn env exceeds limits or contains an invalid key/value",
            ));
        }
        let total_bytes = self
            .argv
            .iter()
            .map(String::len)
            .chain(
                self.env
                    .iter()
                    .flat_map(|(key, value)| [key.len(), value.len()]),
            )
            .chain(
                self.cwd
                    .as_ref()
                    .map(|cwd| cwd.as_os_str().as_encoded_bytes().len()),
            )
            .fold(self.program.len(), usize::saturating_add);
        if total_bytes > MAX_SPAWN_BYTES {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("structured spawn data exceeds {MAX_SPAWN_BYTES} bytes"),
            ));
        }
        Ok(())
    }
}

/// One attach client's last reported window size plus optional cell pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientViewport {
    cols: u32,
    rows: u32,
    cell_width_px: Option<u32>,
    cell_height_px: Option<u32>,
}

impl From<ClientViewport> for Viewport {
    fn from(view: ClientViewport) -> Self {
        Self {
            cols: view.cols,
            rows: view.rows,
            cell_width_px: view.cell_width_px,
            cell_height_px: view.cell_height_px,
        }
    }
}

impl From<Viewport> for ClientViewport {
    fn from(view: Viewport) -> Self {
        Self {
            cols: view.cols,
            rows: view.rows,
            cell_width_px: view.cell_width_px,
            cell_height_px: view.cell_height_px,
        }
    }
}

/// Authoritative cell bounds for one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub window_id: u64,
    pub cols: u32,
    pub rows: u32,
}

/// Wake honesty for `pane.mail` (ADR-0039). Stored, not computed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailWake {
    Unarmed,
    Armed,
    DeferredFocused,
    DeferredBusy,
    Rung,
    Stuck,
    Exhausted,
    McpDead,
}

/// Sticky mail attention for one pane. Depth>0 lights `pane.mail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAttentionState {
    pub pane_id: u64,
    pub cell: String,
    pub gen: u64,
    pub queue_rev: u64,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<MailWake>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_pid: Option<u32>,
}

/// Hive cell address for `MailAttentionSet`.
///
/// Accepts lossless `1b@1` / `12@1` or padded `cell:<32 hex>@<generation>`.
/// Labels such as `cli` are not addresses and must not be stored: Clear peeks
/// this string, and Hive rejects anything that is not an address.
pub fn parse_hive_cell_address(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    const MSG: &str = "mail cell must be a Hive address (1b@1 or cell:<32 hex digits>@generation)";
    if raw.is_empty() || raw.len() > 128 || raw.contains('\0') {
        return Err(MSG.to_string());
    }
    if let Some(rest) = raw.strip_prefix("cell:") {
        let Some((hex, gen)) = rest.rsplit_once('@') else {
            return Err(MSG.to_string());
        };
        if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(MSG.to_string());
        }
        if !valid_hive_generation(gen) {
            return Err(MSG.to_string());
        }
        return Ok(raw.to_string());
    }
    let Some((hex, gen)) = raw.rsplit_once('@') else {
        return Err(MSG.to_string());
    };
    if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MSG.to_string());
    }
    if !valid_hive_generation(gen) {
        return Err(MSG.to_string());
    }
    Ok(raw.to_string())
}

fn valid_hive_generation(gen: &str) -> bool {
    !gen.is_empty()
        && gen.len() <= 20
        && gen.bytes().all(|b| b.is_ascii_digit())
        && gen.parse::<u64>().is_ok()
}

/// Exact inject text. Body is never written.
/// Submit bytes depend on the guest CLI ([`crate::inject_writes`]).
///
/// `HIVE_MAIL` and `SWITCHBOARD_MAIL` are retired.
pub const PMUX_MAIL_NOTIFICATION: &str = "PMUX_MAIL";
/// Default submit (Claude / Grok / unknown): text plus CR.
pub const MAIL_INJECT_PAYLOAD: &str = "PMUX_MAIL\r";
/// Sticky attention label for mux-native mail. `switchboard` is retired.
pub const MAIL_ATTENTION_CELL: &str = "mail";
/// Rolling window for the inject busy gate.
pub const MAIL_INJECT_QUIET_MS: u64 = 3_000;
/// PTY bytes inside [`MAIL_INJECT_QUIET_MS`] that count as busy.
/// Idle TUI spinner/clock ticks stay under this; streamed replies do not.
pub const MAIL_INJECT_BUSY_BYTES: usize = 8_192;
/// After a write that moved epoch, hide ordinary retries for this long.
pub const MAIL_INJECT_HIDE_MS: u64 = 60_000;
/// Verify a Wrote doorbell promptly, without waiting for the ordinary hide window.
pub const MAIL_INJECT_VERIFY_MS: u64 = 8_000;
/// Delay before a lit, unread mail attention retries its doorbell.
pub const MAIL_INJECT_NUDGE_DELAY_MS: u64 = 3_000;
/// Defer inject while a controller write landed inside this window (PT-85).
pub const MAIL_INJECT_INPUT_IDLE_MS: u64 = 10_000;
/// Clear a leftover dirty composer after this long with no write and no output.
pub const MAIL_INJECT_DIRTY_IDLE_MS: u64 = 60_000;
const MAIL_INJECT_EPOCH_WAIT_MS: u64 = 250;
const MAIL_INJECT_EPOCH_SLICE_MS: u64 = 10;

/// Same-step inject result. Defer/skip is success, not an Error — the
/// caller must not count those as attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailInjectOutcome {
    Wrote,
    Stuck,
    DeferredFocused,
    DeferredLease,
    DeferredBusy,
    DeferredDirty,
    DeferredTyping,
    /// The pane's foreground is not a known agent CLI (a plain shell):
    /// nothing was written; attention stays lit and the nudge retries (PT-94).
    DeferredNoAgent,
    DeferredHide,
    SkippedNoMail,
    SkippedExhausted,
    SkippedStuck,
}

#[derive(Clone, Copy)]
struct MailInjectRecord {
    queue_rev: u64,
    stuck: bool,
    hide_until_ms: u64,
    hide_revision: u64,
    writes: u32,
}

#[derive(Clone, Copy)]
struct MailNudge {
    queue_rev: u64,
    due_at_ms: u64,
    /// A post-write verification retry. It is bounded to one extra write.
    verification: bool,
}

/// Last mail doorbell attempt for operator diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailInjectDiagnostic {
    pub agent: String,
    pub queue_rev: u64,
    pub outcome: MailInjectOutcome,
    pub nbytes: usize,
    pub at_ms: u64,
}

/// Per-pane inject-gate facts. `dirty_input` is derived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInputLedger {
    pub last_output_at_ms: Option<u64>,
    pub focused: bool,
    pub controller_id: Option<u64>,
    pub last_controller_write_at_ms: Option<u64>,
    pub last_write_ended_with_cr: bool,
    pub dirty_input: bool,
    /// Last `WritePane` (unix ms). Inject defers while this is recent.
    pub last_input_at_ms: Option<u64>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn write_ended_with_cr(data: &str) -> bool {
    data.as_bytes().last() == Some(&b'\r')
}

fn dirty_input_still_open(
    last_controller_write_at_ms: Option<u64>,
    last_write_ended_with_cr: bool,
    last_output_at_ms: Option<u64>,
    now: u64,
) -> bool {
    let Some(write_at) = last_controller_write_at_ms else {
        return false;
    };
    if last_write_ended_with_cr {
        return false;
    }
    let write_idle = now.saturating_sub(write_at) >= MAIL_INJECT_DIRTY_IDLE_MS;
    let output_idle = last_output_at_ms
        .map(|at| now.saturating_sub(at) >= MAIL_INJECT_DIRTY_IDLE_MS)
        .unwrap_or(true);
    !(write_idle && output_idle)
}

/// Versioned request envelope. Request IDs are monotonic per connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    ServerInfo {
        version: u16,
        request_id: u64,
    },
    Ping {
        version: u16,
        request_id: u64,
    },
    Snapshot {
        version: u16,
        request_id: u64,
    },
    Events {
        version: u16,
        request_id: u64,
        after_sequence: u64,
        limit: Option<usize>,
    },
    /// Stream pane-log frames on this connection (PT-77). Served on the
    /// client thread like `MailWait`. `from_seq` is "I have through this
    /// seq". Older than the ring yields a snapshot then new events.
    SubscribePane {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        from_seq: u64,
        /// Idle wait after catch-up. Zero returns catch-up and ends.
        #[serde(default)]
        timeout_ms: u32,
    },
    Split {
        version: u16,
        request_id: u64,
        window_id: u64,
        target_pane_id: u64,
        axis: AxisWire,
        ratio: f64,
        spawn: SpawnSpec,
        /// When set, caller must hold the target pane controller lease.
        #[serde(default)]
        client_id: Option<u64>,
    },
    Close {
        version: u16,
        request_id: u64,
        window_id: u64,
        pane_id: u64,
        prior_focus_id: u64,
        /// When set, caller must hold the closed pane controller lease.
        #[serde(default)]
        client_id: Option<u64>,
    },
    Resize {
        version: u16,
        request_id: u64,
        window_id: u64,
        cols: u32,
        rows: u32,
        /// Viewer cell size in pixels. Omit to keep the pane's current size
        /// (nominal 10×20 at spawn). Attach sends outer `TIOCGWINSZ` / cols.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_width_px: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_height_px: Option<u32>,
        /// When set, this viewer's size is tracked so a smaller second
        /// attach cannot shrink a shared pane.
        #[serde(default)]
        client_id: Option<u64>,
        /// Apply this size even when the caller does not hold the lease
        /// (PT-100 `C-\ z` / `pmux attach --fit`).
        #[serde(default)]
        fit: bool,
        /// True when this size is the desktop host window (PT-200).
        #[serde(default)]
        host: bool,
    },
    SuggestFocus {
        version: u16,
        request_id: u64,
        window_id: u64,
        pane_id: u64,
        /// Advisory reason (`mail`). Never steals focus.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Operator reports the pane this client is looking at (ledger).
    ReportFocus {
        version: u16,
        request_id: u64,
        client_id: u64,
        window_id: u64,
        pane_id: u64,
    },
    /// Raise a validated agent-attention message without writing to the pane PTY.
    RaiseAttention {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        message: String,
    },
    /// Inspect explicit attention requests without touching mailbox delivery.
    AttentionRequests {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Resolve or snooze only the request that the operator inspected.
    UpdateAttention {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        revision: u64,
        expected_space: String,
        expected_session: u64,
        action: crate::team_attention::AttentionAction,
    },
    /// Publish sticky mail attention for a pane.
    ///
    /// Registered client identity (controller leases). Not a cell token. Not a lease.
    /// Idempotent on `(pane_id, queue_rev)`. `depth == 0` clears.
    MailAttentionSet {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        cell: String,
        gen: u64,
        queue_rev: u64,
        depth: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<MailWake>,
        /// Adopted agent pid (ADR-0039 join key). Optional on the mux verb;
        /// the supervisor bridge maps pid to `pane_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bound_pid: Option<u32>,
    },
    /// Clear `pane.mail` attention for one pane (same as Set with depth=0).
    ///
    /// `queue_rev` is the ordering key: a stale rev must not erase a newer one.
    MailAttentionClear {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        queue_rev: u64,
    },
    /// Gated doorbell inject. Writes only the fixed doorbell token
    /// ([`PMUX_MAIL_NOTIFICATION`]) plus guest submit bytes.
    /// Never TakeoverLease. Defer outcomes are Ok, not errors.
    InjectMail {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        queue_rev: u64,
        /// Remaining attempts for this rev. Zero is exhausted.
        remaining_attempts: u32,
    },
    /// Bind an agent identity to this connection's client.
    ///
    /// Client-asserted, same trust model as the mailbox `hello`:
    /// acceptable while every client is a same-uid local agent. Repeat
    /// binds across connections are fine (reconnect-per-op clients);
    /// one client may rebind its own agent.
    MailHello {
        version: u16,
        request_id: u64,
        client_id: u64,
        agent: String,
    },
    /// Deliver a letter. `to` is an agent id or alias — never a
    /// seat. Unknown recipients still queue (FR-1). The sender identity
    /// is the connection's `MailHello` agent; there is no `from` field.
    MailSend {
        version: u16,
        request_id: u64,
        client_id: u64,
        to: String,
        summary: String,
        body: String,
    },
    /// Fetch this agent's uncommitted letters: open become held,
    /// already-held are re-listed (reconnect recovery).
    MailClaim {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Acknowledge held letters: gone for good.
    MailCommit {
        version: u16,
        request_id: u64,
        client_id: u64,
        ids: Vec<String>,
    },
    /// Return held letters to open.
    MailRelease {
        version: u16,
        request_id: u64,
        client_id: u64,
        ids: Vec<String>,
    },
    /// Peek at mailbox depth without claiming.
    MailInbox {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Block until this agent has open mail or the (capped) timeout
    /// elapses. Handled on the client thread; never parks while holding
    /// the control-plane mutex. Replies `MailDepth` either way.
    MailWait {
        version: u16,
        request_id: u64,
        client_id: u64,
        timeout_ms: u32,
    },
    /// List live agent-bound sessions. An agent is not listed
    /// until its session exists; queued mail for unknown agents is
    /// durable regardless.
    MailWho {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Bind a shorthand to this connection's agent (self-serve).
    MailAlias {
        version: u16,
        request_id: u64,
        client_id: u64,
        name: String,
    },
    /// One letter to every live agent-bound session except the caller's
    /// agent. Single transaction: everyone or nobody.
    MailBroadcast {
        version: u16,
        request_id: u64,
        client_id: u64,
        summary: String,
        body: String,
    },
    /// Attach C-S-G toggle or Esc revoke for `input.rich_focus`.
    /// Requires the pane controller lease. Flag-off is a no-op Ok.
    RichFocusToggle {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        /// When true, revoke only (Esc). When false, toggle grant (C-S-G).
        #[serde(default)]
        revoke: bool,
    },
    /// Connection-bound structured input intent. The server mints the viewer
    /// identity and validates controller, pane, focus, scene, and action state
    /// before it creates an application APC.
    RichInput {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        input: RichInputKind,
    },
    /// Copy the current semantic projection for this pane.
    CopySemantic {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        /// Targeted ack for a peeked `semantic_clipboard_seq`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    /// Mint a durable client identity for lease and write authorization.
    RegisterClient {
        version: u16,
        request_id: u64,
    },
    /// Acquire a free controller lease (fails if held by another client).
    AcquireLease {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
    },
    /// Release the caller's controller lease.
    ReleaseLease {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
    },
    /// Explicit takeover of a pane controller lease.
    TakeoverLease {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
    },
    /// Clear every lease held by `client_id` (connection drop / logout).
    DisconnectClient {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Raw terminal input. A clean, unleased pane permits lease-free writes.
    /// Sync input may fan these bytes out to sibling panes.
    WritePane {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        /// UTF-8 payload for tests and control clients (no shell string evaluation).
        data: String,
    },
    /// Intentional text for exactly one pane. Never takes over a controller.
    PaneWrite {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        expected_child_pid: u32,
        data: String,
        #[serde(default)]
        submit: PaneWriteSubmit,
    },
    /// Read the latest server-owned visible grid for one pane.
    ReadPane {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
    },
    /// Read the visible grid plus RLE style runs. `ReadPane` stays
    /// the text-only projection so JSON dumps remain byte-stable.
    ///
    /// `view_offset` is rows back from the live tail. Absent/`null`
    /// is the live tail. Old servers that reject the field fail this verb;
    /// the attach client then falls back to `ReadPane` and hides scroll mode.
    ReadPaneStyled {
        version: u16,
        request_id: u64,
        client_id: u64,
        pane_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_offset: Option<u32>,
    },
    /// Create a named session with one window and one leaf pane.
    /// `headless` skips the window/pane; mail still queues.
    CreateSession {
        version: u16,
        request_id: u64,
        name: String,
        spawn: SpawnSpec,
        #[serde(default)]
        cols: Option<u32>,
        #[serde(default)]
        rows: Option<u32>,
        /// Opt-in mailbox address. Absent/`null` means no agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// No window, no pane, no PTY.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        headless: bool,
    },
    /// Bind a host writer to one Space for the life of this connection.
    SetClientSpace {
        version: u16,
        request_id: u64,
        client_id: u64,
        space_id: String,
    },
    /// Compare and transfer exclusive Space ownership as one daemon mutation.
    TransferSpaceSessions {
        version: u16,
        request_id: u64,
        session_ids: Vec<u64>,
        from: Option<String>,
        to: Option<String>,
    },
    /// Move one live pane across Spaces without recreating its PTY.
    TransferSpacePane {
        version: u16,
        request_id: u64,
        pane_id: u64,
        to_session_id: u64,
        from_space: String,
        to_space: String,
    },
    /// List live names and retained mailbox addresses for the naming popup.
    SessionNames {
        version: u16,
        request_id: u64,
    },
    /// Set the human session name and mailbox address together. Keep the live seat.
    NameSession {
        version: u16,
        request_id: u64,
        session_id: u64,
        name: String,
    },
    /// Client-local session selection (PRD §2.8.3). Does not destroy others.
    SwitchSession {
        version: u16,
        request_id: u64,
        client_id: u64,
        session_id: u64,
    },
    /// Destroy a named session and all of its windows/panes. The server stays up.
    DestroySession {
        version: u16,
        request_id: u64,
        session_id: u64,
    },
    /// Terminate the long-lived server (control protocol). Not detach.
    ShutdownServer {
        version: u16,
        request_id: u64,
        /// Registered client identity; same tier as session verbs (no pane lease).
        client_id: u64,
    },
    /// Atomically stop an empty daemon and reject later mutations.
    /// Separate verb so older daemons fail closed instead of ignoring a flag.
    ShutdownIdle {
        version: u16,
        request_id: u64,
        client_id: u64,
    },
    /// Create a window (tab) with one leaf pane in an existing session.
    CreateWindow {
        version: u16,
        request_id: u64,
        session_id: u64,
        title: String,
        spawn: SpawnSpec,
        #[serde(default)]
        cols: Option<u32>,
        #[serde(default)]
        rows: Option<u32>,
    },
    /// Destroy a window. Empty-session policy applies (mux architecture).
    DestroyWindow {
        version: u16,
        request_id: u64,
        window_id: u64,
    },
    /// Move a pane between windows. One `PaneMoved` event; never Closed+Split.
    MovePane {
        version: u16,
        request_id: u64,
        from_window_id: u64,
        to_window_id: u64,
        pane_id: u64,
        target_pane_id: u64,
        axis: AxisWire,
        ratio: f64,
        /// When set, caller must hold the moved pane's controller lease.
        #[serde(default)]
        client_id: Option<u64>,
    },
    /// Rebuild the window split tree from a named arrangement (PT-132).
    /// Does not spawn or close panes.
    ApplyArrangement {
        version: u16,
        request_id: u64,
        window_id: u64,
        kind: ArrangementWire,
        /// Becomes the main pane for main-vertical / main-horizontal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focused_pane_id: Option<u64>,
    },
    /// Client-local window (tab) selection (PRD §2.8.3). No topology mutation.
    SwitchWindow {
        version: u16,
        request_id: u64,
        client_id: u64,
        window_id: u64,
    },
    /// Set a window title in place. No lease.
    RenameWindow {
        version: u16,
        request_id: u64,
        window_id: u64,
        title: String,
    },
    /// Fan typed input out to every pane in the window. No pane lease.
    SetSyncInput {
        version: u16,
        request_id: u64,
        client_id: u64,
        window_id: u64,
        enabled: bool,
    },
    /// Guest-driven pane status text. No lease. `None` or empty clears.
    SetPaneStatus {
        version: u16,
        request_id: u64,
        pane_id: u64,
        text: Option<String>,
    },
    /// Set a pane's title; an empty title clears it (PT-128). No lease:
    /// any registered client may rename. Shown by `pmux ls`, the attach
    /// chrome, and saved into space files.
    RenamePane {
        version: u16,
        request_id: u64,
        pane_id: u64,
        title: String,
    },
}

impl ControlRequest {
    fn header(&self) -> (u16, u64) {
        match self {
            Self::ServerInfo {
                version,
                request_id,
            }
            | Self::Ping {
                version,
                request_id,
            }
            | Self::Snapshot {
                version,
                request_id,
            }
            | Self::Events {
                version,
                request_id,
                ..
            }
            | Self::SubscribePane {
                version,
                request_id,
                ..
            }
            | Self::RegisterClient {
                version,
                request_id,
            }
            | Self::AcquireLease {
                version,
                request_id,
                ..
            }
            | Self::ReleaseLease {
                version,
                request_id,
                ..
            }
            | Self::TakeoverLease {
                version,
                request_id,
                ..
            }
            | Self::DisconnectClient {
                version,
                request_id,
                ..
            }
            | Self::PaneWrite {
                version,
                request_id,
                ..
            }
            | Self::WritePane {
                version,
                request_id,
                ..
            }
            | Self::ReadPane {
                version,
                request_id,
                ..
            }
            | Self::ReadPaneStyled {
                version,
                request_id,
                ..
            }
            | Self::Split {
                version,
                request_id,
                ..
            }
            | Self::Close {
                version,
                request_id,
                ..
            }
            | Self::Resize {
                version,
                request_id,
                ..
            }
            | Self::SuggestFocus {
                version,
                request_id,
                ..
            }
            | Self::SessionNames {
                version,
                request_id,
            }
            | Self::NameSession {
                version,
                request_id,
                ..
            }
            | Self::CreateSession {
                version,
                request_id,
                ..
            }
            | Self::SwitchSession {
                version,
                request_id,
                ..
            }
            | Self::TransferSpaceSessions {
                version,
                request_id,
                ..
            }
            | Self::TransferSpacePane {
                version,
                request_id,
                ..
            }
            | Self::SetClientSpace {
                version,
                request_id,
                ..
            }
            | Self::DestroySession {
                version,
                request_id,
                ..
            }
            | Self::ShutdownIdle {
                version,
                request_id,
                ..
            }
            | Self::ShutdownServer {
                version,
                request_id,
                ..
            }
            | Self::CreateWindow {
                version,
                request_id,
                ..
            }
            | Self::DestroyWindow {
                version,
                request_id,
                ..
            }
            | Self::MovePane {
                version,
                request_id,
                ..
            }
            | Self::ApplyArrangement {
                version,
                request_id,
                ..
            }
            | Self::SwitchWindow {
                version,
                request_id,
                ..
            }
            | Self::RenameWindow {
                version,
                request_id,
                ..
            }
            | Self::RenamePane {
                version,
                request_id,
                ..
            }
            | Self::SetSyncInput {
                version,
                request_id,
                ..
            }
            | Self::SetPaneStatus {
                version,
                request_id,
                ..
            }
            | Self::ReportFocus {
                version,
                request_id,
                ..
            }
            | Self::RaiseAttention {
                version,
                request_id,
                ..
            }
            | Self::AttentionRequests {
                version,
                request_id,
                ..
            }
            | Self::UpdateAttention {
                version,
                request_id,
                ..
            }
            | Self::MailAttentionSet {
                version,
                request_id,
                ..
            }
            | Self::MailAttentionClear {
                version,
                request_id,
                ..
            }
            | Self::InjectMail {
                version,
                request_id,
                ..
            }
            | Self::MailHello {
                version,
                request_id,
                ..
            }
            | Self::MailSend {
                version,
                request_id,
                ..
            }
            | Self::MailClaim {
                version,
                request_id,
                ..
            }
            | Self::MailCommit {
                version,
                request_id,
                ..
            }
            | Self::MailRelease {
                version,
                request_id,
                ..
            }
            | Self::MailInbox {
                version,
                request_id,
                ..
            }
            | Self::MailWait {
                version,
                request_id,
                ..
            }
            | Self::MailWho {
                version,
                request_id,
                ..
            }
            | Self::MailAlias {
                version,
                request_id,
                ..
            }
            | Self::MailBroadcast {
                version,
                request_id,
                ..
            }
            | Self::RichFocusToggle {
                version,
                request_id,
                ..
            }
            | Self::RichInput {
                version,
                request_id,
                ..
            }
            | Self::CopySemantic {
                version,
                request_id,
                ..
            } => (*version, *request_id),
        }
    }

    fn claimed_client_id(&self) -> Option<u64> {
        match self {
            Self::AcquireLease { client_id, .. }
            | Self::ReleaseLease { client_id, .. }
            | Self::TakeoverLease { client_id, .. }
            | Self::DisconnectClient { client_id, .. }
            | Self::WritePane { client_id, .. }
            | Self::PaneWrite { client_id, .. }
            | Self::SetClientSpace { client_id, .. }
            | Self::ReadPane { client_id, .. }
            | Self::ReadPaneStyled { client_id, .. }
            | Self::SubscribePane { client_id, .. }
            | Self::SwitchSession { client_id, .. }
            | Self::ShutdownIdle { client_id, .. }
            | Self::ShutdownServer { client_id, .. }
            | Self::SwitchWindow { client_id, .. }
            | Self::ReportFocus { client_id, .. }
            | Self::RaiseAttention { client_id, .. }
            | Self::AttentionRequests { client_id, .. }
            | Self::UpdateAttention { client_id, .. }
            | Self::MailAttentionSet { client_id, .. }
            | Self::MailAttentionClear { client_id, .. }
            | Self::InjectMail { client_id, .. }
            | Self::MailHello { client_id, .. }
            | Self::MailSend { client_id, .. }
            | Self::MailClaim { client_id, .. }
            | Self::MailCommit { client_id, .. }
            | Self::MailRelease { client_id, .. }
            | Self::MailInbox { client_id, .. }
            | Self::MailWait { client_id, .. }
            | Self::MailWho { client_id, .. }
            | Self::MailAlias { client_id, .. }
            | Self::MailBroadcast { client_id, .. }
            | Self::RichFocusToggle { client_id, .. }
            | Self::RichInput { client_id, .. }
            | Self::CopySemantic { client_id, .. }
            | Self::SetSyncInput { client_id, .. } => Some(*client_id),
            Self::Split { client_id, .. }
            | Self::Close { client_id, .. }
            | Self::MovePane { client_id, .. }
            | Self::Resize { client_id, .. } => *client_id,
            Self::ServerInfo { .. }
            | Self::Ping { .. }
            | Self::Snapshot { .. }
            | Self::Events { .. }
            | Self::SuggestFocus { .. }
            | Self::RegisterClient { .. }
            | Self::SessionNames { .. }
            | Self::NameSession { .. }
            | Self::CreateSession { .. }
            | Self::TransferSpaceSessions { .. }
            | Self::TransferSpacePane { .. }
            | Self::DestroySession { .. }
            | Self::CreateWindow { .. }
            | Self::DestroyWindow { .. }
            | Self::RenameWindow { .. }
            | Self::ApplyArrangement { .. }
            | Self::SetPaneStatus { .. }
            | Self::RenamePane { .. } => None,
        }
    }
}

/// A local viewer's input intent. Viewer identity and application bindings are
/// deliberately absent from this same-user wire shape; the mux owns both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RichInputKind {
    Key {
        key: String,
        #[serde(default)]
        modifiers: u8,
    },
    Pointer {
        phase: RichPointerPhase,
        row: u16,
        col: u16,
    },
    Scroll {
        row: u16,
        col: u16,
        delta: i16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RichPointerPhase {
    Press,
    Move,
    Release,
}

/// Response envelope; protocol errors remain machine-readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub version: u16,
    pub request_id: u64,
    #[serde(flatten)]
    pub body: ControlResponseBody,
}

/// How a queued control reply relates to the request we are waiting for
/// (PT-286). A stale id is an earlier reply that was never consumed.
/// An ahead id skipped the awaited one and is a protocol error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlIdMatch {
    Awaited,
    Stale,
    Ahead,
}

/// Give up after this many lower-id replies while waiting for one request.
/// The live attach failure was two behind; this bound fails a broken
/// stream instead of spinning.
pub const CONTROL_STALE_SKIP_MAX: u32 = 32;

pub fn classify_control_request_id(got: u64, awaited: u64) -> ControlIdMatch {
    match got.cmp(&awaited) {
        std::cmp::Ordering::Equal => ControlIdMatch::Awaited,
        std::cmp::Ordering::Less => ControlIdMatch::Stale,
        std::cmp::Ordering::Greater => ControlIdMatch::Ahead,
    }
}

/// Count one skipped stale reply. `None` means the bound was exceeded.
pub fn next_stale_skip(skipped: u32) -> Option<u32> {
    let next = skipped.saturating_add(1);
    (next <= CONTROL_STALE_SKIP_MAX).then_some(next)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ControlResponseBody {
    Ok { response: ControlResponseData },
    Error { error: ControlError },
}

/// One `MailWho` presence entry: a live agent-bound session.
/// No seat addresses — `N@G` is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailPeer {
    /// The session's bound agent id.
    pub agent_id: String,
    /// Session name.
    pub session: String,
    /// At least one pane in the session has a live child process.
    pub pane_live: bool,
    /// Aliases bound to this agent.
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponseData {
    ServerInfo {
        package_version: String,
        pid: u32,
    },
    AttentionRequests {
        requests: Vec<crate::team_attention::AttentionRequest>,
    },
    SessionNames {
        names: Vec<String>,
    },
    Pong,
    Snapshot {
        snapshot: Snapshot,
    },
    Events {
        batch: EventBatch,
    },
    /// One SubscribePane stream frame. `done` ends the long-lived response.
    PaneSubscribe {
        pane_id: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        gap: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<PaneStyled>,
        events: Vec<PaneLogFrame>,
        through_seq: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        done: bool,
    },
    Mutation {
        ack: MutationAck,
    },
    ClientRegistered {
        client_id: u64,
    },
    /// Lease mutation result.
    Lease {
        pane_id: u64,
        controller_id: Option<u64>,
        previous_controller_id: Option<u64>,
    },
    /// Multiple leases cleared (disconnect).
    LeasesReleased {
        client_id: u64,
        pane_ids: Vec<u64>,
    },
    /// Input was queued to the live server-owned pane PTY.
    WriteQueued {
        pane_id: u64,
        nbytes: usize,
    },
    /// Queue receipt, not an application acknowledgement. A partial result
    /// must not be retried automatically.
    PaneWriteResult {
        pane_id: u64,
        child_pid: u32,
        nbytes: usize,
        total_bytes: usize,
        complete: bool,
        submit: PaneWriteSubmit,
        error: Option<ControlError>,
    },
    /// Latest visible server-owned emulator state for one pane.
    PaneContent {
        content: PaneContent,
    },
    /// Text projection plus per-line style runs.
    PaneStyled {
        content: PaneStyled,
    },
    /// Session create or client-local switch result.
    Session {
        session_id: u64,
        name: String,
        window_id: u64,
        pane_id: u64,
    },
    /// Server accepted termination; written before teardown (control protocol).
    ShutdownAccepted,
    /// Window create, destroy ack, or client-local switch result.
    Window {
        session_id: u64,
        window_id: u64,
        pane_id: u64,
        title: String,
    },
    /// Mail attention + input ledger after Set/Clear/ReportFocus.
    MailAttention {
        pane_id: u64,
        attention: Option<MailAttentionState>,
        ledger: PaneInputLedger,
    },
    /// Gated inject result. `wrote`/`stuck` are attempts; defer is not.
    MailInject {
        pane_id: u64,
        queue_rev: u64,
        outcome: MailInjectOutcome,
        nbytes: usize,
        ledger: PaneInputLedger,
    },
    /// Agent identity bound to this connection's client.
    MailSeated {
        agent: String,
    },
    /// Letter accepted for delivery.
    MailSent {
        id: String,
        /// Recipient's open depth after this send (depth readback).
        depth: u32,
    },
    /// Claim result: letters now held by this agent, oldest first.
    MailLetters {
        letters: Vec<crate::mailbox::Letter>,
    },
    /// Commit result.
    MailCommitted {
        committed: u32,
    },
    /// Release result.
    MailReleased {
        released: u32,
    },
    /// Inbox peek, or `MailWait` reply (`open > 0` means mail arrived).
    MailDepth {
        open: u32,
        held: u32,
    },
    /// Presence reply: every live agent-bound session.
    MailPeers {
        peers: Vec<MailPeer>,
    },
    /// Alias registered (or already held by this agent).
    MailAliased {
        name: String,
        agent: String,
    },
    /// Broadcast fanned out. `recipients` are agent ids only.
    MailBroadcasted {
        delivered: u32,
        recipients: Vec<String>,
    },
    /// Mail verb refused in English. Not a protocol error:
    /// unknown identity, bad alias, or an invalid recipient charset.
    MailRefused {
        reason: String,
    },
    /// Attach C-S-G / Esc rich-focus result. `granted` is false when
    /// the flag is off or no region was eligible.
    RichFocus {
        pane_id: u64,
        granted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region_id: Option<u32>,
        /// True for workspace input, false for the legacy
        /// attachment focus path.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        structured: bool,
    },
    /// A structured intent was accepted. `delivered` is false for a motion or
    /// canceled release that intentionally creates no application event.
    RichInput {
        pane_id: u64,
        delivered: bool,
    },
    SemanticCopy {
        pane_id: u64,
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
}

impl ControlResponse {
    fn ok(request_id: u64, response: ControlResponseData) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body: ControlResponseBody::Ok { response },
        }
    }

    fn error(request_id: u64, error: ControlError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body: ControlResponseBody::Error { error },
        }
    }
}

/// Stable error vocabulary for clients and test harnesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    InvalidJson,
    FrameTooLarge,
    InvalidRequest,
    IncompatibleVersion,
    StaleRequestId,
    StaleId,
    StaleSequence,
    EventGap,
    SnapshotRequired,
    TooSmall,
    LastPane,
    SequenceExhausted,
    /// Writable controller lease held by another client.
    LeaseHeld,
    /// Caller is not the writable controller (observer while a holder exists).
    NotController,
    /// Lease-free write refused: the pane holds unsubmitted input (a partial
    /// line, `ledger.dirty_input`). Take the lease — `pmux send --force` —
    /// to write over it (PT-140).
    InputDirty,
    /// Intentional text refused while typing or streaming output.
    InputBusy,
    /// Authority is valid, but no lossless control-to-PTY route is bound yet.
    InputRouteUnavailable,
    /// The bounded PTY input queue is full; caller may retry later.
    Backpressure,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    pub code: ControlErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resnapshot_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_available_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_sequence: Option<u64>,
    /// Set on `LeaseHeld` and `NotController`. Wire-additive; old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<u64>,
}

impl ControlError {
    fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            resnapshot_required: false,
            oldest_available_sequence: None,
            current_sequence: None,
            holder: None,
        }
    }

    fn with_holder(mut self, holder: u64) -> Self {
        self.holder = Some(holder);
        self
    }

    fn resnapshot(
        code: ControlErrorCode,
        message: impl Into<String>,
        oldest_available_sequence: u64,
        current_sequence: u64,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            resnapshot_required: true,
            oldest_available_sequence: Some(oldest_available_sequence),
            current_sequence: Some(current_sequence),
            holder: None,
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ControlError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub sequence: u64,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub id: u64,
    pub title: String,
    pub bounds: WindowBounds,
    pub layout: LayoutSnapshot,
    pub panes: Vec<PaneSnapshot>,
    #[serde(default)]
    pub sync_input: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutSnapshot {
    Leaf {
        pane_id: u64,
    },
    Split {
        axis: AxisWire,
        ratio: f64,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    /// Latest intentional input queue receipt, never proof of execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_write: Option<PaneWriteReceipt>,
    pub id: u64,
    pub title: String,
    /// True after `pmux rename-pane` with a non-empty title (PT-230).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_pinned: bool,
    pub controller_id: Option<u64>,
    pub geometry: PaneGeometry,
    pub spawn: Option<SpawnSpec>,
    /// Live PTY child, when the pane is running. Additive; old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailAttentionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Latest generic agent-attention message for this pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
    /// Last in-process mail doorbell attempt for this pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_inject: Option<MailInjectDiagnostic>,
    pub ledger: PaneInputLedger,
    /// Client that last set this window's size (PT-202). Shared by every
    /// pane in the window. Old clients ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_owner: Option<SizeOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneWriteReceipt {
    pub child_pid: u32,
    pub queued_at_ms: u64,
    pub nbytes: usize,
    pub total_bytes: usize,
    pub complete: bool,
    pub submit: PaneWriteSubmit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGeometry {
    pub pane_id: u64,
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
}

/// Bounded visible-grid projection used by local attach clients.
///
/// The authoritative full emulator and scrollback remain server-owned.
/// exposes enough state for attach clients to establish a live view without
/// transferring process or emulator ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneContent {
    pub pane_id: u64,
    pub revision: u64,
    pub cols: u32,
    pub rows: u32,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub cursor_visible: bool,
    pub alt_active: bool,
    pub child_alive: bool,
    /// Diagnostic process identity for local detach durability checks.
    pub child_pid: Option<u32>,
    pub lines: Vec<String>,
    /// Child DECSCUSR shape. `None` on old snapshots — attach must not emit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_shape: Option<CursorShapeWire>,
}

/// Wire DECSCUSR shape. Ghostty/xterm: block, underline, bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShapeWire {
    Block,
    Underline,
    Bar,
}

impl CursorShapeWire {
    pub fn from_emulator(shape: prismattyc_emulator::CursorShape) -> Self {
        match shape {
            prismattyc_emulator::CursorShape::Block => Self::Block,
            prismattyc_emulator::CursorShape::Underline => Self::Underline,
            prismattyc_emulator::CursorShape::Bar => Self::Bar,
        }
    }

    /// Steady `CSI Ps SP q` for the outer terminal.
    pub const fn decscusr_bytes(self) -> &'static [u8] {
        match self {
            Self::Block => b"\x1b[2 q",
            Self::Underline => b"\x1b[4 q",
            Self::Bar => b"\x1b[6 q",
        }
    }
}

/// Wire color for styled attach. Compact tag so RLE stays small.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ColorWire {
    #[default]
    Default,
    Ansi {
        n: u8,
    },
    Indexed {
        n: u8,
    },
    Rgb {
        r: u8,
        g: u8,
        b: u8,
    },
}

impl ColorWire {
    fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    pub fn from_core(color: prismattyc_core::Color) -> Self {
        match color {
            prismattyc_core::Color::Default => Self::Default,
            prismattyc_core::Color::Ansi(n) => Self::Ansi { n },
            prismattyc_core::Color::Indexed(n) => Self::Indexed { n },
            prismattyc_core::Color::Rgb { r, g, b } => Self::Rgb { r, g, b },
        }
    }
}

/// One run of identically styled text on a visible row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleRun {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inverse: bool,
    #[serde(default, skip_serializing_if = "ColorWire::is_default")]
    pub fg: ColorWire,
    #[serde(default, skip_serializing_if = "ColorWire::is_default")]
    pub bg: ColorWire,
}

impl StyleRun {
    fn key(&self) -> (bool, bool, bool, bool, ColorWire, ColorWire) {
        (
            self.bold,
            self.italic,
            self.underline,
            self.inverse,
            self.fg,
            self.bg,
        )
    }

    fn from_cell(cell: prismattyc_core::CellView<'_>) -> Self {
        let style = cell.style;
        let mut text = String::new();
        cell.write_grapheme_into(&mut text);
        Self {
            text,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            inverse: style.inverse,
            fg: ColorWire::from_core(style.foreground),
            bg: ColorWire::from_core(style.background),
        }
    }
}

/// Rich overlay kind on the attach snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
    CellRect,
    Viewport,
}

/// One styled run inside a rich overlay. Palette indexes match the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayRun {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inverse: bool,
}

/// One granted rich overlay projected for attach paint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneOverlay {
    pub id: u32,
    pub kind: OverlayKind,
    pub row: i32,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<OverlayRun>,
}

/// `PaneContent` plus RLE style runs, one inner vec per visible row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneStyled {
    #[serde(flatten)]
    pub content: PaneContent,
    pub runs: Vec<Vec<StyleRun>>,
    /// Reserved workspace lines above the guest PTY. Empty/omitted preserves
    /// the pre-0.3 JSON contract and gives the guest the full pane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace: Vec<String>,
    /// Effective rows back from the live tail after clamp.
    /// Absent on pre-servers — clients must hide scroll mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_offset: Option<u32>,
    /// Server `max_view_scroll` for this pane (0 on the alternate screen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_view_scroll: Option<u32>,
    /// Child DECSET 1000/1002/1003 is on. Absent on older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_mouse_tracking: Option<bool>,
    /// Child prefers SGR mouse encoding (DECSET 1006).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_mouse_sgr: Option<bool>,
    /// Experimental-rich overlays. Empty and omitted when the flag is off
    /// or nothing is granted — flag-off JSON stays byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<PaneOverlay>,
    /// True when the pane PTY was spawned with `--experimental-rich`.
    /// Omitted when false so flag-off JSON stays byte-identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub experimental_rich: bool,
    /// Host-granted `input.rich_focus` region, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rich_focus_id: Option<u32>,
    /// The viewer-local focus is a workspace action rather than the
    /// legacy attachment focus path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub structured_focus: bool,
    /// Peek of the next unacked copy for this client. Ack with CopySemantic.seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_clipboard: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_clipboard_seq: Option<u64>,
    /// Inverse runs in dock cell coordinates (selected diagnostic only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_inverse: Vec<WorkspaceInverseRun>,
    /// Semantic status runs in dock coordinates. ANSI entries are theme
    /// tokens resolved by the outer terminal; RGB never comes from the app.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_styles: Vec<WorkspaceStyleRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInverseRun {
    pub row: u32,
    pub col: u32,
    pub cols: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStyleRun {
    pub row: u32,
    pub col: u32,
    pub cols: u32,
    #[serde(default, skip_serializing_if = "ColorWire::is_default")]
    pub fg: ColorWire,
}

/// Collapse adjacent same-style cells into runs. Wide continuations add no text.
pub fn rle_style_runs<'a>(
    cells: impl IntoIterator<Item = prismattyc_core::CellView<'a>>,
) -> Vec<StyleRun> {
    let mut runs: Vec<StyleRun> = Vec::new();
    for cell in cells {
        if cell.wide_cont {
            continue;
        }
        let next = StyleRun::from_cell(cell);
        if next.text.is_empty() {
            continue;
        }
        if let Some(last) = runs.last_mut() {
            if last.key() == next.key() {
                last.text.push_str(&next.text);
                continue;
            }
        }
        runs.push(next);
    }
    runs
}

impl StyleRun {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            fg: ColorWire::Default,
            bg: ColorWire::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub sequence: u64,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    PaneSplit {
        window_id: u64,
        target_pane_id: u64,
        new_pane_id: u64,
        axis: AxisWire,
        ratio: f64,
        spawn: SpawnSpec,
        geometry: Vec<PaneGeometry>,
    },
    PaneClosed {
        window_id: u64,
        pane_id: u64,
        suggested_focus_id: u64,
        geometry: Vec<PaneGeometry>,
    },
    GeometryChanged {
        window_id: u64,
        bounds: WindowBounds,
        geometry: Vec<PaneGeometry>,
    },
    /// Who owns the window size (PT-202). Not a geometry change.
    SizeOwnerChanged {
        window_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<SizeOwner>,
    },
    FocusSuggested {
        window_id: u64,
        pane_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Operator-reported pane focus. Not server topology.
    FocusReported {
        client_id: u64,
        window_id: u64,
        pane_id: u64,
    },
    /// Sticky mail attention changed. `depth == 0` means cleared.
    MailAttentionChanged {
        pane_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gen: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_rev: Option<u64>,
        depth: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<MailWake>,
    },
    /// Generic agent attention raised for a pane.
    PaneAttention {
        pane_id: u64,
        message: String,
    },
    /// Generic agent attention cleared after successful pane input.
    PaneAttentionCleared {
        pane_id: u64,
    },
    LeaseChanged {
        pane_id: u64,
        controller_id: Option<u64>,
        previous_controller_id: Option<u64>,
    },
    SpaceOwnershipChanged {
        session_ids: Vec<u64>,
        from: Option<String>,
        to: Option<String>,
    },
    SessionCreated {
        session_id: u64,
        name: String,
        /// `0` when the session is headless (no window). Snapshot never
        /// contains window 0; clients must not chase it.
        window_id: u64,
        /// `0` when the session is headless (no pane).
        pane_id: u64,
        bounds: WindowBounds,
        spawn: SpawnSpec,
    },
    SessionNamed {
        session_id: u64,
        name: String,
    },
    SessionSwitched {
        client_id: u64,
        session_id: u64,
        window_id: u64,
        pane_id: u64,
    },
    SessionDestroyed {
        session_id: u64,
        name: String,
    },
    /// Coalesced live-pane output (at most one ring entry per pane).
    OutputActivity {
        pane_id: u64,
        revision: u64,
        child_alive: bool,
    },
    WindowCreated {
        session_id: u64,
        window_id: u64,
        pane_id: u64,
        title: String,
        bounds: WindowBounds,
        spawn: SpawnSpec,
    },
    WindowDestroyed {
        session_id: u64,
        window_id: u64,
        session_destroyed: bool,
    },
    WindowSwitched {
        client_id: u64,
        session_id: u64,
        window_id: u64,
        pane_id: u64,
    },
    WindowRenamed {
        window_id: u64,
        title: String,
    },
    PaneStatusChanged {
        pane_id: u64,
        status: Option<String>,
    },
    /// A pane title changed (PT-128); empty means cleared.
    PaneRenamed {
        pane_id: u64,
        title: String,
    },
    SyncInputChanged {
        window_id: u64,
        enabled: bool,
    },
    /// Single two-window move. Never a Closed+Split pair (tabs-design).
    PaneMoved {
        from_window_id: u64,
        to_window_id: u64,
        pane_id: u64,
        source_suggested_focus_id: Option<u64>,
        geometry: Vec<PaneGeometry>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventBatch {
    pub after_sequence: u64,
    /// Last sequence included in this batch (or `after_sequence` when empty).
    pub through_sequence: u64,
    pub current_sequence: u64,
    pub has_more: bool,
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationAck {
    pub sequence: u64,
}

/// Capture at most one pane per maintenance tick, releasing the control lock
/// between panes. Restore keys are revalidated before submission; mutations
/// after submission are captured on the next normal cadence.
struct CheckpointCapture {
    keys: std::collections::VecDeque<(u64, String, usize)>,
    restore_keys: Vec<(u64, String, usize)>,
    panes: Vec<PendingPane>,
    captured: std::collections::HashMap<u64, (u64, String, usize)>,
    mark: u64,
}

/// Display-free owner of control-visible mux topology and event history.
///
/// `new_live` additionally binds one server-owned PTY/emulator per pane. The
/// windowed client remains a compositor, never the process-lifetime owner.
pub struct ControlPlane {
    shutdown_pending: bool,
    domain: Domain,
    active_clients: HashSet<ClientId>,
    client_spaces: HashMap<ClientId, String>,
    /// Host-minted identities, never accepted from the control client.
    viewer_ids: HashMap<ClientId, ViewerId>,
    /// Client-local selected session (PRD §2.8.3). Topology is not mutated.
    client_views: HashMap<ClientId, SessionId>,
    /// Client-local selected window (tab). Topology is not mutated.
    client_window_views: HashMap<ClientId, WindowId>,
    bounds: HashMap<u64, WindowBounds>,
    spawn: HashMap<u64, SpawnSpec>,
    sequence: u64,
    events: VecDeque<EventEnvelope>,
    event_capacity: usize,
    live: Option<LiveRuntime>,
    /// Wakes SubscribePane waiters. Shared with `LiveRuntime`.
    pane_log_watch: std::sync::Arc<PaneLogWatch>,
    /// Bind path stamped into each guest as `PMUX_SOCKET`.
    /// Known before `ControlServer::bind`; the file may not exist yet.
    mux_socket: Option<PathBuf>,
    /// Optional Hive supervisor sink (ADR-0037). When set, liveness-edge notices for
    /// Hive-seated panes are best-effort enqueued for `supervisor.cell_exited`.
    supervisor: Option<SupervisorSink>,
    /// Sticky mail attention keyed by pane.
    mail_attention: HashMap<u64, MailAttentionState>,
    /// Latest generic agent-attention message keyed by pane.
    attention: HashMap<u64, String>,
    team_attention: HashMap<u64, crate::team_attention::AttentionRequest>,
    /// Guest-set status text keyed by pane.
    pane_status: HashMap<u64, String>,
    /// Last applied `queue_rev` per pane. Survives Clear so a delayed older
    /// Set cannot resurrect attention.
    mail_rev: HashMap<u64, u64>,
    /// Last successful controller write per pane (inject ledger).
    write_ledger: HashMap<u64, (u64, bool)>,
    /// Last WritePane timestamp per pane (unix ms). Survives lease release.
    last_input_at_ms: HashMap<u64, u64>,
    pane_write_receipts: HashMap<u64, PaneWriteReceipt>,
    /// Tests run `/bin/sh` children; this stands in for the agent classifier.
    #[cfg(test)]
    inject_agent_override: Option<crate::InjectAgent>,
    /// Last OutputActivity timestamp per pane (unix ms).
    last_output_at_ms: HashMap<u64, u64>,
    /// PTY bytes per pane in the quiet window `(unix_ms, nbytes)`.
    output_bytes: HashMap<u64, VecDeque<(u64, usize)>>,
    /// Last reported focused pane per client. Not topology.
    client_pane_focus: HashMap<ClientId, u64>,
    /// Last Resize size per client per window.
    client_viewports: HashMap<ClientId, HashMap<u64, ClientViewport>>,
    /// Last host-chosen size per window (seeded from bootstrap bounds).
    /// Applied bounds live in `bounds`; do not overwrite this with a
    /// remote-applied echo.
    host_viewports: HashMap<u64, ClientViewport>,
    /// Most recently active client per window (PT-200).
    latest_client: HashMap<u64, ClientId>,
    /// Clients that sent `Resize { host: true }`.
    host_clients: HashSet<ClientId>,
    /// `[mux] remote_size` policy.
    remote_size: RemoteSizePolicy,
    /// Per-pane inject hide/stuck watermark.
    mail_inject: HashMap<u64, MailInjectRecord>,
    /// Last inject attempt for operator diagnostics.
    mail_inject_last: HashMap<u64, MailInjectDiagnostic>,
    /// Delayed doorbell retries for unread native mail attention.
    mail_nudges: HashMap<u64, MailNudge>,
    /// Durable mailbox. In-memory until pmuxd binds the
    /// real path via [`ControlPlane::set_mail_store`].
    mail_store: crate::mailbox::Store,
    /// Mail-arrival signal shared with client threads. Ringed
    /// after the plane lock drops, never while holding it.
    mail_watch: std::sync::Arc<crate::mailbox::MailboxWatch>,
    /// Daemon-lifetime aliases: shorthand → agent id.
    mail_aliases: HashMap<String, String>,
    /// Connection client → asserted agent identity.
    client_agents: HashMap<ClientId, String>,
    /// Internal identity the in-process doorbell uses for attention and
    /// inject lease bookkeeping. Never registered on the wire,
    /// never in `active_clients`.
    doorbell_client: ClientId,
    /// Instance-keyed persist file. `None` disables persist (tests, ad-hoc `--socket`).
    pane_log_path: Option<PathBuf>,
    last_pane_log_persist: Option<Instant>,
    pane_log_persist_mark: u64,
    pane_log_worker: Option<PersistWorker>,
    pane_log_pending_mark: Option<u64>,
    pane_log_captured: std::collections::HashMap<u64, (u64, String, usize)>,
    pane_log_capture: Option<CheckpointCapture>,
}

impl ControlPlane {
    pub fn new(
        domain: Domain,
        window_bounds: impl IntoIterator<Item = WindowBounds>,
        event_capacity: Option<usize>,
    ) -> Result<Self, ControlError> {
        let event_capacity = event_capacity.unwrap_or(DEFAULT_EVENT_CAPACITY);
        if !(1..=MAX_EVENT_CAPACITY).contains(&event_capacity) {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("event capacity must be in 1..={MAX_EVENT_CAPACITY}"),
            ));
        }
        let mut bounds = HashMap::new();
        for bound in window_bounds {
            validate_bounds(bound)?;
            if domain.window(window_id(bound.window_id)?).is_none() {
                return Err(stale_id("window", bound.window_id));
            }
            if bounds.insert(bound.window_id, bound).is_some() {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("duplicate bounds for window {}", bound.window_id),
                ));
            }
        }
        for session in domain.sessions() {
            for id in &session.windows {
                let bound = bounds
                    .get(&id.get())
                    .copied()
                    .ok_or_else(|| stale_id("window geometry", id.get()))?;
                let layout = &domain.window(*id).expect("session window exists").layout;
                layout_to_rects(layout, rect_for(bound), DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS)
                    .map_err(control_geometry_error)?;
            }
        }
        let mut domain = domain;
        let doorbell_client = domain.mint_client().map_err(control_domain_error)?;
        let host_viewports = bounds
            .values()
            .map(|bound| {
                (
                    bound.window_id,
                    ClientViewport {
                        cols: bound.cols,
                        rows: bound.rows,
                        cell_width_px: None,
                        cell_height_px: None,
                    },
                )
            })
            .collect();
        Ok(Self {
            shutdown_pending: false,
            domain,
            active_clients: HashSet::new(),
            client_spaces: HashMap::new(),
            viewer_ids: HashMap::new(),
            client_views: HashMap::new(),
            client_window_views: HashMap::new(),
            bounds,
            spawn: HashMap::new(),
            sequence: 0,
            events: VecDeque::with_capacity(event_capacity),
            event_capacity,
            live: None,
            pane_log_watch: PaneLogWatch::new(),
            mux_socket: None,
            supervisor: None,
            mail_attention: HashMap::new(),
            attention: HashMap::new(),
            team_attention: HashMap::new(),
            pane_status: HashMap::new(),
            mail_rev: HashMap::new(),
            write_ledger: HashMap::new(),
            last_input_at_ms: HashMap::new(),
            pane_write_receipts: HashMap::new(),
            #[cfg(test)]
            inject_agent_override: None,
            last_output_at_ms: HashMap::new(),
            output_bytes: HashMap::new(),
            client_pane_focus: HashMap::new(),
            client_viewports: HashMap::new(),
            host_viewports,
            latest_client: HashMap::new(),
            host_clients: HashSet::new(),
            remote_size: RemoteSizePolicy::Latest,
            mail_inject: HashMap::new(),
            mail_inject_last: HashMap::new(),
            mail_nudges: HashMap::new(),
            mail_store: crate::mailbox::Store::open_in_memory().map_err(|error| {
                ControlError::new(
                    ControlErrorCode::Internal,
                    format!("open in-memory mailbox: {error}"),
                )
            })?,
            mail_watch: std::sync::Arc::new(crate::mailbox::MailboxWatch::new()),
            mail_aliases: HashMap::new(),
            client_agents: HashMap::new(),
            doorbell_client,
            pane_log_path: None,
            last_pane_log_persist: None,
            pane_log_persist_mark: 0,
            pane_log_worker: None,
            pane_log_pending_mark: None,
            pane_log_captured: std::collections::HashMap::new(),
            pane_log_capture: None,
        })
    }

    /// `[mux] remote_size` policy. Default is [`RemoteSizePolicy::Latest`].
    pub fn set_remote_size(&mut self, policy: RemoteSizePolicy) {
        self.remote_size = policy;
    }

    /// Build the long-lived Phase 2B server state.
    ///
    /// Every existing pane must have one structured spawn specification. All
    /// children are started before the plane is returned, so callers never
    /// publish a socket for a partially initialized domain.
    ///
    /// `mux_socket` is the intended bind path. Guests receive it as
    /// `PMUX_SOCKET` at spawn, before the listener exists.
    ///
    /// When `HIVE_SOCKET` is set in the server environment, a supervisor sink is
    /// armed: Hive-seated panes (those whose `SpawnSpec.env` carries `HIVE_CELL`)
    /// will best-effort emit `supervisor.cell_exited` on the child liveness edge
    /// (ADR-0037 §3). Absent `HIVE_SOCKET`, no supervisor traffic is attempted.
    pub fn new_live(
        domain: Domain,
        window_bounds: impl IntoIterator<Item = WindowBounds>,
        event_capacity: Option<usize>,
        pane_spawns: impl IntoIterator<Item = (u64, SpawnSpec)>,
        mux_socket: Option<PathBuf>,
    ) -> Result<Self, ControlError> {
        let mut plane = Self::new(domain, window_bounds, event_capacity)?;
        if let Some(ref path) = mux_socket {
            if !path.is_absolute() {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    "mux socket must be an absolute path",
                ));
            }
            if path.to_str().is_none() {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    "mux socket must be UTF-8",
                ));
            }
        }
        plane.mux_socket = mux_socket;
        let mut spawns = HashMap::new();
        for (pane_raw, spawn) in pane_spawns {
            spawn.validate()?;
            let pane = pane_id(pane_raw)?;
            if plane.domain.pane(pane).is_none() {
                return Err(stale_id("pane", pane_raw));
            }
            if spawns.insert(pane_raw, spawn).is_some() {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("duplicate spawn spec for pane {pane_raw}"),
                ));
            }
        }
        let geometry = plane.all_geometry()?;
        if geometry
            .iter()
            .any(|pane| !spawns.contains_key(&pane.pane_id))
        {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "every live pane requires one spawn spec",
            ));
        }
        let live = LiveRuntime::spawn_with_watch(
            &geometry,
            &spawns,
            plane.mux_socket.clone(),
            std::sync::Arc::clone(&plane.pane_log_watch),
        )
        .map_err(live_internal_error)?;
        plane.spawn = spawns;
        plane.live = Some(live);
        plane.supervisor = hive_supervisor_from_env();
        Ok(plane)
    }

    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    pub(crate) fn pane_log_watch(&self) -> std::sync::Arc<PaneLogWatch> {
        std::sync::Arc::clone(&self.pane_log_watch)
    }

    #[cfg(test)]
    pub(crate) fn shrink_pane_log_for_test(&mut self, pane_id: u64, cap: usize) {
        if let Some(live) = self.live.as_mut() {
            live.shrink_pane_log(pane_id, cap);
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_for_test(&mut self) {
        self.drain_live();
    }

    #[cfg(test)]
    pub(crate) fn stamp_last_output_at_ms(&mut self, pane_raw: u64, ms: u64) {
        self.last_output_at_ms.insert(pane_raw, ms);
    }

    #[cfg(test)]
    pub(crate) fn stamp_last_input_at_ms(&mut self, pane_raw: u64, ms: u64) {
        self.last_input_at_ms.insert(pane_raw, ms);
    }

    #[cfg(test)]
    pub(crate) fn stamp_write_ledger(&mut self, pane_raw: u64, at_ms: u64, ended_cr: bool) {
        self.write_ledger.insert(pane_raw, (at_ms, ended_cr));
    }

    #[cfg(test)]
    pub(crate) fn record_output_bytes(&mut self, pane_raw: u64, nbytes: usize) {
        self.note_output_bytes(pane_raw, nbytes, now_unix_ms());
    }

    #[cfg(test)]
    pub(crate) fn clear_output_bytes(&mut self, pane_raw: u64) {
        self.output_bytes.remove(&pane_raw);
    }

    #[cfg(test)]
    pub(crate) fn mail_inject_writes(&self, pane_raw: u64) -> u32 {
        self.mail_inject
            .get(&pane_raw)
            .map(|record| record.writes)
            .unwrap_or(0)
    }

    pub(crate) fn retry_mail_nudges(&mut self) {
        let now = now_unix_ms();
        let due: Vec<(u64, MailNudge)> = self
            .mail_nudges
            .iter()
            .filter_map(|(&pane_raw, &nudge)| (nudge.due_at_ms <= now).then_some((pane_raw, nudge)))
            .collect();
        for (pane_raw, nudge) in due {
            self.mail_nudges.remove(&pane_raw);
            let current = self.mail_attention.get(&pane_raw);
            if current.is_none_or(|attention| {
                attention.depth == 0
                    || attention.queue_rev != nudge.queue_rev
                    || attention.wake != Some(MailWake::Armed)
            }) {
                continue;
            }
            let result = self.gated_mail_inject(
                self.doorbell_client,
                pane_raw,
                nudge.queue_rev,
                1,
                nudge.verification,
            );
            let retry = match result {
                Ok(ControlResponseData::MailInject { outcome, .. }) => {
                    matches!(
                        outcome,
                        MailInjectOutcome::DeferredBusy
                            | MailInjectOutcome::DeferredDirty
                            | MailInjectOutcome::DeferredTyping
                            | MailInjectOutcome::DeferredNoAgent
                            | MailInjectOutcome::DeferredLease
                            | MailInjectOutcome::DeferredFocused
                    ) || (nudge.verification && outcome == MailInjectOutcome::DeferredHide)
                }
                Err(_) => true,
                Ok(_) => false,
            };
            let still_lit = self.mail_attention.get(&pane_raw).is_some_and(|attention| {
                attention.depth > 0
                    && attention.queue_rev == nudge.queue_rev
                    && attention.wake == Some(MailWake::Armed)
            });
            if retry && still_lit {
                let due_at_ms = now.saturating_add(MAIL_INJECT_NUDGE_DELAY_MS);
                self.mail_nudges.insert(
                    pane_raw,
                    MailNudge {
                        queue_rev: nudge.queue_rev,
                        due_at_ms,
                        verification: nudge.verification,
                    },
                );
            }
        }
    }

    fn drain_live(&mut self) {
        let (notices, activity) = match self.live.as_mut() {
            Some(live) => live.drain(),
            None => return,
        };
        if let Some(sink) = self.supervisor.as_ref() {
            for notice in notices {
                sink.try_emit(notice);
            }
        }
        for tick in activity {
            if let Some(message) = tick.attention {
                self.attention.insert(tick.pane_id, message.clone());
                let _ = self.emit(Event::PaneAttention {
                    pane_id: tick.pane_id,
                    message,
                });
            }
            self.emit_output_activity(tick.pane_id, tick.revision, tick.child_alive, tick.nbytes);
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn require_client_space(&self, client: ClientId, pane: PaneId) -> Result<(), ControlError> {
        let Some(expected) = self.client_spaces.get(&client) else {
            return Ok(());
        };
        let window = self
            .domain
            .pane_owner(pane)
            .ok_or_else(|| stale_id("pane", pane.get()))?;
        let owner = self
            .domain
            .sessions()
            .find(|s| s.windows.contains(&window))
            .and_then(|s| s.space_id.as_ref());
        if owner != Some(expected) {
            return Err(ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "pane moved to another Space; refresh this view",
            ));
        }
        Ok(())
    }

    fn require_active_client(&self, raw: u64) -> Result<ClientId, ControlError> {
        let client = client_id(raw)?;
        if self.active_clients.contains(&client) {
            Ok(client)
        } else {
            Err(stale_id("client", raw))
        }
    }

    fn pane_is_focused(&self, pane_raw: u64) -> bool {
        self.client_pane_focus
            .values()
            .any(|focused| *focused == pane_raw)
    }

    fn pane_ledger(&self, pane_raw: u64) -> PaneInputLedger {
        let controller_id = pane_id(pane_raw)
            .ok()
            .and_then(|pane| self.domain.pane(pane))
            .and_then(|pane| pane.controller)
            .map(|id| id.get());
        let (last_controller_write_at_ms, last_write_ended_with_cr) = self
            .write_ledger
            .get(&pane_raw)
            .copied()
            .map(|(at, cr)| (Some(at), cr))
            .unwrap_or((None, false));
        let last_output_at_ms = self.last_output_at_ms.get(&pane_raw).copied();
        let last_input_at_ms = self.last_input_at_ms.get(&pane_raw).copied();
        let now = now_unix_ms();
        PaneInputLedger {
            last_output_at_ms,
            focused: self.pane_is_focused(pane_raw),
            controller_id,
            last_controller_write_at_ms,
            last_write_ended_with_cr,
            // Lease release does not clear this. A 750 ms idle is not a CR.
            dirty_input: dirty_input_still_open(
                last_controller_write_at_ms,
                last_write_ended_with_cr,
                last_output_at_ms,
                now,
            ),
            last_input_at_ms,
        }
    }

    fn mail_attention_response(&self, pane_raw: u64) -> ControlResponseData {
        ControlResponseData::MailAttention {
            pane_id: pane_raw,
            attention: self.mail_attention.get(&pane_raw).cloned(),
            ledger: self.pane_ledger(pane_raw),
        }
    }

    fn raise_attention(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        message: String,
    ) -> Result<ControlResponseData, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        validate_attention_message(&message)?;
        if self
            .team_attention
            .get(&pane_raw)
            .is_some_and(|request| request.message == message)
        {
            return Ok(ControlResponseData::Mutation {
                ack: MutationAck {
                    sequence: self.sequence,
                },
            });
        }
        // Do not reuse a request revision when a daemon restarts and IDs recycle.
        // Thirteen hex digits also round-trip exactly through JSON number clients.
        let random = crate::new_space_id()
            .map_err(|error| ControlError::new(ControlErrorCode::Internal, error.to_string()))?;
        let revision = u64::from_str_radix(&random[..13], 16)
            .map_err(|error| ControlError::new(ControlErrorCode::Internal, error.to_string()))?;
        self.ensure_sequence_capacity()?;
        self.attention.insert(pane_raw, message.clone());
        let sequence = self.emit(Event::PaneAttention {
            pane_id: pane_raw,
            message: message.clone(),
        });
        self.team_attention.insert(
            pane_raw,
            crate::team_attention::AttentionRequest {
                pane_id: pane_raw,
                revision,
                message,
                source: "explicit attention request".into(),
                raised_at_ms: now_unix_ms(),
                snoozed_until_ms: 0,
            },
        );
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    pub fn set_mail_store(&mut self, store: crate::mailbox::Store) {
        self.mail_store = store;
    }

    fn update_team_attention(
        &mut self,
        client: u64,
        pane: u64,
        revision: u64,
        expected_space: String,
        expected_session: u64,
        action: crate::team_attention::AttentionAction,
    ) -> Result<ControlResponseData, ControlError> {
        self.require_active_client(client)?;
        if !self.snapshot()?.sessions.iter().any(|session| {
            session.id == expected_session
                && session.space_id.as_deref() == Some(expected_space.as_str())
                && session
                    .windows
                    .iter()
                    .flat_map(|window| &window.panes)
                    .any(|p| p.id == pane)
        }) {
            return Err(ControlError::new(
                ControlErrorCode::StaleId,
                "session moved or changed; refresh its owning Space",
            ));
        }
        if self.domain.pane(pane_id(pane)?).is_none() {
            return Err(stale_id("pane", pane));
        }
        let current = self
            .team_attention
            .get(&pane)
            .filter(|r| r.revision == revision)
            .ok_or_else(|| {
                ControlError::new(
                    ControlErrorCode::StaleRequestId,
                    "attention changed; refresh the session details",
                )
            })?;
        match action {
            crate::team_attention::AttentionAction::Resolve => {
                let message = current.message.clone();
                self.ensure_sequence_capacity()?;
                self.team_attention.remove(&pane);
                // A mail notification may have replaced the generic hint.
                if self.attention.get(&pane) == Some(&message) {
                    self.attention.remove(&pane);
                    self.emit(Event::PaneAttentionCleared { pane_id: pane });
                }
            }
            crate::team_attention::AttentionAction::Snooze { seconds } => {
                if !(1..=86_400).contains(&seconds) {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "snooze must be between 1 and 86400 seconds",
                    ));
                }
                self.team_attention.get_mut(&pane).unwrap().snoozed_until_ms =
                    now_unix_ms().saturating_add(u64::from(seconds) * 1000);
            }
        }
        Ok(ControlResponseData::AttentionRequests {
            requests: self.team_attention.values().cloned().collect(),
        })
    }

    /// Enable pane-log persist at `path` and restore if a file is already there.
    pub fn set_pane_log_path(&mut self, path: PathBuf) -> std::io::Result<()> {
        let worker = PersistWorker::start()?;
        self.pane_log_worker.take();
        self.pane_log_pending_mark = None;
        self.pane_log_captured.clear();
        self.pane_log_capture = None;
        self.pane_log_path = Some(path);
        self.restore_pane_logs();
        self.pane_log_worker = Some(worker);
        Ok(())
    }

    pub(crate) fn flush_pane_logs(&mut self) {
        // Shutdown must finish the older write before publishing the final state.
        self.pane_log_worker.take();
        self.pane_log_pending_mark = None;
        self.pane_log_capture = None;
        let Some(path) = self.pane_log_path.clone() else {
            return;
        };
        let keys = self.persist_keys();
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let file = PersistFile::from_panes(
            live.export_persist(&keys, &EmulatorStateCodec),
            ScreenCodec::id(&EmulatorStateCodec),
        );
        if write_persist(&path, &file).is_ok() {
            self.last_pane_log_persist = Some(Instant::now());
            self.pane_log_persist_mark = live.persist_mark(&keys);
        }
    }

    /// `(pane_id, session name, pane_index)` in session order. Restore keys on this.
    fn persist_keys(&self) -> Vec<(u64, String, usize)> {
        let mut keys = Vec::new();
        for session in self.domain.sessions() {
            let mut pane_index = 0usize;
            for window_id in &session.windows {
                let Some(window) = self.domain.window(*window_id) else {
                    continue;
                };
                for pane in window.layout.panes() {
                    keys.push((pane.get(), session.name.clone(), pane_index));
                    pane_index += 1;
                }
            }
        }
        keys
    }

    fn maybe_persist_pane_logs(&mut self) {
        let Some(worker) = self.pane_log_worker.as_ref() else {
            return;
        };
        if let Some((success, compact)) = worker.completed() {
            if let Some(mark) = self.pane_log_pending_mark.take() {
                if success {
                    self.pane_log_persist_mark = mark;
                }
                if !success || compact {
                    self.pane_log_captured.clear();
                }
            }
        }
        if self.pane_log_pending_mark.is_some() {
            return;
        }
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if self.pane_log_capture.is_none() {
            let keys = self.persist_keys();
            let mark = live.persist_mark(&keys);
            if mark == self.pane_log_persist_mark
                || self
                    .last_pane_log_persist
                    .is_some_and(|at| at.elapsed() < PERSIST_CADENCE)
            {
                return;
            }
            self.pane_log_capture = Some(CheckpointCapture {
                keys: keys.clone().into(),
                restore_keys: keys,
                panes: Vec::new(),
                captured: std::collections::HashMap::new(),
                mark,
            });
            self.last_pane_log_persist = Some(Instant::now());
        }
        let Some(path) = self.pane_log_path.clone() else {
            return;
        };
        let current_keys = self.persist_keys();
        if self
            .pane_log_capture
            .as_ref()
            .is_some_and(|capture| capture.restore_keys != current_keys)
        {
            self.pane_log_capture = None;
            return;
        }
        let capture = self.pane_log_capture.as_mut().unwrap();
        if let Some(key) = capture.keys.pop_front() {
            let (panes, captured) = live.capture_persist(&[key], &self.pane_log_captured);
            capture.panes.extend(panes);
            capture.captured.extend(captured);
        }
        if !capture.keys.is_empty() {
            return;
        }
        let capture = self.pane_log_capture.take().unwrap();
        // Rate-limit failed writes as well as successful ones. A slow writer
        // cannot queue unbounded snapshots or hold up incoming keystrokes.
        if worker.submit(path, capture.panes) {
            self.pane_log_pending_mark = Some(capture.mark);
            self.pane_log_captured = capture.captured;
        }
    }

    fn restore_pane_logs(&mut self) {
        let Some(path) = self.pane_log_path.clone() else {
            return;
        };
        if !path.exists() {
            return;
        }
        let keys = self.persist_keys();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match read_persist(&path) {
            Ok(file) => {
                match file.validate(
                    crate::pane_log_persist::PERSIST_FORMAT,
                    ScreenCodec::id(&EmulatorStateCodec),
                ) {
                    Ok(()) => live.restore_persist(&file.panes, &keys, &EmulatorStateCodec),
                    Err(fail) => live.reset_first_pane_log(fail.log_reason()),
                }
            }
            Err(_) => live.reset_first_pane_log("corrupt persist"),
        }
    }

    /// The shared mail-arrival signal. Client threads clone this to park
    /// on `MailWait` without holding the plane lock.
    pub fn mail_watch(&self) -> std::sync::Arc<crate::mailbox::MailboxWatch> {
        std::sync::Arc::clone(&self.mail_watch)
    }

    /// The connection's asserted agent identity.
    fn mail_agent(&self, client_raw: u64) -> Result<String, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let agent = self.client_agents.get(&client).cloned().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InvalidRequest,
                "no agent identity: send MailHello first (pmux mail --as <agent>)",
            )
        })?;
        self.mail_store
            .resolve(&agent)
            .map_err(Self::mail_store_error)
    }

    /// Depth for `agent`, for the `MailWait` predicate. Runs under the
    /// plane lock, briefly, inside `MailboxWatch::wait_until`.
    pub fn mail_depth_for(&self, agent: &str) -> Result<(u32, u32), ControlError> {
        let agent = self
            .mail_store
            .resolve(agent)
            .map_err(Self::mail_store_error)?;
        let agent = crate::mailbox::AgentId::new(agent).map_err(|error| {
            ControlError::new(ControlErrorCode::InvalidRequest, error.to_string())
        })?;
        self.mail_store.depth(&agent).map_err(|error| {
            ControlError::new(
                ControlErrorCode::Internal,
                format!("mailbox depth: {error}"),
            )
        })
    }

    fn mail_store_error(error: rusqlite::Error) -> ControlError {
        ControlError::new(
            ControlErrorCode::Internal,
            format!("mailbox store: {error}"),
        )
    }

    fn mail_hello(
        &mut self,
        client_raw: u64,
        agent: String,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let agent = crate::mailbox::AgentId::new(agent)
            .map_err(|error| {
                ControlError::new(ControlErrorCode::InvalidRequest, error.to_string())
            })?
            .to_string();
        let agent = self
            .mail_store
            .resolve(&agent)
            .map_err(Self::mail_store_error)?;
        self.client_agents.insert(client, agent.clone());
        Ok(ControlResponseData::MailSeated { agent })
    }

    /// Resolve `to` (agent id or alias) to an agent id. Unknown agents
    /// are fine — their mail queues (FR-1).
    fn mail_resolve_recipient(&self, to: &str) -> Result<crate::mailbox::AgentId, ControlError> {
        let resolved = self
            .mail_aliases
            .get(to)
            .cloned()
            .unwrap_or_else(|| to.to_string());
        let resolved = self
            .mail_store
            .resolve(&resolved)
            .map_err(Self::mail_store_error)?;
        crate::mailbox::AgentId::new(resolved).map_err(|error| {
            ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("recipient {to:?}: {error}"),
            )
        })
    }

    fn mail_send(
        &mut self,
        client_raw: u64,
        to: String,
        summary: String,
        body: String,
    ) -> Result<ControlResponseData, ControlError> {
        let from = self.mail_agent(client_raw)?;
        let to = self.mail_resolve_recipient(&to)?;
        let (id, depth) = self
            .mail_store
            .send(&from, &to, &summary, &body)
            .map_err(Self::mail_store_error)?;
        self.on_mail_stored(to.as_str(), depth);
        Ok(ControlResponseData::MailSent { id, depth })
    }

    fn mail_claim(&mut self, client_raw: u64) -> Result<ControlResponseData, ControlError> {
        let agent = self.mail_agent(client_raw)?;
        let agent = crate::mailbox::AgentId::new(agent)
            .map_err(|error| ControlError::new(ControlErrorCode::Internal, error.to_string()))?;
        let letters = self
            .mail_store
            .claim(&agent)
            .map_err(Self::mail_store_error)?;
        self.refresh_mail_attention(agent.as_str());
        Ok(ControlResponseData::MailLetters { letters })
    }

    fn mail_commit(
        &mut self,
        client_raw: u64,
        ids: Vec<String>,
    ) -> Result<ControlResponseData, ControlError> {
        let agent = self.mail_agent(client_raw)?;
        let agent = crate::mailbox::AgentId::new(agent)
            .map_err(|error| ControlError::new(ControlErrorCode::Internal, error.to_string()))?;
        let committed = self
            .mail_store
            .commit(&agent, &ids)
            .map_err(Self::mail_store_error)?;
        self.refresh_mail_attention(agent.as_str());
        Ok(ControlResponseData::MailCommitted { committed })
    }

    fn mail_release(
        &mut self,
        client_raw: u64,
        ids: Vec<String>,
    ) -> Result<ControlResponseData, ControlError> {
        let agent = self.mail_agent(client_raw)?;
        let agent = crate::mailbox::AgentId::new(agent)
            .map_err(|error| ControlError::new(ControlErrorCode::Internal, error.to_string()))?;
        let released = self
            .mail_store
            .release(&agent, &ids)
            .map_err(Self::mail_store_error)?;
        self.refresh_mail_attention(agent.as_str());
        Ok(ControlResponseData::MailReleased { released })
    }

    fn mail_inbox(&mut self, client_raw: u64) -> Result<ControlResponseData, ControlError> {
        let agent = self.mail_agent(client_raw)?;
        let (open, held) = self.mail_depth_for(&agent)?;
        self.cancel_mail_nudge_for_agent(&agent);
        Ok(ControlResponseData::MailDepth { open, held })
    }

    fn mail_who(&mut self, client_raw: u64) -> Result<ControlResponseData, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        let mut peers = Vec::new();
        for session in self.domain.sessions() {
            let Some(agent_id) = session.agent_id.clone() else {
                continue;
            };
            let pane_live = self.live.as_ref().is_some_and(|live| {
                session.windows.iter().any(|window| {
                    self.domain.window(*window).is_some_and(|w| {
                        w.layout
                            .panes()
                            .iter()
                            .any(|p| live.child_pid(p.get()).is_some())
                    })
                })
            });
            let mut aliases: Vec<String> = self
                .mail_aliases
                .iter()
                .filter(|(_, owner)| **owner == agent_id)
                .map(|(name, _)| name.clone())
                .collect();
            aliases.extend(
                self.mail_store
                    .forwards(&agent_id)
                    .map_err(Self::mail_store_error)?,
            );
            aliases.sort();
            aliases.dedup();
            peers.push(MailPeer {
                agent_id,
                session: session.name.clone(),
                pane_live,
                aliases,
            });
        }
        peers.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        Ok(ControlResponseData::MailPeers { peers })
    }

    fn mail_alias(
        &mut self,
        client_raw: u64,
        name: String,
    ) -> Result<ControlResponseData, ControlError> {
        let agent = self.mail_agent(client_raw)?;
        let name = crate::mailbox::AgentId::new(&name)
            .map_err(|error| {
                ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("alias {name:?} is not an agent-id charset name: {error}"),
                )
            })?
            .to_string();
        if self
            .domain
            .sessions()
            .any(|session| session.agent_id.as_deref() == Some(name.as_str()))
        {
            return Ok(ControlResponseData::MailRefused {
                reason: format!("alias {name} is a bound agent id"),
            });
        }
        let canonical = self
            .mail_store
            .resolve(&name)
            .map_err(Self::mail_store_error)?;
        if canonical != name && canonical != agent {
            return Ok(ControlResponseData::MailRefused {
                reason: format!("alias {name} is a retained mailbox address"),
            });
        }
        match self.mail_aliases.get(&name) {
            Some(owner) if *owner != agent => {
                return Ok(ControlResponseData::MailRefused {
                    reason: format!("alias {name} is taken"),
                });
            }
            _ => {}
        }
        self.mail_aliases.insert(name.clone(), agent.clone());
        Ok(ControlResponseData::MailAliased { name, agent })
    }

    fn mail_broadcast(
        &mut self,
        client_raw: u64,
        summary: String,
        body: String,
    ) -> Result<ControlResponseData, ControlError> {
        let from = self.mail_agent(client_raw)?;
        let mut recipients: Vec<crate::mailbox::AgentId> = Vec::new();
        let mut names = Vec::new();
        for session in self.domain.sessions() {
            let Some(agent_id) = session.agent_id.as_deref() else {
                continue;
            };
            if agent_id == from {
                continue;
            }
            let agent = crate::mailbox::AgentId::new(agent_id).map_err(|error| {
                ControlError::new(ControlErrorCode::Internal, error.to_string())
            })?;
            recipients.push(agent);
            names.push(agent_id.to_string());
        }
        let delivered = self
            .mail_store
            .broadcast(&from, &recipients, &summary, &body)
            .map_err(Self::mail_store_error)?;
        for agent in &recipients {
            let depth = self
                .mail_depth_for(agent.as_str())
                .map(|(open, _)| open)
                .unwrap_or(0);
            self.on_mail_stored(agent.as_str(), depth);
        }
        Ok(ControlResponseData::MailBroadcasted {
            delivered,
            recipients: names,
        })
    }

    /// Validate a `MailWait` and hand the client thread what it needs to
    /// park off-lock: the caller's agent and the shared watch.
    pub fn mail_wait_prepare(
        &self,
        client_raw: u64,
    ) -> Result<(String, std::sync::Arc<crate::mailbox::MailboxWatch>), ControlError> {
        let agent = self.mail_agent(client_raw)?;
        Ok((agent, self.mail_watch()))
    }

    /// The pane the in-process doorbell nudges for `agent`: the
    /// first pane of the session's first window. Agent sessions are
    /// single-pane in practice (`pmux new --agent`); multi-pane sessions
    /// get the nudge on the first pane. `None` when the agent has no
    /// live session (mail queues silently) or the session is headless.
    fn doorbell_pane(&self, agent: &str) -> Option<u64> {
        let session = self.domain.session_by_agent(agent)?;
        let session = self.domain.session(session)?;
        let window = self.domain.window(*session.windows.first()?)?;
        window.layout.panes().first().map(|pane| pane.get())
    }

    fn cancel_mail_nudge_for_agent(&mut self, agent: &str) {
        if let Some(pane_raw) = self.doorbell_pane(agent) {
            self.mail_nudges.remove(&pane_raw);
        }
    }

    /// In-process doorbell: one synchronous call after a letter
    /// is stored for `recipient`. Arms sticky `mail` attention on
    /// the recipient's pane and injects the `PMUX_MAIL` token
    /// plus submit bytes through the gates. Focused panes still get the
    /// token: the occupant is the agent. Busy output and unsubmitted
    /// composer text still defer. Retries continue until peek or claim.
    fn on_mail_stored(&mut self, recipient: &str, depth: u32) {
        let Some(pane_raw) = self.doorbell_pane(recipient) else {
            return; // headless or unknown — mail queues
        };
        let queue_rev = self.mail_rev.get(&pane_raw).copied().unwrap_or(0) + 1;
        if self
            .arm_mail_attention(
                pane_raw,
                MAIL_ATTENTION_CELL.into(),
                0,
                queue_rev,
                depth,
                Some(MailWake::Armed),
                None,
            )
            .is_err()
        {
            return;
        }
        let _ = self.gated_mail_inject(self.doorbell_client, pane_raw, queue_rev, 1, false);
    }

    /// After a session is created and bound to `agent`, fire the
    /// doorbell for mail that queued while no live pane existed
    /// (PT-68 placeholder recreate). No-op when depth is zero or
    /// the session is headless.
    fn rearm_queued_mail_for_agent(&mut self, agent: &str) {
        let Ok((open, _)) = self.mail_depth_for(agent) else {
            return;
        };
        if open == 0 {
            return;
        }
        self.on_mail_stored(agent, open);
    }

    /// Keep the sticky indicator truthful after the agent reads or
    /// re-queues mail: re-arm with the current open depth, or
    /// clear when nothing is open. Never injects — the agent is already
    /// at the keyboard.
    fn refresh_mail_attention(&mut self, agent: &str) {
        let Some(pane_raw) = self.doorbell_pane(agent) else {
            return;
        };
        self.mail_nudges.remove(&pane_raw);
        let Ok((open, _)) = self.mail_depth_for(agent) else {
            return;
        };
        let queue_rev = self.mail_rev.get(&pane_raw).copied().unwrap_or(0) + 1;
        if open == 0 {
            let _ = self.clear_mail_attention(pane_raw, queue_rev);
        } else {
            let _ = self.arm_mail_attention(
                pane_raw,
                MAIL_ATTENTION_CELL.into(),
                0,
                queue_rev,
                open,
                None,
                None,
            );
        }
    }

    fn emit_mail_attention_changed(
        &mut self,
        state: Option<&MailAttentionState>,
        pane_raw: u64,
        clear_queue_rev: Option<u64>,
    ) {
        let depth = state.map(|attention| attention.depth).unwrap_or(0);
        if let Some(live) = self.live.as_mut() {
            live.log_mail_depth(pane_raw, depth);
        }
        match state {
            Some(attention) => {
                let _ = self.emit(Event::MailAttentionChanged {
                    pane_id: pane_raw,
                    cell: Some(attention.cell.clone()),
                    gen: Some(attention.gen),
                    queue_rev: Some(attention.queue_rev),
                    depth: attention.depth,
                    wake: attention.wake,
                });
            }
            None => {
                let _ = self.emit(Event::MailAttentionChanged {
                    pane_id: pane_raw,
                    cell: None,
                    gen: None,
                    queue_rev: clear_queue_rev,
                    depth: 0,
                    wake: None,
                });
            }
        }
    }

    fn forget_pane_mail(&mut self, pane_raw: u64) {
        self.pane_write_receipts.remove(&pane_raw);
        let had = self.mail_attention.remove(&pane_raw).is_some();
        self.attention.remove(&pane_raw);
        self.mail_rev.remove(&pane_raw);
        self.mail_nudges.remove(&pane_raw);
        self.write_ledger.remove(&pane_raw);
        self.last_input_at_ms.remove(&pane_raw);
        self.last_output_at_ms.remove(&pane_raw);
        self.output_bytes.remove(&pane_raw);
        self.mail_inject.remove(&pane_raw);
        self.mail_inject_last.remove(&pane_raw);
        self.client_pane_focus
            .retain(|_, focused| *focused != pane_raw);
        if had {
            self.emit_mail_attention_changed(None, pane_raw, None);
        }
        if self.pane_status.remove(&pane_raw).is_some() {
            let _ = self.emit(Event::PaneStatusChanged {
                pane_id: pane_raw,
                status: None,
            });
        }
    }

    fn mail_rev_is_stale(&self, pane_raw: u64, queue_rev: u64) -> bool {
        self.mail_rev
            .get(&pane_raw)
            .is_some_and(|watermark| queue_rev <= *watermark)
    }

    fn validate_mail_cell(cell: &str) -> Result<(), ControlError> {
        let cell = cell.trim();
        if cell.is_empty() || cell.len() > 128 || cell.contains('\0') {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "mail cell must be a non-empty label",
            ));
        }
        Ok(())
    }

    fn mail_inject_response(
        &mut self,
        pane_raw: u64,
        queue_rev: u64,
        outcome: MailInjectOutcome,
        nbytes: usize,
    ) -> ControlResponseData {
        let agent = match self.foreground_agent_for(pane_raw) {
            crate::InjectAgent::Claude => "claude",
            crate::InjectAgent::Grok => "grok",
            crate::InjectAgent::Cursor => "cursor",
            crate::InjectAgent::Codex => "codex",
            crate::InjectAgent::Kiro => "kiro",
            crate::InjectAgent::Unknown => "unknown",
        };
        let at_ms = now_unix_ms();
        eprintln!(
            "INFO pmux_mail_inject agent={agent} pane={pane_raw} queue_rev={queue_rev} outcome={outcome:?} nbytes={nbytes}"
        );
        self.mail_inject_last.insert(
            pane_raw,
            MailInjectDiagnostic {
                agent: agent.to_string(),
                queue_rev,
                outcome,
                nbytes,
                at_ms,
            },
        );
        ControlResponseData::MailInject {
            pane_id: pane_raw,
            queue_rev,
            outcome,
            nbytes,
            ledger: self.pane_ledger(pane_raw),
        }
    }

    fn pane_revision(&self, pane_raw: u64) -> Option<u64> {
        self.live
            .as_ref()
            .and_then(|live| live.content(pane_raw))
            .map(|content| content.revision)
    }

    fn apply_mail_inject(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        queue_rev: u64,
        remaining_attempts: u32,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        self.gated_mail_inject(client, pane_raw, queue_rev, remaining_attempts, false)
    }

    /// Gated inject core (gates in-process doorbell).
    /// Caller has validated the client is active and the pane exists.
    fn gated_mail_inject(
        &mut self,
        client: ClientId,
        pane_raw: u64,
        queue_rev: u64,
        remaining_attempts: u32,
        verification_retry: bool,
    ) -> Result<ControlResponseData, ControlError> {
        let pane = pane_id(pane_raw)?;

        let attention = self.mail_attention.get(&pane_raw).cloned();
        let Some(attention) = attention.filter(|a| a.depth > 0 && a.queue_rev == queue_rev) else {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::SkippedNoMail,
                0,
            ));
        };
        if remaining_attempts == 0 || attention.wake == Some(MailWake::Exhausted) {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::SkippedExhausted,
                0,
            ));
        }
        let prior = self.mail_inject.get(&pane_raw).copied();
        if prior.is_some_and(|r| r.queue_rev == queue_rev && r.stuck)
            || attention.wake == Some(MailWake::Stuck)
        {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::SkippedStuck,
                0,
            ));
        }
        let ledger = self.pane_ledger(pane_raw);
        if ledger.dirty_input {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::DeferredDirty,
                0,
            ));
        }
        let now = now_unix_ms();
        if ledger
            .last_input_at_ms
            .is_some_and(|at| now.saturating_sub(at) < MAIL_INJECT_INPUT_IDLE_MS)
        {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::DeferredTyping,
                0,
            ));
        }
        if self.output_bytes_in_quiet_window(pane_raw, now) >= MAIL_INJECT_BUSY_BYTES {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::DeferredBusy,
                0,
            ));
        }
        if let Some(record) = prior.filter(|r| r.queue_rev == queue_rev && !r.stuck) {
            let still_hidden = now < record.hide_until_ms
                && self
                    .pane_revision(pane_raw)
                    .is_some_and(|rev| rev == record.hide_revision);
            if still_hidden && !verification_retry {
                return Ok(self.mail_inject_response(
                    pane_raw,
                    queue_rev,
                    MailInjectOutcome::DeferredHide,
                    0,
                ));
            }
        }
        // A normal client-triggered inject remains one-shot. The only second
        // write is the bounded verification nudge scheduled after Wrote.
        if prior.is_some_and(|r| {
            r.queue_rev == queue_rev
                && r.writes > 0
                && !r.stuck
                && !(verification_retry && r.writes == 1)
        }) {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::SkippedExhausted,
                0,
            ));
        }

        // A plain shell has no composer to ring: keep the letter lit and let
        // the nudge retry once an agent CLI is the terminal foreground. A
        // background job is not the foreground (PT-94).
        let agent = self.foreground_agent_for(pane_raw);
        if agent == crate::InjectAgent::Unknown {
            return Ok(self.mail_inject_response(
                pane_raw,
                queue_rev,
                MailInjectOutcome::DeferredNoAgent,
                0,
            ));
        }

        let held = self.domain.controller(pane).ok() == Some(Some(client));
        match self.domain.acquire_controller(pane, client) {
            Ok(()) => {}
            Err(DomainError::LeaseHeld { .. }) => {
                return Ok(self.mail_inject_response(
                    pane_raw,
                    queue_rev,
                    MailInjectOutcome::DeferredLease,
                    0,
                ));
            }
            Err(error) => return Err(control_domain_error(error)),
        }
        let acquired_now = !held;

        let before = self.pane_revision(pane_raw);
        // The same foreground agent picks the submit chord (PT-94).
        let chunks = crate::inject_writes(agent);
        let nbytes = match self.write_inject_chunks(pane_raw, &chunks) {
            Ok(n) => n,
            Err(LiveWriteError::Backpressure) => {
                if acquired_now {
                    let _ = self.domain.release_controller(pane, client);
                }
                return Err(ControlError::new(
                    ControlErrorCode::Backpressure,
                    "pane input queue is full; retry later",
                ));
            }
            Err(LiveWriteError::UnknownPane | LiveWriteError::Disconnected) => {
                if acquired_now {
                    let _ = self.domain.release_controller(pane, client);
                }
                return Err(ControlError::new(
                    ControlErrorCode::InputRouteUnavailable,
                    "live PTY input route is unavailable",
                ));
            }
        };
        // Submit consumed the line; do not leave dirty_input set.
        self.write_ledger.insert(pane_raw, (now_unix_ms(), true));

        let deadline = Instant::now() + Duration::from_millis(MAIL_INJECT_EPOCH_WAIT_MS);
        let mut bumped = false;
        // Start below the 10 ms send budget. A fixed 10 ms sleep makes
        // every reply that arrives after the first drain miss that budget.
        // Back off for slow readers while keeping the existing deadline.
        let mut pause = Duration::from_millis(1);
        while Instant::now() < deadline {
            self.drain_live();
            if self.pane_revision(pane_raw) != before {
                bumped = true;
                break;
            }
            thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
            pause = (pause * 2).min(Duration::from_millis(MAIL_INJECT_EPOCH_SLICE_MS));
        }
        if acquired_now {
            let _ = self.domain.release_controller(pane, client);
        }

        let writes = self
            .mail_inject
            .get(&pane_raw)
            .map(|r| r.writes)
            .unwrap_or(0)
            + 1;
        let after = self.pane_revision(pane_raw).unwrap_or(0);
        if bumped {
            let hide_until_ms = now_unix_ms().saturating_add(MAIL_INJECT_HIDE_MS);
            self.mail_inject.insert(
                pane_raw,
                MailInjectRecord {
                    queue_rev,
                    stuck: false,
                    hide_until_ms,
                    hide_revision: after,
                    writes,
                },
            );
            if writes == 1 {
                self.mail_nudges.insert(
                    pane_raw,
                    MailNudge {
                        queue_rev,
                        due_at_ms: now_unix_ms().saturating_add(MAIL_INJECT_VERIFY_MS),
                        verification: true,
                    },
                );
            }
            Ok(self.mail_inject_response(pane_raw, queue_rev, MailInjectOutcome::Wrote, nbytes))
        } else {
            self.mail_inject.insert(
                pane_raw,
                MailInjectRecord {
                    queue_rev,
                    stuck: true,
                    hide_until_ms: 0,
                    hide_revision: after,
                    writes,
                },
            );
            Ok(self.mail_inject_response(pane_raw, queue_rev, MailInjectOutcome::Stuck, nbytes))
        }
    }

    #[cfg(test)]
    pub(crate) fn set_inject_agent_override(&mut self, agent: Option<crate::InjectAgent>) {
        self.inject_agent_override = agent;
    }

    /// Agent CLI in the pane's terminal foreground process group. `Unknown`
    /// when the foreground is a shell prompt, a non-agent command, or cannot
    /// be read (fail closed). Uses the same detection as `space save`.
    fn foreground_agent_for(&self, pane_raw: u64) -> crate::InjectAgent {
        #[cfg(test)]
        if let Some(agent) = self.inject_agent_override {
            return agent;
        }
        let Some(root) = self.live.as_ref().and_then(|live| live.child_pid(pane_raw)) else {
            return crate::InjectAgent::Unknown;
        };
        crate::procinfo::foreground_command(root)
            .and_then(|cmd| crate::classify_cmdline(&cmd))
            .unwrap_or(crate::InjectAgent::Unknown)
    }

    fn write_inject_chunks(
        &self,
        pane_raw: u64,
        chunks: &[Vec<u8>],
    ) -> Result<usize, LiveWriteError> {
        let live = self.live.as_ref().ok_or(LiveWriteError::Disconnected)?;
        let mut n = 0usize;
        for (i, chunk) in chunks.iter().enumerate() {
            live.write(pane_raw, chunk.clone())?;
            n = n.saturating_add(chunk.len());
            if i + 1 < chunks.len() {
                // Codex inserts on the first CR and submits on a later CR.
                thread::sleep(Duration::from_millis(80));
            }
        }
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_mail_attention_set(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        cell: String,
        gen: u64,
        queue_rev: u64,
        depth: u32,
        wake: Option<MailWake>,
        bound_pid: Option<u32>,
    ) -> Result<ControlResponseData, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        if depth == 0 {
            return self.apply_mail_attention_clear(client_raw, pane_raw, queue_rev);
        }
        self.arm_mail_attention(pane_raw, cell, gen, queue_rev, depth, wake, bound_pid)
    }

    /// Arm sticky mail attention on a pane (set path
    /// in-process doorbell). Caller has validated the pane exists.
    #[allow(clippy::too_many_arguments)]
    fn arm_mail_attention(
        &mut self,
        pane_raw: u64,
        cell: String,
        gen: u64,
        queue_rev: u64,
        depth: u32,
        wake: Option<MailWake>,
        bound_pid: Option<u32>,
    ) -> Result<ControlResponseData, ControlError> {
        Self::validate_mail_cell(&cell)?;
        let next = MailAttentionState {
            pane_id: pane_raw,
            cell: cell.trim().to_string(),
            gen,
            queue_rev,
            depth,
            wake,
            bound_pid,
        };
        if self.mail_rev_is_stale(pane_raw, queue_rev) {
            return Ok(self.mail_attention_response(pane_raw));
        }
        self.ensure_sequence_capacity()?;
        self.mail_rev.insert(pane_raw, queue_rev);
        self.mail_attention.insert(pane_raw, next.clone());
        self.emit_mail_attention_changed(Some(&next), pane_raw, None);
        if next.wake == Some(MailWake::Armed) {
            self.mail_nudges.insert(
                pane_raw,
                MailNudge {
                    queue_rev,
                    due_at_ms: now_unix_ms().saturating_add(MAIL_INJECT_NUDGE_DELAY_MS),
                    verification: false,
                },
            );
        } else {
            self.mail_nudges.remove(&pane_raw);
        }
        Ok(self.mail_attention_response(pane_raw))
    }

    fn apply_mail_attention_clear(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        queue_rev: u64,
    ) -> Result<ControlResponseData, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        self.clear_mail_attention(pane_raw, queue_rev)
    }

    /// Clear sticky mail attention on a pane (clear path
    /// doorbell refresh). Caller has validated the pane exists.
    fn clear_mail_attention(
        &mut self,
        pane_raw: u64,
        queue_rev: u64,
    ) -> Result<ControlResponseData, ControlError> {
        if self.mail_rev_is_stale(pane_raw, queue_rev) {
            return Ok(self.mail_attention_response(pane_raw));
        }
        let had = self.mail_attention.contains_key(&pane_raw);
        if had {
            self.ensure_sequence_capacity()?;
        }
        self.mail_rev.insert(pane_raw, queue_rev);
        self.mail_attention.remove(&pane_raw);
        self.mail_nudges.remove(&pane_raw);
        if had {
            self.emit_mail_attention_changed(None, pane_raw, Some(queue_rev));
        }
        Ok(self.mail_attention_response(pane_raw))
    }

    /// First live pane whose PTY tree contains `bound_pid` (ADR-0039 join).
    pub(crate) fn pane_for_bound_pid(&self, bound: u32) -> Option<u64> {
        if bound == 0 {
            return None;
        }
        let live = self.live.as_ref()?;
        for &pane_raw in self.spawn.keys() {
            let Some(root) = live.child_pid(pane_raw) else {
                continue;
            };
            if crate::pid_in_tree(root, bound) || crate::pid_in_tree(bound, root) {
                return Some(pane_raw);
            }
        }
        None
    }

    /// Lit mail panes that name a Hive cell (join key for `attention.peek`).
    pub(crate) fn lit_mail_cells(&self) -> Vec<(u64, String)> {
        self.mail_attention
            .values()
            .filter(|state| state.depth > 0 && !state.cell.trim().is_empty())
            .map(|state| (state.pane_id, state.cell.clone()))
            .collect()
    }

    /// Hive drained this pane's inbox (`depth == 0` / peek not found).
    /// Advances the watermark so a stale Set cannot resurrect the letter.
    pub(crate) fn clear_mail_after_hive_drain(&mut self, pane_raw: u64) -> bool {
        let Some(state) = self.mail_attention.get(&pane_raw) else {
            return false;
        };
        let queue_rev = state.queue_rev.saturating_add(1);
        if pane_id(pane_raw)
            .ok()
            .is_none_or(|pane| self.domain.pane(pane).is_none())
        {
            return false;
        }
        if self.mail_rev_is_stale(pane_raw, queue_rev) {
            return false;
        }
        if self.ensure_sequence_capacity().is_err() {
            return false;
        }
        self.mail_rev.insert(pane_raw, queue_rev);
        self.mail_attention.remove(&pane_raw);
        self.mail_nudges.remove(&pane_raw);
        self.emit_mail_attention_changed(None, pane_raw, Some(queue_rev));
        true
    }

    pub fn snapshot(&self) -> Result<Snapshot, ControlError> {
        let mut sessions = Vec::new();
        for session in self.domain.sessions() {
            let mut windows = Vec::new();
            for window_id in &session.windows {
                let window = self
                    .domain
                    .window(*window_id)
                    .ok_or_else(|| stale_id("window", window_id.get()))?;
                let bound = self
                    .bounds
                    .get(&window_id.get())
                    .copied()
                    .ok_or_else(|| stale_id("window geometry", window_id.get()))?;
                let geometry = self.window_geometry(*window_id)?;
                let geometry_by_pane: HashMap<_, _> = geometry
                    .into_iter()
                    .map(|geometry| (geometry.pane_id, geometry))
                    .collect();
                let mut panes = Vec::new();
                for pane_id in window.layout.panes() {
                    let pane = self
                        .domain
                        .pane(pane_id)
                        .ok_or_else(|| stale_id("pane", pane_id.get()))?;
                    panes.push(PaneSnapshot {
                        pane_write: self.pane_write_receipts.get(&pane_id.get()).cloned(),
                        id: pane_id.get(),
                        title: pane.title.clone(),
                        title_pinned: pane.title_pinned,
                        controller_id: pane.controller.map(|id| id.get()),
                        geometry: *geometry_by_pane
                            .get(&pane_id.get())
                            .ok_or_else(|| stale_id("pane geometry", pane_id.get()))?,
                        spawn: self.spawn.get(&pane_id.get()).cloned(),
                        child_pid: self
                            .live
                            .as_ref()
                            .and_then(|live| live.child_pid(pane_id.get())),
                        mail: self.mail_attention.get(&pane_id.get()).cloned(),
                        status: self.pane_status.get(&pane_id.get()).cloned(),
                        attention: self.attention.get(&pane_id.get()).cloned(),
                        mail_inject: self.mail_inject_last.get(&pane_id.get()).cloned(),
                        ledger: self.pane_ledger(pane_id.get()),
                        size_owner: self.size_owner_for(window_id.get()),
                    });
                }
                windows.push(WindowSnapshot {
                    id: window.id.get(),
                    title: window.title.clone(),
                    bounds: bound,
                    layout: layout_snapshot(&window.layout),
                    panes,
                    sync_input: window.sync_input,
                });
            }
            sessions.push(SessionSnapshot {
                id: session.id.get(),
                name: session.name.clone(),
                agent_id: session.agent_id.clone(),
                space_id: session.space_id.clone(),
                windows,
            });
        }
        Ok(Snapshot {
            sequence: self.sequence,
            sessions,
        })
    }

    pub fn handle(&mut self, request: ControlRequest) -> ControlResponse {
        let (version, request_id) = request.header();
        if request_id == 0 {
            return ControlResponse::error(
                request_id,
                ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    "request_id must be non-zero",
                ),
            );
        }
        if version != PROTOCOL_VERSION {
            return ControlResponse::error(
                request_id,
                ControlError::new(
                    ControlErrorCode::IncompatibleVersion,
                    format!("protocol {version} is incompatible with {PROTOCOL_VERSION}"),
                ),
            );
        }
        if self.shutdown_pending {
            return ControlResponse::error(
                request_id,
                ControlError::new(ControlErrorCode::InvalidRequest, "daemon is shutting down"),
            );
        }
        match self.handle_compatible(request) {
            Ok(response) => ControlResponse::ok(request_id, response),
            Err(error) => ControlResponse::error(request_id, error),
        }
    }

    fn handle_compatible(
        &mut self,
        request: ControlRequest,
    ) -> Result<ControlResponseData, ControlError> {
        match request {
            ControlRequest::ServerInfo { .. } => Ok(ControlResponseData::ServerInfo {
                package_version: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
            }),
            ControlRequest::Ping { .. } => Ok(ControlResponseData::Pong),
            ControlRequest::Snapshot { .. } => Ok(ControlResponseData::Snapshot {
                snapshot: self.snapshot()?,
            }),
            ControlRequest::Events {
                after_sequence,
                limit,
                ..
            } => Ok(ControlResponseData::Events {
                batch: self.events_after(after_sequence, limit)?,
            }),
            ControlRequest::SubscribePane { .. } => Err(ControlError::new(
                ControlErrorCode::Internal,
                "SubscribePane is served on the client thread, never under the plane lock",
            )),
            ControlRequest::RegisterClient { .. } => {
                let mut viewer_bytes = [0u8; 16];
                getrandom::fill(&mut viewer_bytes).map_err(|error| {
                    ControlError::new(
                        ControlErrorCode::Internal,
                        format!("mint rich viewer identity: {error}"),
                    )
                })?;
                let client = self.domain.mint_client().map_err(control_domain_error)?;
                self.active_clients.insert(client);
                self.viewer_ids
                    .insert(client, ViewerId::from_bytes(viewer_bytes));
                if let Some(session) = self.domain.sessions().next() {
                    self.client_views.insert(client, session.id);
                }
                Ok(ControlResponseData::ClientRegistered {
                    client_id: client.get(),
                })
            }
            ControlRequest::AcquireLease {
                client_id: client_raw,
                pane_id: pane_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let client = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                self.require_client_space(client, pane)?;
                let previous = self.domain.controller(pane).map_err(control_domain_error)?;
                self.domain
                    .acquire_controller(pane, client)
                    .map_err(control_domain_error)?;
                if let Some(live) = self.live.as_mut() {
                    live.note_controller(pane_raw, Some(client_raw));
                }
                let sequence = self.emit(Event::LeaseChanged {
                    pane_id: pane_raw,
                    controller_id: Some(client_raw),
                    previous_controller_id: previous.map(|id| id.get()),
                });
                let _ = sequence;
                Ok(ControlResponseData::Lease {
                    pane_id: pane_raw,
                    controller_id: Some(client_raw),
                    previous_controller_id: previous.map(|id| id.get()),
                })
            }
            ControlRequest::ReleaseLease {
                client_id: client_raw,
                pane_id: pane_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let client = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                let previous = self.domain.controller(pane).map_err(control_domain_error)?;
                if let Some(viewer_id) = self.viewer_ids.get(&client).copied() {
                    if let Some(live) = self.live.as_mut() {
                        let _ = live.revoke_viewer_on(pane_raw, viewer_id);
                    }
                }
                self.domain
                    .release_controller(pane, client)
                    .map_err(control_domain_error)?;
                if let Some(live) = self.live.as_mut() {
                    live.note_controller(pane_raw, None);
                }
                let _ = self.emit(Event::LeaseChanged {
                    pane_id: pane_raw,
                    controller_id: None,
                    previous_controller_id: previous.map(|id| id.get()),
                });
                Ok(ControlResponseData::Lease {
                    pane_id: pane_raw,
                    controller_id: None,
                    previous_controller_id: previous.map(|id| id.get()),
                })
            }
            ControlRequest::TakeoverLease {
                client_id: client_raw,
                pane_id: pane_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let client = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                self.require_client_space(client, pane)?;
                let previous = self
                    .domain
                    .takeover_controller(pane, client)
                    .map_err(control_domain_error)?;
                if let Some(live) = self.live.as_mut() {
                    live.note_controller(pane_raw, Some(client_raw));
                }
                if let Some(previous_client) = previous {
                    if let Some(viewer_id) = self.viewer_ids.get(&previous_client).copied() {
                        if let Some(live) = self.live.as_mut() {
                            let _ = live.revoke_viewer_on(pane_raw, viewer_id);
                        }
                    }
                }
                let _ = self.emit(Event::LeaseChanged {
                    pane_id: pane_raw,
                    controller_id: Some(client_raw),
                    previous_controller_id: previous.map(|id| id.get()),
                });
                Ok(ControlResponseData::Lease {
                    pane_id: pane_raw,
                    controller_id: Some(client_raw),
                    previous_controller_id: previous.map(|id| id.get()),
                })
            }
            ControlRequest::DisconnectClient {
                client_id: client_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let client = self.require_active_client(client_raw)?;
                let viewer_id = self.viewer_ids.get(&client).copied();
                let released = self.domain.release_all_controller_leases(client);
                for pane in &released {
                    if let (Some(viewer_id), Some(live)) = (viewer_id, self.live.as_mut()) {
                        let _ = live.revoke_viewer_on(pane.get(), viewer_id);
                    }
                    let _ = self.emit(Event::LeaseChanged {
                        pane_id: pane.get(),
                        controller_id: None,
                        previous_controller_id: Some(client_raw),
                    });
                }
                let viewport_windows: Vec<u64> = self
                    .client_viewports
                    .get(&client)
                    .map(|views| views.keys().copied().collect())
                    .unwrap_or_default();
                self.active_clients.remove(&client);
                self.client_spaces.remove(&client);
                self.viewer_ids.remove(&client);
                self.client_views.remove(&client);
                self.client_window_views.remove(&client);
                self.client_pane_focus.remove(&client);
                self.client_viewports.remove(&client);
                self.client_agents.remove(&client);
                self.host_clients.remove(&client);
                for window_raw in viewport_windows {
                    let owner_before = self.size_owner_for(window_raw);
                    if self.latest_client.get(&window_raw) == Some(&client) {
                        self.latest_client.remove(&window_raw);
                        if let Some(host) = self.host_clients.iter().copied().next() {
                            self.latest_client.insert(window_raw, host);
                        }
                    }
                    let _ = self.apply_disconnect_viewport(window_raw);
                    self.publish_size_owner(window_raw, owner_before);
                }
                Ok(ControlResponseData::LeasesReleased {
                    client_id: client_raw,
                    pane_ids: released.iter().map(|id| id.get()).collect(),
                })
            }
            ControlRequest::PaneWrite {
                client_id,
                pane_id,
                expected_child_pid,
                data,
                submit,
                ..
            } => self.intentional_pane_write(client_id, pane_id, expected_child_pid, data, submit),
            ControlRequest::WritePane {
                client_id: client_raw,
                pane_id: pane_raw,
                data,
                ..
            } => {
                if data.len() > MAX_SPAWN_BYTES {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        format!("write payload exceeds {MAX_SPAWN_BYTES} bytes"),
                    ));
                }
                let client = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                self.require_client_space(client, pane)?;
                if self.host_clients.contains(&client) {
                    self.mark_host_latest(pane);
                }
                // Holder, or lease-free when no controller. Observers of a
                // held pane still fail.
                self.domain
                    .allow_write(pane, client)
                    .map_err(control_domain_error)?;
                // Authoritative dirty gate (PT-140): a lease-free writer must
                // not land bytes in a partial line someone else is typing.
                // The holder is exempt — its own keystrokes are the dirt.
                let controller = self.domain.controller(pane).map_err(control_domain_error)?;
                if controller.is_none() && self.pane_ledger(pane_raw).dirty_input {
                    return Err(ControlError::new(
                        ControlErrorCode::InputDirty,
                        "pane has unsubmitted input; take the lease (pmux send --force) to write",
                    ));
                }
                let siblings = self.sync_siblings(pane, pane_raw);
                let nbytes = data.len();
                let ended_cr = write_ended_with_cr(&data);
                let clears_attention = self.attention.contains_key(&pane_raw);
                if clears_attention {
                    self.ensure_sequence_capacity()?;
                }
                let live = self.live.as_mut().ok_or_else(|| {
                    ControlError::new(
                        ControlErrorCode::InputRouteUnavailable,
                        "controller authority verified, but this plane has no live PTY runtime",
                    )
                })?;
                let copy_requests = data
                    .bytes()
                    .filter(|&byte| byte == b'y' || byte == b'Y')
                    .count();
                let bytes = data.into_bytes();
                match live.write(pane_raw, bytes.clone()) {
                    Ok(()) => {
                        for sibling in siblings {
                            // Sibling fan-out is best-effort. Swallow
                            // Backpressure/Disconnected/UnknownPane; the
                            // focused pane already accepted the write.
                            let _ = live.write(sibling, bytes.clone());
                        }
                        for _ in 0..copy_requests {
                            live.queue_copy_request(pane_raw, client_raw);
                        }
                        let now = now_unix_ms();
                        self.write_ledger.insert(pane_raw, (now, ended_cr));
                        self.last_input_at_ms.insert(pane_raw, now);
                        if clears_attention {
                            self.attention.remove(&pane_raw);
                            self.team_attention.remove(&pane_raw);
                            let _ = self.emit(Event::PaneAttentionCleared { pane_id: pane_raw });
                        }
                        Ok(ControlResponseData::WriteQueued {
                            pane_id: pane_raw,
                            nbytes,
                        })
                    }
                    Err(LiveWriteError::Backpressure) => Err(ControlError::new(
                        ControlErrorCode::Backpressure,
                        "pane input queue is full; retry later",
                    )),
                    Err(LiveWriteError::UnknownPane | LiveWriteError::Disconnected) => {
                        Err(ControlError::new(
                            ControlErrorCode::InputRouteUnavailable,
                            "live PTY input route is unavailable",
                        ))
                    }
                }
            }
            ControlRequest::ReadPane {
                client_id: client_raw,
                pane_id: pane_raw,
                ..
            } => {
                let _ = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                if self.domain.pane(pane).is_none() {
                    return Err(stale_id("pane", pane_raw));
                }
                let content = self
                    .live
                    .as_ref()
                    .and_then(|live| live.content(pane_raw))
                    .ok_or_else(|| {
                        ControlError::new(
                            ControlErrorCode::InputRouteUnavailable,
                            "server-owned pane content is unavailable",
                        )
                    })?;
                Ok(ControlResponseData::PaneContent { content })
            }
            ControlRequest::ReadPaneStyled {
                client_id: client_raw,
                pane_id: pane_raw,
                view_offset,
                ..
            } => {
                let client = self.require_active_client(client_raw)?;
                let pane = pane_id(pane_raw)?;
                if self.domain.pane(pane).is_none() {
                    return Err(stale_id("pane", pane_raw));
                }
                let viewer_id = self.viewer_ids.get(&client).copied();
                let content = self
                    .live
                    .as_mut()
                    .and_then(|live| {
                        viewer_id.and_then(|viewer_id| {
                            let mut content = live.styled(pane_raw, view_offset, viewer_id)?;
                            if let Some((seq, text)) =
                                live.peek_pending_semantic_copy(pane_raw, client_raw)
                            {
                                content.semantic_clipboard = Some(text);
                                content.semantic_clipboard_seq = Some(seq);
                            }
                            Some(content)
                        })
                    })
                    .ok_or_else(|| {
                        ControlError::new(
                            ControlErrorCode::InputRouteUnavailable,
                            "server-owned pane content is unavailable",
                        )
                    })?;
                Ok(ControlResponseData::PaneStyled { content })
            }
            ControlRequest::Split {
                window_id: window_raw,
                target_pane_id,
                axis,
                ratio,
                spawn,
                client_id: client_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                spawn.validate()?;
                let window = window_id(window_raw)?;
                let target = pane_id(target_pane_id)?;
                if let Some(raw) = client_raw {
                    let client = self.require_active_client(raw)?;
                    self.domain
                        .require_controller(target, client)
                        .map_err(control_domain_error)?;
                }
                let bound = self.window_bounds(window)?;
                let split_agent_id = self.agent_id_for_window(window);
                let new_pane = self
                    .domain
                    .split_pane(
                        window,
                        target,
                        axis.into(),
                        ratio,
                        Some((
                            bound.cols as usize,
                            bound.rows as usize,
                            DEFAULT_MIN_COLS,
                            DEFAULT_MIN_ROWS,
                        )),
                    )
                    .map_err(control_domain_error)?;
                self.spawn.insert(new_pane.get(), spawn.clone());
                let geometry = self.window_geometry(window)?;
                if let Some(live) = self.live.as_mut() {
                    let new_geometry = geometry
                        .iter()
                        .find(|pane| pane.pane_id == new_pane.get())
                        .copied()
                        .ok_or_else(|| stale_id("new pane geometry", new_pane.get()))?;
                    let live_result = live
                        .spawn_pane(
                            new_geometry,
                            &spawn,
                            Some(target.get()),
                            split_agent_id.as_deref(),
                        )
                        .and_then(|()| live.apply_geometry(&geometry, None, None));
                    if let Err(error) = live_result {
                        live.remove_pane(new_pane.get());
                        self.spawn.remove(&new_pane.get());
                        if let Err(rollback) = self.domain.close_pane(window, new_pane, target) {
                            return Err(ControlError::new(
                                ControlErrorCode::Internal,
                                format!(
                                    "live split failed ({error:#}) and topology rollback failed ({rollback})"
                                ),
                            ));
                        }
                        return Err(live_internal_error(error));
                    }
                }
                let sequence = self.emit(Event::PaneSplit {
                    window_id: window_raw,
                    target_pane_id,
                    new_pane_id: new_pane.get(),
                    axis,
                    ratio,
                    spawn,
                    geometry,
                });
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck { sequence },
                })
            }
            ControlRequest::Close {
                window_id: window_raw,
                pane_id: pane_raw,
                prior_focus_id,
                client_id: client_raw,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let window = window_id(window_raw)?;
                let pane = pane_id(pane_raw)?;
                let prior_focus = pane_id(prior_focus_id)?;
                if let Some(raw) = client_raw {
                    let client = self.require_active_client(raw)?;
                    self.domain
                        .require_controller(pane, client)
                        .map_err(control_domain_error)?;
                }
                let layout = &self
                    .domain
                    .window(window)
                    .ok_or_else(|| stale_id("window", window_raw))?
                    .layout;
                if !layout.contains_pane(prior_focus) {
                    return Err(stale_id("prior focus pane", prior_focus_id));
                }
                let prior_domain = self.domain.clone();
                let suggested_focus = self
                    .domain
                    .close_pane(window, pane, prior_focus)
                    .map_err(control_domain_error)?;
                let geometry = self.window_geometry(window)?;
                if let Some(live) = self.live.as_mut() {
                    if let Err(error) = live.apply_geometry(&geometry, None, None) {
                        self.domain = prior_domain;
                        return Err(live_internal_error(error));
                    }
                    live.remove_pane(pane_raw);
                }
                self.spawn.remove(&pane_raw);
                self.forget_pane_mail(pane_raw);
                let sequence = self.emit(Event::PaneClosed {
                    window_id: window_raw,
                    pane_id: pane_raw,
                    suggested_focus_id: suggested_focus.get(),
                    geometry,
                });
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck { sequence },
                })
            }
            ControlRequest::Resize {
                window_id: window_raw,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                client_id: client_raw,
                fit,
                host,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let window = window_id(window_raw)?;
                if let Some(raw) = client_raw {
                    let client = self.require_active_client(raw)?;
                    if let Some(window) = self.domain.window(window) {
                        for pane in window.layout.panes() {
                            self.require_client_space(client, pane)?;
                        }
                    }
                }
                let cell_width_px = cell_width_px.filter(|w| *w > 0);
                let cell_height_px = cell_height_px.filter(|h| *h > 0);
                let reported = Viewport {
                    cols,
                    rows,
                    cell_width_px,
                    cell_height_px,
                };
                let current = self
                    .bounds
                    .get(&window_raw)
                    .map(|bound| (bound.cols, bound.rows));
                let owner_before = self.size_owner_for(window_raw);
                let (cols, rows, cell_width_px, cell_height_px) = if let Some(raw) = client_raw {
                    let client = self.require_active_client(raw)?;
                    self.client_viewports
                        .entry(client)
                        .or_default()
                        .insert(window_raw, reported.into());
                    let latest_is_remote = self
                        .latest_client
                        .get(&window_raw)
                        .is_some_and(|latest| !self.host_clients.contains(latest));
                    if host {
                        self.host_clients.insert(client);
                        if record_host_chosen(
                            (reported.cols, reported.rows),
                            current,
                            latest_is_remote,
                        ) {
                            self.host_viewports.insert(window_raw, reported.into());
                        }
                    }
                    self.latest_client.insert(window_raw, client);
                    let role = if host || self.host_clients.contains(&client) {
                        ClientRole::Host
                    } else {
                        ClientRole::Remote
                    };
                    let host_size = self.host_viewport(window_raw);
                    match resize_decision(self.remote_size, role, reported, fit, host_size, current)
                    {
                        Some(view) => (
                            view.cols,
                            view.rows,
                            view.cell_width_px,
                            view.cell_height_px,
                        ),
                        None => {
                            let sequence = self
                                .publish_size_owner(window_raw, owner_before)
                                .unwrap_or(self.sequence);
                            return Ok(ControlResponseData::Mutation {
                                ack: MutationAck { sequence },
                            });
                        }
                    }
                } else if current == Some((cols, rows)) {
                    return Ok(ControlResponseData::Mutation {
                        ack: MutationAck {
                            sequence: self.sequence,
                        },
                    });
                } else {
                    (cols, rows, cell_width_px, cell_height_px)
                };
                let bound = WindowBounds {
                    window_id: window_raw,
                    cols,
                    rows,
                };
                validate_bounds(bound)?;
                let layout = &self
                    .domain
                    .window(window)
                    .ok_or_else(|| stale_id("window", window_raw))?
                    .layout;
                layout_to_rects(layout, rect_for(bound), DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS)
                    .map_err(control_geometry_error)?;
                let previous_bound = self.bounds.insert(window_raw, bound);
                let geometry = match self.window_geometry(window) {
                    Ok(geometry) => geometry,
                    Err(error) => {
                        if let Some(previous) = previous_bound {
                            self.bounds.insert(window_raw, previous);
                        }
                        return Err(error);
                    }
                };
                let owner = self.size_owner_for(window_raw);
                if let Some(live) = self.live.as_mut() {
                    live.set_pending_size_owner(owner);
                    if let Err(error) =
                        live.apply_geometry(&geometry, cell_width_px, cell_height_px)
                    {
                        if let Some(previous) = previous_bound {
                            self.bounds.insert(window_raw, previous);
                        }
                        return Err(live_internal_error(error));
                    }
                }
                let sequence = self.emit(Event::GeometryChanged {
                    window_id: window_raw,
                    bounds: bound,
                    geometry,
                });
                let sequence = self
                    .publish_size_owner(window_raw, owner_before)
                    .unwrap_or(sequence);
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck { sequence },
                })
            }
            ControlRequest::SuggestFocus {
                window_id: window_raw,
                pane_id: pane_raw,
                reason,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let window = window_id(window_raw)?;
                let pane = pane_id(pane_raw)?;
                let layout = &self
                    .domain
                    .window(window)
                    .ok_or_else(|| stale_id("window", window_raw))?
                    .layout;
                if !layout.contains_pane(pane) {
                    return Err(stale_id("pane", pane_raw));
                }
                let sequence = self.emit(Event::FocusSuggested {
                    window_id: window_raw,
                    pane_id: pane_raw,
                    reason,
                });
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck { sequence },
                })
            }
            ControlRequest::ReportFocus {
                client_id: client_raw,
                window_id: window_raw,
                pane_id: pane_raw,
                ..
            } => {
                let client = self.require_active_client(client_raw)?;
                let window = window_id(window_raw)?;
                let pane = pane_id(pane_raw)?;
                let layout = &self
                    .domain
                    .window(window)
                    .ok_or_else(|| stale_id("window", window_raw))?
                    .layout;
                if !layout.contains_pane(pane) {
                    return Err(stale_id("pane", pane_raw));
                }
                let previous = self.client_pane_focus.insert(client, pane_raw);
                if let Some(previous_pane) = previous.filter(|prior| *prior != pane_raw) {
                    if let Some(viewer_id) = self.viewer_ids.get(&client).copied() {
                        if let Some(live) = self.live.as_mut() {
                            let _ = live.revoke_viewer_on(previous_pane, viewer_id);
                        }
                    }
                }
                if previous != Some(pane_raw) {
                    self.ensure_sequence_capacity()?;
                    let _ = self.emit(Event::FocusReported {
                        client_id: client_raw,
                        window_id: window_raw,
                        pane_id: pane_raw,
                    });
                }
                Ok(self.mail_attention_response(pane_raw))
            }
            ControlRequest::RaiseAttention {
                client_id: client_raw,
                pane_id: pane_raw,
                message,
                ..
            } => self.raise_attention(client_raw, pane_raw, message),
            ControlRequest::AttentionRequests { client_id, .. } => {
                self.require_active_client(client_id)?;
                self.team_attention.retain(|pane, _| {
                    pane_id(*pane)
                        .ok()
                        .is_some_and(|id| self.domain.pane(id).is_some())
                });
                Ok(ControlResponseData::AttentionRequests {
                    requests: self.team_attention.values().cloned().collect(),
                })
            }
            ControlRequest::UpdateAttention {
                client_id,
                pane_id,
                revision,
                expected_space,
                expected_session,
                action,
                ..
            } => self.update_team_attention(
                client_id,
                pane_id,
                revision,
                expected_space,
                expected_session,
                action,
            ),
            ControlRequest::MailAttentionSet {
                client_id,
                pane_id,
                cell,
                gen,
                queue_rev,
                depth,
                wake,
                bound_pid,
                ..
            } => self.apply_mail_attention_set(
                client_id, pane_id, cell, gen, queue_rev, depth, wake, bound_pid,
            ),
            ControlRequest::MailAttentionClear {
                client_id,
                pane_id,
                queue_rev,
                ..
            } => self.apply_mail_attention_clear(client_id, pane_id, queue_rev),
            ControlRequest::InjectMail {
                client_id,
                pane_id,
                queue_rev,
                remaining_attempts,
                ..
            } => self.apply_mail_inject(client_id, pane_id, queue_rev, remaining_attempts),
            ControlRequest::RichFocusToggle {
                client_id: client_raw,
                pane_id: pane_raw,
                revoke,
                ..
            } => self.rich_focus_toggle(client_raw, pane_raw, revoke),
            ControlRequest::RichInput {
                client_id: client_raw,
                pane_id: pane_raw,
                input,
                ..
            } => self.rich_input(client_raw, pane_raw, input),
            ControlRequest::CopySemantic {
                client_id: client_raw,
                pane_id: pane_raw,
                seq,
                ..
            } => self.copy_semantic(client_raw, pane_raw, seq),
            ControlRequest::SessionNames { .. } => {
                let mut names = self
                    .mail_store
                    .reserved_addresses()
                    .map_err(Self::mail_store_error)?;
                names.extend(
                    self.domain
                        .sessions()
                        .flat_map(|s| std::iter::once(s.name.clone()).chain(s.agent_id.clone())),
                );
                names.extend(self.mail_aliases.keys().cloned());
                names.sort();
                names.dedup();
                Ok(ControlResponseData::SessionNames { names })
            }
            ControlRequest::NameSession {
                session_id, name, ..
            } => self.name_session(session_id, name),
            ControlRequest::CreateSession {
                name,
                spawn,
                cols,
                rows,
                agent_id,
                headless,
                ..
            } => self.create_session(name, spawn, cols, rows, agent_id, headless),
            ControlRequest::SetClientSpace {
                client_id,
                space_id,
                ..
            } => {
                let client = self.require_active_client(client_id)?;
                if !crate::valid_space_id(&space_id)
                    || self
                        .client_spaces
                        .get(&client)
                        .is_some_and(|prior| prior != &space_id)
                {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "client Space context cannot change",
                    ));
                }
                self.client_spaces.insert(client, space_id);
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck {
                        sequence: self.sequence,
                    },
                })
            }
            ControlRequest::TransferSpacePane {
                pane_id,
                to_session_id,
                from_space,
                to_space,
                ..
            } => self.transfer_space_pane(pane_id, to_session_id, &from_space, &to_space),
            ControlRequest::TransferSpaceSessions {
                session_ids,
                from,
                to,
                ..
            } => {
                self.ensure_sequence_capacity()?;
                let ids = session_ids
                    .iter()
                    .copied()
                    .map(session_id)
                    .collect::<Result<Vec<_>, _>>()?;
                self.domain
                    .transfer_space_sessions(&ids, from.as_deref(), to.as_deref())
                    .map_err(control_domain_error)?;
                let panes: Vec<_> = ids
                    .iter()
                    .filter_map(|id| self.domain.session(*id))
                    .flat_map(|session| &session.windows)
                    .filter_map(|id| self.domain.window(*id))
                    .flat_map(|window| window.layout.panes())
                    .collect();
                for pane in panes {
                    self.revoke_space_controller(pane);
                }
                let sequence = self.emit(Event::SpaceOwnershipChanged {
                    session_ids,
                    from,
                    to,
                });
                Ok(ControlResponseData::Mutation {
                    ack: MutationAck { sequence },
                })
            }
            ControlRequest::SwitchSession {
                client_id: client_raw,
                session_id: session_raw,
                ..
            } => self.switch_session(client_raw, session_raw),
            ControlRequest::DestroySession {
                session_id: session_raw,
                ..
            } => self.destroy_session(session_raw),
            ControlRequest::ShutdownIdle { client_id, .. } => {
                self.require_active_client(client_id)?;
                if self.domain.sessions().next().is_some() {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "sessions are running; daemon restart deferred",
                    ));
                }
                self.shutdown_pending = true;
                Ok(ControlResponseData::ShutdownAccepted)
            }
            ControlRequest::ShutdownServer {
                client_id: client_raw,
                ..
            } => {
                let _ = self.require_active_client(client_raw)?;
                Ok(ControlResponseData::ShutdownAccepted)
            }
            ControlRequest::CreateWindow {
                session_id,
                title,
                spawn,
                cols,
                rows,
                ..
            } => self.create_window(session_id, title, spawn, cols, rows),
            ControlRequest::DestroyWindow { window_id, .. } => self.destroy_window(window_id),
            ControlRequest::MovePane {
                from_window_id,
                to_window_id,
                pane_id,
                target_pane_id,
                axis,
                ratio,
                client_id,
                ..
            } => self.move_pane(
                from_window_id,
                to_window_id,
                pane_id,
                target_pane_id,
                axis,
                ratio,
                client_id,
            ),
            ControlRequest::ApplyArrangement {
                window_id: window_raw,
                kind,
                focused_pane_id,
                ..
            } => self.apply_arrangement(window_raw, kind, focused_pane_id),
            ControlRequest::SwitchWindow {
                client_id,
                window_id,
                ..
            } => self.switch_window(client_id, window_id),
            ControlRequest::RenameWindow {
                window_id, title, ..
            } => self.rename_window(window_id, title),
            ControlRequest::SetSyncInput {
                client_id: client_raw,
                window_id,
                enabled,
                ..
            } => self.set_sync_input(client_raw, window_id, enabled),
            ControlRequest::SetPaneStatus { pane_id, text, .. } => {
                self.set_pane_status(pane_id, text)
            }
            ControlRequest::RenamePane { pane_id, title, .. } => self.rename_pane(pane_id, title),
            ControlRequest::MailHello {
                client_id: client_raw,
                agent,
                ..
            } => self.mail_hello(client_raw, agent),
            ControlRequest::MailSend {
                client_id: client_raw,
                to,
                summary,
                body,
                ..
            } => self.mail_send(client_raw, to, summary, body),
            ControlRequest::MailClaim {
                client_id: client_raw,
                ..
            } => self.mail_claim(client_raw),
            ControlRequest::MailCommit {
                client_id: client_raw,
                ids,
                ..
            } => self.mail_commit(client_raw, ids),
            ControlRequest::MailRelease {
                client_id: client_raw,
                ids,
                ..
            } => self.mail_release(client_raw, ids),
            ControlRequest::MailInbox {
                client_id: client_raw,
                ..
            } => self.mail_inbox(client_raw),
            ControlRequest::MailWait { .. } => Err(ControlError::new(
                ControlErrorCode::Internal,
                "MailWait is served on the client thread, never under the plane lock",
            )),
            ControlRequest::MailWho {
                client_id: client_raw,
                ..
            } => self.mail_who(client_raw),
            ControlRequest::MailAlias {
                client_id: client_raw,
                name,
                ..
            } => self.mail_alias(client_raw, name),
            ControlRequest::MailBroadcast {
                client_id: client_raw,
                summary,
                body,
                ..
            } => self.mail_broadcast(client_raw, summary, body),
        }
    }

    fn rich_focus_toggle(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        revoke: bool,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        self.domain
            .require_controller(pane, client)
            .map_err(control_domain_error)?;
        let viewer_id = self.viewer_ids.get(&client).copied().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::StaleId,
                "registered client has no viewer identity",
            )
        })?;
        let live = self.live.as_mut().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "controller authority verified, but this plane has no live PTY runtime",
            )
        })?;
        match live.rich_focus(pane_raw, viewer_id, revoke) {
            Ok((granted, region_id, structured)) => Ok(ControlResponseData::RichFocus {
                pane_id: pane_raw,
                granted,
                region_id,
                structured,
            }),
            Err(LiveWriteError::Backpressure) => Err(ControlError::new(
                ControlErrorCode::Backpressure,
                "pane input queue is full; retry later",
            )),
            Err(LiveWriteError::UnknownPane | LiveWriteError::Disconnected) => {
                Err(ControlError::new(
                    ControlErrorCode::InputRouteUnavailable,
                    "live PTY input route is unavailable",
                ))
            }
        }
    }

    fn copy_semantic(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        seq: Option<u64>,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        self.domain
            .require_controller(pane, client)
            .map_err(control_domain_error)?;
        let live = self.live.as_mut().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "this plane has no live PTY runtime",
            )
        })?;
        let (text, acked) = live.copy_semantic_for(pane_raw, client_raw, seq);
        Ok(ControlResponseData::SemanticCopy {
            pane_id: pane_raw,
            text,
            seq: acked,
        })
    }

    fn rich_input(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        input: RichInputKind,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        self.domain
            .require_controller(pane, client)
            .map_err(control_domain_error)?;
        let viewer_id = self.viewer_ids.get(&client).copied().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::StaleId,
                "registered client has no viewer identity",
            )
        })?;
        let live = self.live.as_mut().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "controller authority verified, but this plane has no live PTY runtime",
            )
        })?;
        let copy_key = matches!(
            &input,
            RichInputKind::Key { key, modifiers }
                if *modifiers == 0 && (key == "y" || key == "Y")
        );
        let routed = match input {
            RichInputKind::Key { key, modifiers } => {
                if key.is_empty() || key.len() > prismattyc_protocol::MAX_FOCUS_KEY_BYTES {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "structured key token must be 1..=32 bytes",
                    ));
                }
                let modifiers = InputModifiers::new(modifiers).ok_or_else(|| {
                    ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "structured key modifiers contain unknown bits",
                    )
                })?;
                live.rich_key(pane_raw, viewer_id, key, modifiers)
            }
            RichInputKind::Pointer { phase, row, col } => live.rich_pointer(
                pane_raw,
                viewer_id,
                match phase {
                    RichPointerPhase::Press => PointerPhase::Press,
                    RichPointerPhase::Move => PointerPhase::Move,
                    RichPointerPhase::Release => PointerPhase::Release,
                },
                row,
                col,
            ),
            RichInputKind::Scroll { row, col, delta } => {
                if delta == 0 {
                    return Err(ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "structured scroll delta must be non-zero",
                    ));
                }
                live.rich_scroll(pane_raw, viewer_id, row, col, delta)
            }
        };
        match routed {
            Ok(Some(delivered)) => {
                if delivered && copy_key {
                    live.queue_copy_request(pane_raw, client_raw);
                }
                Ok(ControlResponseData::RichInput {
                    pane_id: pane_raw,
                    delivered,
                })
            }
            Ok(None) => Err(ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "viewer has no structured focus route for this pane",
            )),
            Err(LiveWriteError::Backpressure) => Err(ControlError::new(
                ControlErrorCode::Backpressure,
                "pane input queue is full; retry later",
            )),
            Err(LiveWriteError::UnknownPane | LiveWriteError::Disconnected) => {
                Err(ControlError::new(
                    ControlErrorCode::InputRouteUnavailable,
                    "live PTY input route is unavailable",
                ))
            }
        }
    }

    fn name_session(
        &mut self,
        raw: u64,
        name: String,
    ) -> Result<ControlResponseData, ControlError> {
        let name = crate::mailbox::AgentId::new(name.trim().to_string())
            .map_err(|e| ControlError::new(ControlErrorCode::InvalidRequest, e.to_string()))?
            .to_string();
        let id = session_id(raw)?;
        let old = self
            .domain
            .session(id)
            .ok_or_else(|| stale_id("session", raw))?
            .clone();
        let old_agent = old.agent_id.as_deref().unwrap_or(&name);
        let resolved = self
            .mail_store
            .resolve(&name)
            .map_err(Self::mail_store_error)?;
        if (resolved != name && resolved != old_agent)
            || self
                .mail_aliases
                .get(&name)
                .is_some_and(|owner| owner != old_agent)
        {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("mailbox address {name:?} is already reserved"),
            ));
        }
        // Validate topology first. After the durable mail transaction commits,
        // swapping the validated domain cannot fail.
        let mut domain = self.domain.clone();
        domain
            .name_session(id, &name)
            .map_err(control_domain_error)?;
        self.ensure_sequence_capacity()?;
        self.mail_store
            .rename(old_agent, &name)
            .map_err(Self::mail_store_error)?;
        for owner in self.mail_aliases.values_mut() {
            if owner == old_agent {
                *owner = name.clone();
            }
        }
        self.mail_aliases.remove(&name);
        self.domain = domain;
        crate::mailbox::agents::unbind(&old.name);
        crate::mailbox::agents::bind(&name, &name);
        self.refresh_mail_attention(&name);
        self.mail_watch.ring();
        let sequence = self.emit(Event::SessionNamed {
            session_id: raw,
            name,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn create_session(
        &mut self,
        name: String,
        spawn: SpawnSpec,
        cols: Option<u32>,
        rows: Option<u32>,
        agent_id: Option<String>,
        headless: bool,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        if !headless {
            spawn.validate()?;
        }
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || name.contains('\0') {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "session name must be 1..=64 bytes and contain no NUL",
            ));
        }
        if let Some(agent) = &agent_id {
            let resolved = self
                .mail_store
                .resolve(agent)
                .map_err(Self::mail_store_error)?;
            if resolved != *agent || self.mail_aliases.contains_key(agent) {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    format!("mailbox address {agent:?} is already reserved"),
                ));
            }
        }
        if self.domain.sessions().any(|session| session.name == name) {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("session name {name:?} already exists"),
            ));
        }
        let cols = cols.unwrap_or_else(|| {
            self.bounds
                .values()
                .next()
                .map(|bound| bound.cols)
                .unwrap_or(80)
        });
        let rows = rows.unwrap_or_else(|| {
            self.bounds
                .values()
                .next()
                .map(|bound| bound.rows)
                .unwrap_or(24)
        });
        let session = self
            .domain
            .create_session(name)
            .map_err(control_domain_error)?;
        if let Err(error) = self.domain.set_agent_id(session, agent_id.clone()) {
            let _ = self.domain.destroy_session(session);
            return Err(control_domain_error(error));
        }
        if let Some(agent_id) = agent_id.as_deref() {
            crate::mailbox::agents::bind(name, agent_id);
        }
        if headless {
            self.emit(Event::SessionCreated {
                session_id: session.get(),
                name: name.to_string(),
                window_id: 0,
                pane_id: 0,
                bounds: WindowBounds {
                    window_id: 0,
                    cols: 80,
                    rows: 24,
                },
                spawn,
            });
            return Ok(ControlResponseData::Session {
                session_id: session.get(),
                name: name.to_string(),
                window_id: 0,
                pane_id: 0,
            });
        }
        let (window, pane) = match self.domain.create_window(session, "main") {
            Ok(ids) => ids,
            Err(error) => {
                let _ = self.domain.destroy_session(session);
                return Err(control_domain_error(error));
            }
        };
        let bounds = WindowBounds {
            window_id: window.get(),
            cols,
            rows,
        };
        if let Err(error) = validate_bounds(bounds) {
            let _ = self.domain.destroy_session(session);
            return Err(error);
        }
        self.bounds.insert(window.get(), bounds);
        if let Err(error) = self.window_geometry(window) {
            self.bounds.remove(&window.get());
            let _ = self.domain.destroy_session(session);
            return Err(error);
        }
        self.spawn.insert(pane.get(), spawn.clone());
        let live_geometry = match self.window_geometry(window) {
            Ok(geometry) => geometry,
            Err(error) => {
                self.spawn.remove(&pane.get());
                self.bounds.remove(&window.get());
                let _ = self.domain.destroy_session(session);
                return Err(error);
            }
        };
        if let Some(live) = self.live.as_mut() {
            let pane_geometry = live_geometry[0];
            if let Err(error) = live.spawn_pane(pane_geometry, &spawn, None, agent_id.as_deref()) {
                live.remove_pane(pane.get());
                self.spawn.remove(&pane.get());
                self.bounds.remove(&window.get());
                let _ = self.domain.destroy_session(session);
                return Err(live_internal_error(error));
            }
        }
        self.emit(Event::SessionCreated {
            session_id: session.get(),
            name: name.to_string(),
            window_id: window.get(),
            pane_id: pane.get(),
            bounds,
            spawn,
        });
        if let Some(agent) = agent_id.as_deref() {
            self.rearm_queued_mail_for_agent(agent);
        }
        Ok(ControlResponseData::Session {
            session_id: session.get(),
            name: name.to_string(),
            window_id: window.get(),
            pane_id: pane.get(),
        })
    }

    fn switch_session(
        &mut self,
        client_raw: u64,
        session_raw: u64,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let client = self.require_active_client(client_raw)?;
        let session = session_id(session_raw)?;
        let (name, window, pane) = {
            let sess = self
                .domain
                .session(session)
                .ok_or_else(|| stale_id("session", session_raw))?;
            let window = *sess
                .windows
                .first()
                .ok_or_else(|| stale_id("session window", session_raw))?;
            let layout = &self
                .domain
                .window(window)
                .ok_or_else(|| stale_id("window", window.get()))?
                .layout;
            let pane = *layout
                .panes()
                .first()
                .ok_or_else(|| stale_id("session pane", session_raw))?;
            (sess.name.clone(), window, pane)
        };
        if let Some(previous_pane) = self.client_pane_focus.remove(&client) {
            if let Some(viewer_id) = self.viewer_ids.get(&client).copied() {
                if let Some(live) = self.live.as_mut() {
                    let _ = live.revoke_viewer_on(previous_pane, viewer_id);
                }
            }
        }
        self.client_views.insert(client, session);
        self.emit(Event::SessionSwitched {
            client_id: client_raw,
            session_id: session_raw,
            window_id: window.get(),
            pane_id: pane.get(),
        });
        Ok(ControlResponseData::Session {
            session_id: session_raw,
            name,
            window_id: window.get(),
            pane_id: pane.get(),
        })
    }

    fn destroy_session(&mut self, session_raw: u64) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let session = session_id(session_raw)?;
        let (name, windows, panes) = {
            let sess = self
                .domain
                .session(session)
                .ok_or_else(|| stale_id("session", session_raw))?;
            let windows = sess.windows.clone();
            let mut panes = Vec::new();
            for window in &windows {
                let layout = &self
                    .domain
                    .window(*window)
                    .ok_or_else(|| stale_id("window", window.get()))?
                    .layout;
                panes.extend(layout.panes());
            }
            (sess.name.clone(), windows, panes)
        };
        crate::mailbox::agents::unbind(&name);
        self.domain
            .destroy_session(session)
            .map_err(control_domain_error)?;
        for window in &windows {
            self.bounds.remove(&window.get());
            self.client_window_views
                .retain(|_, selected| *selected != *window);
        }
        for pane in &panes {
            self.spawn.remove(&pane.get());
            if let Some(live) = self.live.as_mut() {
                live.remove_pane(pane.get());
            }
            self.forget_pane_mail(pane.get());
        }
        self.client_views.retain(|_, selected| *selected != session);
        let sequence = self.emit(Event::SessionDestroyed {
            session_id: session_raw,
            name,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn create_window(
        &mut self,
        session_raw: u64,
        title: String,
        spawn: SpawnSpec,
        cols: Option<u32>,
        rows: Option<u32>,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        spawn.validate()?;
        let title = title.trim();
        if title.is_empty() || title.len() > 64 || title.contains('\0') {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "window title must be 1..=64 bytes and contain no NUL",
            ));
        }
        let session = session_id(session_raw)?;
        if self.domain.session(session).is_none() {
            return Err(stale_id("session", session_raw));
        }
        let cols = cols.unwrap_or_else(|| {
            self.bounds
                .values()
                .next()
                .map(|bound| bound.cols)
                .unwrap_or(80)
        });
        let rows = rows.unwrap_or_else(|| {
            self.bounds
                .values()
                .next()
                .map(|bound| bound.rows)
                .unwrap_or(24)
        });
        let (window, pane) = self
            .domain
            .create_window(session, title)
            .map_err(control_domain_error)?;
        let bounds = WindowBounds {
            window_id: window.get(),
            cols,
            rows,
        };
        if let Err(error) = validate_bounds(bounds) {
            let _ = self.domain.destroy_window(window);
            return Err(error);
        }
        self.bounds.insert(window.get(), bounds);
        if let Err(error) = self.window_geometry(window) {
            self.bounds.remove(&window.get());
            let _ = self.domain.destroy_window(window);
            return Err(error);
        }
        self.spawn.insert(pane.get(), spawn.clone());
        let live_geometry = match self.window_geometry(window) {
            Ok(geometry) => geometry,
            Err(error) => {
                self.spawn.remove(&pane.get());
                self.bounds.remove(&window.get());
                let _ = self.domain.destroy_window(window);
                return Err(error);
            }
        };
        if let Some(live) = self.live.as_mut() {
            let pane_geometry = live_geometry[0];
            let agent_id = self
                .domain
                .session(session)
                .and_then(|session| session.agent_id.clone());
            if let Err(error) = live.spawn_pane(pane_geometry, &spawn, None, agent_id.as_deref()) {
                live.remove_pane(pane.get());
                self.spawn.remove(&pane.get());
                self.bounds.remove(&window.get());
                let _ = self.domain.destroy_window(window);
                return Err(live_internal_error(error));
            }
        }
        self.emit(Event::WindowCreated {
            session_id: session_raw,
            window_id: window.get(),
            pane_id: pane.get(),
            title: title.to_string(),
            bounds,
            spawn,
        });
        Ok(ControlResponseData::Window {
            session_id: session_raw,
            window_id: window.get(),
            pane_id: pane.get(),
            title: title.to_string(),
        })
    }

    fn destroy_window(&mut self, window_raw: u64) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let window = window_id(window_raw)?;
        let layout = self
            .domain
            .window(window)
            .ok_or_else(|| stale_id("window", window_raw))?
            .layout
            .clone();
        let panes = layout.panes();
        let session_raw = self
            .domain
            .sessions()
            .find(|session| session.windows.contains(&window))
            .map(|session| session.id.get())
            .ok_or_else(|| stale_id("window session", window_raw))?;
        self.domain
            .destroy_window(window)
            .map_err(control_domain_error)?;
        self.bounds.remove(&window_raw);
        for pane in &panes {
            self.spawn.remove(&pane.get());
            if let Some(live) = self.live.as_mut() {
                live.remove_pane(pane.get());
            }
            self.forget_pane_mail(pane.get());
        }
        self.client_window_views
            .retain(|_, selected| *selected != window);
        let session_destroyed = self.domain.session(session_id(session_raw)?).is_none();
        let sequence = self.emit(Event::WindowDestroyed {
            session_id: session_raw,
            window_id: window_raw,
            session_destroyed,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn apply_arrangement(
        &mut self,
        window_raw: u64,
        kind: ArrangementWire,
        focused_pane_id: Option<u64>,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let window = window_id(window_raw)?;
        let layout = self
            .domain
            .window(window)
            .ok_or_else(|| stale_id("window", window_raw))?
            .layout
            .clone();
        let leaves = layout.panes();
        let main = focused_pane_id.and_then(|raw| pane_id(raw).ok());
        let new_layout =
            apply_arrangement(kind.into(), &leaves, main).map_err(control_geometry_error)?;
        let bound = self.window_bounds(window)?;
        layout_to_rects(
            &new_layout,
            rect_for(bound),
            DEFAULT_MIN_COLS,
            DEFAULT_MIN_ROWS,
        )
        .map_err(control_geometry_error)?;
        let prior = layout;
        self.domain
            .set_layout(window, new_layout)
            .map_err(control_domain_error)?;
        let geometry = match self.window_geometry(window) {
            Ok(geometry) => geometry,
            Err(error) => {
                let _ = self.domain.set_layout(window, prior);
                return Err(error);
            }
        };
        if let Some(live) = self.live.as_mut() {
            if let Err(error) = live.apply_geometry(&geometry, None, None) {
                let _ = self.domain.set_layout(window, prior);
                return Err(live_internal_error(error));
            }
        }
        let sequence = self.emit(Event::GeometryChanged {
            window_id: window_raw,
            bounds: bound,
            geometry,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn revoke_space_controller(&mut self, pane: PaneId) {
        if let Ok(Some(client)) = self.domain.controller(pane) {
            let _ = self.domain.release_controller(pane, client);
            if let Some(live) = self.live.as_mut() {
                live.note_controller(pane.get(), None);
                if let Some(viewer) = self.viewer_ids.get(&client) {
                    let _ = live.revoke_viewer_on(pane.get(), *viewer);
                }
            }
        }
    }

    fn transfer_space_pane(
        &mut self,
        pane_raw: u64,
        target_raw: u64,
        from_space: &str,
        to_space: &str,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let pane = pane_id(pane_raw)?;
        let target = session_id(target_raw)?;
        let source_window = self
            .domain
            .pane_owner(pane)
            .ok_or_else(|| stale_id("pane", pane_raw))?;
        let source = self
            .domain
            .sessions()
            .find(|s| s.windows.contains(&source_window))
            .ok_or_else(|| stale_id("source session", pane_raw))?
            .clone();
        let destination = self
            .domain
            .session(target)
            .ok_or_else(|| stale_id("session", target_raw))?
            .clone();
        if source.space_id.as_deref() != Some(from_space)
            || destination.space_id.as_deref() != Some(to_space)
            || from_space == to_space
        {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "pane transfer Space owner changed",
            ));
        }
        let mail_endpoint = source
            .windows
            .first()
            .and_then(|w| self.domain.window(*w))
            .and_then(|w| w.layout.panes().first().copied());
        let moved_agent = (mail_endpoint == Some(pane))
            .then(|| source.agent_id.clone())
            .flatten();
        if moved_agent.is_some() && destination.agent_id.is_some() {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                "destination session already has an agent; move to a new session",
            ));
        }
        let prior_domain = self.domain.clone();
        let prior_bounds = self.bounds.clone();
        let result = if let Some(window) = destination.windows.first() {
            let leaf = self
                .domain
                .window(*window)
                .and_then(|w| w.layout.panes().first().copied())
                .ok_or_else(|| stale_id("destination pane", target_raw))?;
            self.move_pane(
                source_window.get(),
                window.get(),
                pane_raw,
                leaf.get(),
                AxisWire::Horizontal,
                0.5,
                None,
            )
        } else {
            let bound = self.window_bounds(source_window)?;
            let title = self
                .domain
                .pane(pane)
                .map(|p| p.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "moved".into());
            let window = self
                .domain
                .open_window_with_pane(target, &title, pane)
                .map_err(control_domain_error)?;
            self.bounds.insert(
                window.get(),
                WindowBounds {
                    window_id: window.get(),
                    ..bound
                },
            );
            if self.domain.window(source_window).is_none() {
                self.bounds.remove(&source_window.get());
            }
            let geometry = match self.moved_geometry(source_window, window) {
                Ok(geometry) => geometry,
                Err(error) => {
                    self.domain = prior_domain;
                    self.bounds = prior_bounds;
                    return Err(error);
                }
            };
            if let Some(live) = self.live.as_mut() {
                if let Err(error) = live.apply_geometry(&geometry, None, None) {
                    self.domain = prior_domain.clone();
                    self.bounds = prior_bounds;
                    return Err(live_internal_error(error));
                }
            }
            let sequence = self.emit(Event::PaneMoved {
                from_window_id: source_window.get(),
                to_window_id: window.get(),
                pane_id: pane_raw,
                source_suggested_focus_id: None,
                geometry,
            });
            Ok(ControlResponseData::Mutation {
                ack: MutationAck { sequence },
            })
        };
        if result.is_ok() {
            self.revoke_space_controller(pane);
            self.client_window_views
                .retain(|_, window| self.domain.window(*window).is_some());
            if let Some(agent) = moved_agent {
                if self.domain.session(source.id).is_some() {
                    self.domain
                        .set_agent_id(source.id, None)
                        .map_err(control_domain_error)?;
                }
                self.domain
                    .set_agent_id(target, Some(agent.clone()))
                    .map_err(control_domain_error)?;
                crate::mailbox::agents::bind(&destination.name, &agent);
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn move_pane(
        &mut self,
        from_raw: u64,
        to_raw: u64,
        pane_raw: u64,
        target_raw: u64,
        axis: AxisWire,
        ratio: f64,
        client_raw: Option<u64>,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let from_window = window_id(from_raw)?;
        let to_window = window_id(to_raw)?;
        let pane = pane_id(pane_raw)?;
        let target = pane_id(target_raw)?;
        if let Some(raw) = client_raw {
            let client = self.require_active_client(raw)?;
            self.domain
                .require_controller(pane, client)
                .map_err(control_domain_error)?;
        }
        let src_probe = self.window_bounds(from_window).ok().map(|bound| {
            (
                bound.cols as usize,
                bound.rows as usize,
                DEFAULT_MIN_COLS,
                DEFAULT_MIN_ROWS,
            )
        });
        let dst_probe = self.window_bounds(to_window).ok().map(|bound| {
            (
                bound.cols as usize,
                bound.rows as usize,
                DEFAULT_MIN_COLS,
                DEFAULT_MIN_ROWS,
            )
        });
        let prior_domain = self.domain.clone();
        let prior_bounds = self.bounds.clone();
        let prior_window_views = self.client_window_views.clone();
        let suggested = self
            .domain
            .move_pane(
                from_window,
                to_window,
                pane,
                target,
                axis.into(),
                ratio,
                src_probe,
                dst_probe,
            )
            .map_err(control_domain_error)?;
        if self.domain.window(from_window).is_none() {
            self.bounds.remove(&from_raw);
            self.client_window_views
                .retain(|_, selected| *selected != from_window);
        }
        let geometry = match self.moved_geometry(from_window, to_window) {
            Ok(geometry) => geometry,
            Err(error) => {
                self.domain = prior_domain;
                self.bounds = prior_bounds;
                self.client_window_views = prior_window_views;
                return Err(error);
            }
        };
        if let Some(live) = self.live.as_mut() {
            if let Err(error) = live.apply_geometry(&geometry, None, None) {
                self.domain = prior_domain;
                self.bounds = prior_bounds;
                self.client_window_views = prior_window_views;
                return Err(live_internal_error(error));
            }
        }
        let sequence = self.emit(Event::PaneMoved {
            from_window_id: from_raw,
            to_window_id: to_raw,
            pane_id: pane_raw,
            source_suggested_focus_id: suggested.map(|id| id.get()),
            geometry,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn switch_window(
        &mut self,
        client_raw: u64,
        window_raw: u64,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let client = self.require_active_client(client_raw)?;
        let window = window_id(window_raw)?;
        let (session_raw, title, pane) = {
            let win = self
                .domain
                .window(window)
                .ok_or_else(|| stale_id("window", window_raw))?;
            let pane = *win
                .layout
                .panes()
                .first()
                .ok_or_else(|| stale_id("window pane", window_raw))?;
            let title = win.title.clone();
            let session_raw = self
                .domain
                .sessions()
                .find(|session| session.windows.contains(&window))
                .map(|session| session.id.get())
                .ok_or_else(|| stale_id("window session", window_raw))?;
            (session_raw, title, pane)
        };
        if let Some(previous_pane) = self.client_pane_focus.remove(&client) {
            if let Some(viewer_id) = self.viewer_ids.get(&client).copied() {
                if let Some(live) = self.live.as_mut() {
                    let _ = live.revoke_viewer_on(previous_pane, viewer_id);
                }
            }
        }
        self.client_window_views.insert(client, window);
        self.emit(Event::WindowSwitched {
            client_id: client_raw,
            session_id: session_raw,
            window_id: window_raw,
            pane_id: pane.get(),
        });
        Ok(ControlResponseData::Window {
            session_id: session_raw,
            window_id: window_raw,
            pane_id: pane.get(),
            title,
        })
    }

    fn rename_window(
        &mut self,
        window_raw: u64,
        title: String,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let window = window_id(window_raw)?;
        let win = self
            .domain
            .window(window)
            .ok_or_else(|| stale_id("window", window_raw))?;
        let pane = *win
            .layout
            .panes()
            .first()
            .ok_or_else(|| stale_id("window pane", window_raw))?;
        let session_raw = self
            .domain
            .sessions()
            .find(|session| session.windows.contains(&window))
            .map(|session| session.id.get())
            .ok_or_else(|| stale_id("window session", window_raw))?;
        self.domain
            .rename_window(window, &title)
            .map_err(control_domain_error)?;
        let title = self
            .domain
            .window(window)
            .map(|win| win.title.clone())
            .unwrap_or_default();
        self.emit(Event::WindowRenamed {
            window_id: window_raw,
            title: title.clone(),
        });
        Ok(ControlResponseData::Window {
            session_id: session_raw,
            window_id: window_raw,
            pane_id: pane.get(),
            title,
        })
    }

    fn set_sync_input(
        &mut self,
        client_raw: u64,
        window_raw: u64,
        enabled: bool,
    ) -> Result<ControlResponseData, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        self.ensure_sequence_capacity()?;
        let window = window_id(window_raw)?;
        self.domain
            .set_sync_input(window, enabled)
            .map_err(control_domain_error)?;
        let sequence = self.emit(Event::SyncInputChanged {
            window_id: window_raw,
            enabled,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn set_pane_status(
        &mut self,
        pane_raw: u64,
        text: Option<String>,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        let stored = match text {
            None => None,
            Some(raw) => {
                let validated = validate_pane_status(&raw)?;
                if validated.is_empty() {
                    None
                } else {
                    Some(validated)
                }
            }
        };
        match &stored {
            Some(status) => {
                self.pane_status.insert(pane_raw, status.clone());
            }
            None => {
                self.pane_status.remove(&pane_raw);
            }
        }
        if let Some(live) = self.live.as_mut() {
            live.log_status(pane_raw, stored.clone());
        }
        let sequence = self.emit(Event::PaneStatusChanged {
            pane_id: pane_raw,
            status: stored,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn rename_pane(
        &mut self,
        pane_raw: u64,
        title: String,
    ) -> Result<ControlResponseData, ControlError> {
        self.ensure_sequence_capacity()?;
        let pane = pane_id(pane_raw)?;
        if self.domain.pane(pane).is_none() {
            return Err(stale_id("pane", pane_raw));
        }
        let title = validate_pane_title(&title)?;
        self.domain
            .rename_pane(pane, &title)
            .map_err(control_domain_error)?;
        let sequence = self.emit(Event::PaneRenamed {
            pane_id: pane_raw,
            title,
        });
        Ok(ControlResponseData::Mutation {
            ack: MutationAck { sequence },
        })
    }

    fn sync_siblings(&self, pane: PaneId, pane_raw: u64) -> Vec<u64> {
        let Some(owner) = self.domain.pane_owner(pane) else {
            return Vec::new();
        };
        let Some(win) = self.domain.window(owner) else {
            return Vec::new();
        };
        if !win.sync_input {
            return Vec::new();
        }
        win.layout
            .panes()
            .into_iter()
            .map(|id| id.get())
            .filter(|id| *id != pane_raw)
            .collect()
    }

    fn moved_geometry(
        &self,
        from_window: WindowId,
        to_window: WindowId,
    ) -> Result<Vec<PaneGeometry>, ControlError> {
        let mut geometry = Vec::new();
        if self.domain.window(from_window).is_some() {
            geometry.extend(self.window_geometry(from_window)?);
        }
        geometry.extend(self.window_geometry(to_window)?);
        Ok(geometry)
    }

    fn window_bounds(&self, window: WindowId) -> Result<WindowBounds, ControlError> {
        if self.domain.window(window).is_none() {
            return Err(stale_id("window", window.get()));
        }
        self.bounds
            .get(&window.get())
            .copied()
            .ok_or_else(|| stale_id("window geometry", window.get()))
    }

    /// Bound agent of the session that owns `window`, if any. Stamped into
    /// spawned panes as `PMUX_AGENT`.
    fn agent_id_for_window(&self, window: WindowId) -> Option<String> {
        self.domain
            .sessions()
            .find(|session| session.windows.contains(&window))
            .and_then(|session| session.agent_id.clone())
    }

    fn window_geometry(&self, window: WindowId) -> Result<Vec<PaneGeometry>, ControlError> {
        let bound = self.window_bounds(window)?;
        let layout = &self.domain.window(window).expect("checked").layout;
        layout_to_rects(layout, rect_for(bound), DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS)
            .map_err(control_geometry_error)?
            .into_iter()
            .map(|(pane, rect)| pane_geometry(pane, rect))
            .collect()
    }

    fn size_owner_for(&self, window_raw: u64) -> Option<SizeOwner> {
        let client = self.latest_client.get(&window_raw)?;
        let kind = if self.host_clients.contains(client) {
            SizeOwnerKind::Host
        } else {
            SizeOwnerKind::Remote
        };
        Some(SizeOwner {
            client_id: client.get(),
            kind,
        })
    }

    fn publish_size_owner(&mut self, window_raw: u64, previous: Option<SizeOwner>) -> Option<u64> {
        let now = self.size_owner_for(window_raw);
        if now == previous {
            return None;
        }
        let sequence = self.emit(Event::SizeOwnerChanged {
            window_id: window_raw,
            owner: now,
        });
        let panes: Vec<u64> = window_id(window_raw)
            .ok()
            .and_then(|window| self.domain.window(window))
            .map(|win| {
                win.layout
                    .panes()
                    .into_iter()
                    .map(|pane| pane.get())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(live) = self.live.as_mut() {
            for pane_id in panes {
                live.record_size_owner(pane_id, now);
            }
        }
        Some(sequence)
    }

    fn host_viewport(&self, window_raw: u64) -> Option<Viewport> {
        self.host_viewports
            .get(&window_raw)
            .copied()
            .map(Into::into)
    }

    fn remaining_viewport(&self, window_raw: u64) -> Option<Viewport> {
        if let Some(client) = self.latest_client.get(&window_raw) {
            if let Some(view) = self
                .client_viewports
                .get(client)
                .and_then(|views| views.get(&window_raw))
            {
                return Some((*view).into());
            }
        }
        self.client_viewports
            .values()
            .find_map(|views| views.get(&window_raw).copied().map(Into::into))
    }

    fn mark_host_latest(&mut self, pane: PaneId) {
        for session in self.domain.sessions() {
            for window in &session.windows {
                if self
                    .domain
                    .window(*window)
                    .is_some_and(|win| win.layout.panes().contains(&pane))
                {
                    if let Some(client) = self
                        .host_clients
                        .iter()
                        .find(|client| {
                            self.client_viewports
                                .get(client)
                                .is_some_and(|views| views.contains_key(&window.get()))
                        })
                        .copied()
                    {
                        self.latest_client.insert(window.get(), client);
                    }
                }
            }
        }
    }

    fn apply_disconnect_viewport(&mut self, window_raw: u64) -> Result<(), ControlError> {
        let Some(view) = disconnect_decision(
            self.host_viewport(window_raw),
            self.remaining_viewport(window_raw),
        ) else {
            return Ok(());
        };
        self.apply_chosen_viewport(window_raw, view)
    }

    fn apply_chosen_viewport(
        &mut self,
        window_raw: u64,
        view: Viewport,
    ) -> Result<(), ControlError> {
        let Viewport {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } = view;
        let window = window_id(window_raw)?;
        let bound = WindowBounds {
            window_id: window_raw,
            cols,
            rows,
        };
        validate_bounds(bound)?;
        let layout = &self
            .domain
            .window(window)
            .ok_or_else(|| stale_id("window", window_raw))?
            .layout;
        layout_to_rects(layout, rect_for(bound), DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS)
            .map_err(control_geometry_error)?;
        let previous_bound = self.bounds.insert(window_raw, bound);
        if previous_bound.is_some_and(|prev| prev.cols == cols && prev.rows == rows) {
            return Ok(());
        }
        let geometry = match self.window_geometry(window) {
            Ok(geometry) => geometry,
            Err(error) => {
                if let Some(previous) = previous_bound {
                    self.bounds.insert(window_raw, previous);
                }
                return Err(error);
            }
        };
        let owner = self.size_owner_for(window_raw);
        if let Some(live) = self.live.as_mut() {
            live.set_pending_size_owner(owner);
            if let Err(error) = live.apply_geometry(&geometry, cell_width_px, cell_height_px) {
                if let Some(previous) = previous_bound {
                    self.bounds.insert(window_raw, previous);
                }
                return Err(live_internal_error(error));
            }
        }
        let _ = self.emit(Event::GeometryChanged {
            window_id: window_raw,
            bounds: bound,
            geometry,
        });
        Ok(())
    }

    fn all_geometry(&self) -> Result<Vec<PaneGeometry>, ControlError> {
        let mut geometry = Vec::new();
        for session in self.domain.sessions() {
            for window in &session.windows {
                geometry.extend(self.window_geometry(*window)?);
            }
        }
        Ok(geometry)
    }

    fn events_after(
        &self,
        after_sequence: u64,
        limit: Option<usize>,
    ) -> Result<EventBatch, ControlError> {
        let limit = limit.unwrap_or(MAX_EVENT_BATCH);
        if !(1..=MAX_EVENT_BATCH).contains(&limit) {
            return Err(ControlError::new(
                ControlErrorCode::InvalidRequest,
                format!("event limit must be in 1..={MAX_EVENT_BATCH}"),
            ));
        }
        let oldest = self.oldest_available_sequence();
        if after_sequence > self.sequence {
            return Err(ControlError::resnapshot(
                ControlErrorCode::StaleSequence,
                format!(
                    "sequence {after_sequence} is ahead of current {}",
                    self.sequence
                ),
                oldest,
                self.sequence,
            ));
        }
        if after_sequence.saturating_add(1) < oldest {
            return Err(ControlError::resnapshot(
                ControlErrorCode::EventGap,
                format!("events after {after_sequence} are no longer retained; oldest is {oldest}"),
                oldest,
                self.sequence,
            ));
        }
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .take(limit)
            .cloned()
            .collect();
        let through_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(EventBatch {
            after_sequence,
            through_sequence,
            current_sequence: self.sequence,
            has_more: through_sequence < self.sequence,
            events,
        })
    }

    fn subscribe_has_work(
        &self,
        client_raw: u64,
        pane_raw: u64,
        through_seq: u64,
    ) -> Result<Option<()>, ControlError> {
        let _ = self.require_active_client(client_raw)?;
        let live = self.live.as_ref().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "server-owned pane content is unavailable",
            )
        })?;
        if live.pane_gone(pane_raw) {
            return Ok(Some(()));
        }
        match live.subscribe_catch_up(pane_raw, through_seq) {
            Some(CatchUp::Events(events)) if events.is_empty() => Ok(None),
            Some(CatchUp::Ahead { .. }) => Ok(None),
            Some(_) => Ok(Some(())),
            None => Ok(Some(())),
        }
    }

    fn subscribe_frame(
        &self,
        client_raw: u64,
        pane_raw: u64,
        from_seq: u64,
        done: bool,
        gone_is_terminal: bool,
    ) -> Result<(std::sync::Arc<PaneLogWatch>, u64, ControlResponseData), ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        let live = self.live.as_ref().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::InputRouteUnavailable,
                "server-owned pane content is unavailable",
            )
        })?;
        let watch = live.pane_log_watch();
        let gone = self.domain.pane(pane).is_none() || live.pane_gone(pane_raw);
        if gone {
            if gone_is_terminal {
                return Ok((
                    watch,
                    from_seq,
                    ControlResponseData::PaneSubscribe {
                        pane_id: pane_raw,
                        gap: false,
                        snapshot: None,
                        events: Vec::new(),
                        through_seq: from_seq,
                        done: true,
                    },
                ));
            }
            return Err(stale_id("pane", pane_raw));
        }
        let catch = live
            .subscribe_catch_up(pane_raw, from_seq)
            .ok_or_else(|| stale_id("pane", pane_raw))?;
        let (gap, snapshot, events, through_seq) = match catch {
            CatchUp::Ahead { current, oldest } => {
                return Err(ControlError::resnapshot(
                    ControlErrorCode::StaleSequence,
                    format!("pane seq {from_seq} is ahead of current {current}"),
                    oldest.unwrap_or(1),
                    current,
                ));
            }
            CatchUp::Gap { oldest: _, current } => {
                let viewer_id = self.viewer_ids.get(&client).copied();
                let snapshot =
                    viewer_id.and_then(|viewer_id| live.styled(pane_raw, None, viewer_id));
                (true, snapshot, Vec::new(), current)
            }
            CatchUp::Events(frames) => {
                let events: Vec<PaneLogFrame> =
                    frames.into_iter().take(MAX_PANE_LOG_BATCH).collect();
                let through = events.last().map_or(from_seq, |frame| frame.seq);
                (false, None, events, through)
            }
        };
        Ok((
            watch,
            through_seq,
            ControlResponseData::PaneSubscribe {
                pane_id: pane_raw,
                gap,
                snapshot,
                events,
                through_seq,
                done,
            },
        ))
    }

    fn ensure_sequence_capacity(&self) -> Result<(), ControlError> {
        if self.sequence == u64::MAX {
            Err(ControlError::new(
                ControlErrorCode::SequenceExhausted,
                "event sequence space exhausted",
            ))
        } else {
            Ok(())
        }
    }

    fn oldest_available_sequence(&self) -> u64 {
        self.events
            .front()
            .map_or_else(|| self.sequence.saturating_add(1), |event| event.sequence)
    }

    fn snapshot_required_error(&self) -> ControlError {
        ControlError::resnapshot(
            ControlErrorCode::SnapshotRequired,
            "take a fresh snapshot before reading events on this connection",
            self.oldest_available_sequence(),
            self.sequence,
        )
    }

    fn emit(&mut self, event: Event) -> u64 {
        self.sequence += 1;
        if self.events.len() == self.event_capacity {
            self.events.pop_front();
        }
        self.events.push_back(EventEnvelope {
            sequence: self.sequence,
            event,
        });
        self.sequence
    }

    fn note_output_bytes(&mut self, pane_id: u64, nbytes: usize, now: u64) {
        if nbytes == 0 {
            return;
        }
        let queue = self.output_bytes.entry(pane_id).or_default();
        queue.push_back((now, nbytes));
        while queue
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= MAIL_INJECT_QUIET_MS)
        {
            queue.pop_front();
        }
    }

    fn output_bytes_in_quiet_window(&mut self, pane_id: u64, now: u64) -> usize {
        let Some(queue) = self.output_bytes.get_mut(&pane_id) else {
            return 0;
        };
        while queue
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= MAIL_INJECT_QUIET_MS)
        {
            queue.pop_front();
        }
        let total = queue.iter().map(|(_, n)| *n).sum();
        if queue.is_empty() {
            self.output_bytes.remove(&pane_id);
        }
        total
    }

    /// Keep at most one `OutputActivity` per pane in the ring. Busy PTYs must not
    /// evict topology events (control protocol sequence is for mutations; activity is
    /// a coalesced rising-edge hint — use `ReadPane` for content).
    fn emit_output_activity(
        &mut self,
        pane_id: u64,
        revision: u64,
        child_alive: bool,
        nbytes: usize,
    ) {
        let now = now_unix_ms();
        self.last_output_at_ms.insert(pane_id, now);
        self.note_output_bytes(pane_id, nbytes, now);
        if let Some(existing) = self.events.iter_mut().rev().find(|envelope| {
            matches!(
                &envelope.event,
                Event::OutputActivity {
                    pane_id: id,
                    ..
                } if *id == pane_id
            )
        }) {
            if let Event::OutputActivity {
                revision: last_revision,
                child_alive: last_alive,
                ..
            } = &mut existing.event
            {
                *last_revision = revision;
                *last_alive = child_alive;
            }
            return;
        }
        if self.ensure_sequence_capacity().is_ok() {
            self.emit(Event::OutputActivity {
                pane_id,
                revision,
                child_alive,
            });
        }
    }
}

const MAX_ATTENTION_BYTES: usize = 512;

fn validate_attention_message(message: &str) -> Result<(), ControlError> {
    if message.len() > MAX_ATTENTION_BYTES || message.chars().any(char::is_control) {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "attention message must be valid text without control characters and no larger than 512 bytes",
        ));
    }
    Ok(())
}
fn layout_snapshot(layout: &PaneLayout) -> LayoutSnapshot {
    match layout {
        PaneLayout::Leaf(pane) => LayoutSnapshot::Leaf {
            pane_id: pane.get(),
        },
        PaneLayout::Split(split) => LayoutSnapshot::Split {
            axis: match split.axis {
                Axis::Horizontal => AxisWire::Horizontal,
                Axis::Vertical => AxisWire::Vertical,
            },
            ratio: split.ratio,
            first: Box::new(layout_snapshot(&split.first)),
            second: Box::new(layout_snapshot(&split.second)),
        },
    }
}

fn validate_bounds(bounds: WindowBounds) -> Result<(), ControlError> {
    if bounds.cols == 0 || bounds.rows == 0 {
        return Err(ControlError::new(
            ControlErrorCode::TooSmall,
            "window bounds must be non-zero",
        ));
    }
    Ok(())
}

fn live_internal_error(error: anyhow::Error) -> ControlError {
    ControlError::new(
        ControlErrorCode::Internal,
        format!("live pane runtime failed: {error:#}"),
    )
}

/// Arm a Hive supervisor sink from the server environment when `HIVE_SOCKET` is set.
/// Missing/empty socket ⇒ `None` (no Hive traffic). Lease file is optional best-effort.
fn hive_supervisor_from_env() -> Option<SupervisorSink> {
    let path = std::env::var_os("HIVE_SOCKET").map(PathBuf::from)?;
    if path.as_os_str().is_empty() {
        return None;
    }
    let lease = read_operator_lease(&path);
    Some(SupervisorSink::spawn(path, lease))
}

fn rect_for(bounds: WindowBounds) -> CellRect {
    CellRect {
        col: 0,
        row: 0,
        cols: bounds.cols as usize,
        rows: bounds.rows as usize,
    }
}

fn pane_geometry(pane: PaneId, rect: CellRect) -> Result<PaneGeometry, ControlError> {
    Ok(PaneGeometry {
        pane_id: pane.get(),
        col: u32::try_from(rect.col).map_err(|_| geometry_overflow())?,
        row: u32::try_from(rect.row).map_err(|_| geometry_overflow())?,
        cols: u32::try_from(rect.cols).map_err(|_| geometry_overflow())?,
        rows: u32::try_from(rect.rows).map_err(|_| geometry_overflow())?,
    })
}

fn geometry_overflow() -> ControlError {
    ControlError::new(
        ControlErrorCode::InvalidRequest,
        "geometry exceeds wire limits",
    )
}

fn session_id(raw: u64) -> Result<SessionId, ControlError> {
    if raw == 0 {
        Err(stale_id("session", raw))
    } else {
        Ok(SessionId::from_raw(raw))
    }
}

fn window_id(raw: u64) -> Result<WindowId, ControlError> {
    if raw == 0 {
        Err(stale_id("window", raw))
    } else {
        Ok(WindowId::from_raw(raw))
    }
}

fn pane_id(raw: u64) -> Result<PaneId, ControlError> {
    if raw == 0 {
        Err(stale_id("pane", raw))
    } else {
        Ok(PaneId::from_raw(raw))
    }
}

fn client_id(raw: u64) -> Result<ClientId, ControlError> {
    if raw == 0 {
        Err(stale_id("client", raw))
    } else {
        Ok(ClientId::from_raw(raw))
    }
}

fn stale_id(kind: &str, raw: u64) -> ControlError {
    ControlError::new(
        ControlErrorCode::StaleId,
        format!("unknown or stale {kind} id {raw}"),
    )
}

fn control_geometry_error(error: GeometryError) -> ControlError {
    match error {
        GeometryError::TooSmall => ControlError::new(ControlErrorCode::TooSmall, error.to_string()),
        GeometryError::LastPane => ControlError::new(ControlErrorCode::LastPane, error.to_string()),
        GeometryError::UnknownPane(pane) => stale_id("pane", pane.get()),
        GeometryError::InvalidRatio | GeometryError::Overflow => {
            ControlError::new(ControlErrorCode::InvalidRequest, error.to_string())
        }
    }
}

/// Pane titles share the status rules: trimmed, at most 64 bytes, no
/// control characters. Empty (after trim) clears the title.
fn validate_pane_title(text: &str) -> Result<String, ControlError> {
    let trimmed = text.trim();
    if trimmed.len() > 64 {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "pane title must be at most 64 bytes",
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "pane title must not contain control characters",
        ));
    }
    Ok(trimmed.to_string())
}

fn validate_pane_status(text: &str) -> Result<String, ControlError> {
    let trimmed = text.trim();
    if trimmed.len() > 64 {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "pane status must be at most 64 bytes",
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "pane status must not contain control characters",
        ));
    }
    Ok(trimmed.to_string())
}

fn control_domain_error(error: DomainError) -> ControlError {
    match error {
        DomainError::UnknownSession(id) => stale_id("session", id.get()),
        DomainError::UnknownWindow(id) => stale_id("window", id.get()),
        DomainError::UnknownPane(id) => stale_id("pane", id.get()),
        DomainError::Geometry(error) => control_geometry_error(error),
        DomainError::LastLeafRefused => {
            ControlError::new(ControlErrorCode::LastPane, error.to_string())
        }
        DomainError::EmptyName
        | DomainError::InvalidName
        | DomainError::DuplicatePane(_)
        | DomainError::PaneOwnedElsewhere { .. } => {
            ControlError::new(ControlErrorCode::InvalidRequest, error.to_string())
        }
        DomainError::LeaseHeld { holder } => ControlError::new(
            ControlErrorCode::LeaseHeld,
            format!("controller lease held by {holder}"),
        )
        .with_holder(holder.get()),
        DomainError::NotController { holder } => {
            let mapped = ControlError::new(ControlErrorCode::NotController, error.to_string());
            match holder {
                Some(id) => mapped.with_holder(id.get()),
                None => mapped,
            }
        }
        DomainError::IdSpaceExhausted => {
            ControlError::new(ControlErrorCode::Internal, error.to_string())
        }
        DomainError::AgentIdInUse { .. } | DomainError::SpaceOwnerMismatch { .. } => {
            ControlError::new(ControlErrorCode::InvalidRequest, error.to_string())
        }
    }
}

/// Liveness of a candidate control-socket path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketLiveness {
    /// Path does not exist.
    Missing,
    /// Same-uid Unix socket with a peer that accepts connections.
    Live,
    /// Same-uid Unix socket leftover (no listener). Safe to replace.
    Stale,
    /// Non-socket, symlink to something else, or foreign uid. Never unlink.
    Foreign,
}

fn is_stale_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    ) || matches!(error.raw_os_error(), Some(111 | 2 | 104 | 32))
}

/// Probe a control-socket path without unlinking it.
pub fn probe_socket_liveness(path: impl AsRef<Path>) -> SocketLiveness {
    let path = path.as_ref();
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return SocketLiveness::Missing;
    };
    if !owned_socket(path, &metadata) {
        return SocketLiveness::Foreign;
    }
    match UnixStream::connect(path) {
        // A successful connect is not proof of a live server: macOS keeps a
        // crashed server's socket connectable while its accept queue drains, so
        // connect lands in a backlog no one will ever accept. Require the peer
        // to answer the versioned handshake before declaring it Live.
        Ok(stream) if control_peer_answers(&stream) => SocketLiveness::Live,
        Ok(_) => SocketLiveness::Stale,
        Err(error) if is_stale_connect_error(&error) => SocketLiveness::Stale,
        // Conservatively treat other connect failures as occupied so bind
        // does not steal a socket we could not diagnose.
        Err(_) => SocketLiveness::Live,
    }
}

/// Whether a connected control socket answers the versioned handshake within
/// `IO_TIMEOUT`. A peer that never replies — a crashed server whose accept queue
/// is still draining, or a same-uid socket that does not speak this protocol —
/// is not Live. The client socket is blocking, so `set_read_timeout` bounds the
/// wait via `SO_RCVTIMEO`.
fn control_peer_answers(stream: &UnixStream) -> bool {
    if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return false;
    }
    let ping = ControlRequest::Ping {
        version: PROTOCOL_VERSION,
        request_id: 0,
    };
    let mut writer = stream;
    if serde_json::to_writer(&mut writer, &ping).is_err()
        || writer.write_all(b"\n").is_err()
        || writer.flush().is_err()
    {
        return false;
    }
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        // Any well-formed response line proves a control peer is serving.
        Ok(n) if n > 0 => serde_json::from_str::<ControlResponse>(&line).is_ok(),
        // EOF (0) or timeout/error: no answering peer.
        _ => false,
    }
}

fn validate_socket_instance(instance: &str) -> io::Result<()> {
    if instance.is_empty()
        || instance.len() > 48
        || !instance
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket instance must be 1..=48 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn xdg_runtime_dir_for_uid(uid: u32) -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    if !runtime.is_absolute() {
        return None;
    }
    let metadata = fs::metadata(&runtime).ok()?;
    if metadata.is_dir() && metadata.uid() == uid && metadata.permissions().mode() & 0o077 == 0 {
        Some(runtime)
    } else {
        None
    }
}

fn socket_filename(instance: &str) -> String {
    if instance == "default" {
        "pmux.sock".into()
    } else {
        format!("pmux-{instance}.sock")
    }
}

/// Resolve the Prismattyc socket path under `$XDG_RUNTIME_DIR/prismattyc/`,
/// with a UID-qualified `/tmp/prismattyc-<uid>/` fallback.
/// `instance` cannot contain path separators.
#[cfg(unix)]
pub fn default_socket_path(instance: &str) -> io::Result<PathBuf> {
    validate_socket_instance(instance)?;
    let uid = rustix::process::geteuid().as_raw();
    if let Some(runtime) = xdg_runtime_dir_for_uid(uid) {
        return Ok(runtime.join("prismattyc").join(socket_filename(instance)));
    }
    Ok(PathBuf::from(format!("/tmp/prismattyc-{uid}")).join(socket_filename(instance)))
}

/// Systemd user runtime dir for `uid`. Desktop mux sockets usually live here.
pub fn systemd_user_runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// Live mux sockets under `/run/user/<uid>` that this CLI did not use.
/// SSH often has a different `XDG_RUNTIME_DIR` (or none) and then falls
/// back to `/tmp`. Callers must not switch sockets automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDirMiss {
    pub resolved_socket: PathBuf,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub user_runtime: PathBuf,
    pub live_sockets: Vec<PathBuf>,
}

impl fmt::Display for RuntimeDirMiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.xdg_runtime_dir {
            None => writeln!(
                f,
                "hint: XDG_RUNTIME_DIR is unset; this CLI used {}.",
                self.resolved_socket.display()
            )?,
            Some(dir) => writeln!(
                f,
                "hint: XDG_RUNTIME_DIR is {}; this CLI used {}.",
                dir.display(),
                self.resolved_socket.display()
            )?,
        }
        let sockets = self
            .live_sockets
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(f, "A live mux socket is at {sockets}.")?;
        write!(
            f,
            "Point PMUX_SOCKET at that path, or export XDG_RUNTIME_DIR={}.",
            self.user_runtime.display()
        )
    }
}

/// If `XDG_RUNTIME_DIR` is not `user_runtime` and `live_sockets` lists
/// connectable pmux sockets there (other than `resolved_socket`), return a
/// miss. Does not change which socket the CLI uses.
pub fn diagnose_runtime_dir_miss(
    resolved_socket: &Path,
    xdg_runtime_dir: Option<&Path>,
    user_runtime: &Path,
    live_sockets: &[PathBuf],
) -> Option<RuntimeDirMiss> {
    let xdg_misses = match xdg_runtime_dir {
        None => true,
        Some(dir) => dir != user_runtime,
    };
    if !xdg_misses {
        return None;
    }
    let mut live_sockets: Vec<PathBuf> = live_sockets
        .iter()
        .filter(|path| path.as_path() != resolved_socket)
        .cloned()
        .collect();
    if live_sockets.is_empty() {
        return None;
    }
    live_sockets.sort();
    Some(RuntimeDirMiss {
        resolved_socket: resolved_socket.to_path_buf(),
        xdg_runtime_dir: xdg_runtime_dir.map(Path::to_path_buf),
        user_runtime: user_runtime.to_path_buf(),
        live_sockets,
    })
}

fn is_pmux_socket_name(name: &str) -> bool {
    name.ends_with(".sock") && (name == "pmux.sock" || name.starts_with("pmux-"))
}

fn collect_live_pmux_sockets(dir: &Path, live: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_pmux_socket_name(name) {
            continue;
        }
        if probe_socket_liveness(&path) == SocketLiveness::Live {
            live.push(path);
        }
    }
}

/// Live pmux sockets under `dir` and `dir/prismattyc/` that answer the
/// control handshake.
pub fn live_pmux_sockets_in(dir: &Path) -> Vec<PathBuf> {
    let mut live = Vec::new();
    collect_live_pmux_sockets(dir, &mut live);
    collect_live_pmux_sockets(&dir.join("prismattyc"), &mut live);
    live.sort();
    live
}

/// Diagnose a live `/run/user/<uid>` mux that this process is not using.
#[cfg(unix)]
pub fn diagnose_runtime_dir_miss_from_env(resolved_socket: &Path) -> Option<RuntimeDirMiss> {
    let uid = rustix::process::geteuid().as_raw();
    let user_runtime = systemd_user_runtime_dir(uid);
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    diagnose_runtime_dir_miss(
        resolved_socket,
        xdg.as_deref(),
        &user_runtime,
        &live_pmux_sockets_in(&user_runtime),
    )
}

/// Process-exit signal: the connection handler raises this after flushing
/// `ShutdownAccepted`. The control plane never exits the process itself.
struct ShutdownGate {
    flag: Mutex<bool>,
    cvar: Condvar,
}

impl ShutdownGate {
    fn request(&self) {
        let mut requested = self
            .flag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *requested = true;
        self.cvar.notify_all();
    }

    fn wait(&self) {
        let mut requested = self
            .flag
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*requested {
            requested = self
                .cvar
                .wait(requested)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Threaded local socket server. Each connection is isolated; the shared
/// control-plane mutex is released before any potentially blocking write.
pub struct ControlServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    shutdown: Arc<ShutdownGate>,
    accept_thread: Option<thread::JoinHandle<()>>,
    maintenance_thread: Option<thread::JoinHandle<()>>,
    attention_thread: Option<thread::JoinHandle<()>>,
    plane: Arc<Mutex<ControlPlane>>,
}

impl ControlServer {
    pub fn bind(path: impl AsRef<Path>, plane: ControlPlane) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let has_live_runtime = plane.is_live();
        #[cfg(windows)]
        crate::platform::require_private_directory(
            path.parent().unwrap_or_else(|| Path::new(".")),
        )?;
        prepare_socket_path(&path)?;
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = crate::platform::set_mode(&path, 0o600) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(ShutdownGate {
            flag: Mutex::new(false),
            cvar: Condvar::new(),
        });
        let plane = Arc::new(Mutex::new(plane));
        let attention_thread =
            crate::attention::spawn_reconciler(Arc::clone(&plane), Arc::clone(&stop));
        let thread_stop = Arc::clone(&stop);
        let thread_plane = Arc::clone(&plane);
        let thread_shutdown = Arc::clone(&shutdown);
        let clients = Arc::new(AtomicUsize::new(0));
        let accept_thread = match thread::Builder::new()
            .name("prism-control-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    thread_plane,
                    thread_stop,
                    thread_shutdown,
                    clients,
                )
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let maintenance_thread = if has_live_runtime {
            let maintenance_stop = Arc::clone(&stop);
            let maintenance_plane = Arc::clone(&plane);
            match thread::Builder::new()
                .name("prism-server-runtime".into())
                .spawn(move || {
                    while !maintenance_stop.load(Ordering::Acquire) {
                        let watch = {
                            let mut guard = maintenance_plane
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            guard.drain_live();
                            guard.maybe_persist_pane_logs();
                            guard.retry_mail_nudges();
                            guard.pane_log_watch()
                        };
                        watch.ring_pending();
                        thread::sleep(Duration::from_millis(4));
                    }
                }) {
                Ok(thread) => Some(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    let _ = accept_thread.join();
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
            }
        } else {
            None
        };
        Ok(Self {
            path,
            stop,
            shutdown,
            accept_thread: Some(accept_thread),
            maintenance_thread,
            attention_thread,
            plane,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn plane(&self) -> Arc<Mutex<ControlPlane>> {
        Arc::clone(&self.plane)
    }

    /// Block until a flushed `ShutdownAccepted` has requested process exit.
    pub fn wait_shutdown(&self) {
        self.shutdown.wait();
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let watch = self
            .plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pane_log_watch();
        watch.ring();
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.maintenance_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.attention_thread.take() {
            let _ = thread.join();
        }
        self.plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush_pane_logs();
        remove_owned_socket(&self.path);
    }
}

fn prepare_socket_path(path: &Path) -> io::Result<()> {
    match probe_socket_liveness(path) {
        SocketLiveness::Missing => Ok(()),
        SocketLiveness::Live => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "control socket is already accepting connections",
        )),
        SocketLiveness::Foreign => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to replace a non-socket or foreign-owned path",
        )),
        SocketLiveness::Stale => fs::remove_file(path),
    }
}

fn remove_owned_socket(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if owned_socket(path, &metadata) {
            let _ = fs::remove_file(path);
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    plane: Arc<Mutex<ControlPlane>>,
    stop: Arc<AtomicBool>,
    shutdown: Arc<ShutdownGate>,
    clients: Arc<AtomicUsize>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                    clients.fetch_sub(1, Ordering::AcqRel);
                    drop(stream);
                    continue;
                }
                let client_plane = Arc::clone(&plane);
                let client_stop = Arc::clone(&stop);
                let client_shutdown = Arc::clone(&shutdown);
                let client_count = Arc::clone(&clients);
                if thread::Builder::new()
                    .name("prism-control-client".into())
                    .spawn(move || {
                        let _guard = ClientCountGuard(client_count);
                        let _ = handle_client(stream, client_plane, client_stop, client_shutdown);
                    })
                    .is_err()
                {
                    clients.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

struct ClientCountGuard(Arc<AtomicUsize>);

impl Drop for ClientCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ClientConnState {
    last_request_id: u64,
    snapshot_established: bool,
    /// Last client registered on this connection (disconnect clears leases).
    connection_client: Option<u64>,
    frame: Vec<u8>,
}

/// Configure a freshly accepted client stream for bounded blocking I/O.
///
/// The accept loop runs the listener non-blocking (so it can poll `stop`), and
/// on macOS/BSD an accepted socket INHERITS the listener's `O_NONBLOCK`. A
/// non-blocking socket ignores `SO_RCVTIMEO`, so `set_read_timeout` alone does
/// not bound a read — `recvfrom` returns `EAGAIN` instantly. The quiet-client
/// `continue` in `handle_client_loop` then busy-spins at 100% CPU per attached
/// client. Clearing non-blocking mode restores blocking reads that
/// honor the timeout: an idle client costs ~0 CPU and shutdown is still
/// observed once per `IO_TIMEOUT`.
fn configure_accepted_stream(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

fn handle_client(
    stream: UnixStream,
    plane: Arc<Mutex<ControlPlane>>,
    stop: Arc<AtomicBool>,
    shutdown: Arc<ShutdownGate>,
) -> io::Result<()> {
    verify_same_user(&stream)?;
    configure_accepted_stream(&stream)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut state = ClientConnState {
        last_request_id: 0,
        snapshot_established: false,
        connection_client: None,
        frame: Vec::new(),
    };
    let result = handle_client_loop(
        &mut reader,
        &mut writer,
        &plane,
        &stop,
        &shutdown,
        &mut state,
    );
    if let Some(client_raw) = state.connection_client {
        if let Ok(mut guard) = plane.lock() {
            let _ = guard.handle(ControlRequest::DisconnectClient {
                version: PROTOCOL_VERSION,
                request_id: state.last_request_id.saturating_add(1).max(1),
                client_id: client_raw,
            });
        }
    }
    result
}

/// Upper bound on a `MailWait` requested timeout. A watcher that
/// wants longer re-arms; the cap keeps a forgotten watcher from pinning
/// a connection thread forever.
const MAX_MAIL_WAIT: Duration = Duration::from_secs(3600);

/// Serve `MailWait` on the client thread. The wait parks on the
/// shared [`crate::mailbox::MailboxWatch`] while NOT holding the plane
/// lock; the predicate re-locks the plane briefly per check. Replies
/// `MailDepth` either way: `open > 0` means mail arrived.
fn handle_mail_wait(
    plane: &Arc<Mutex<ControlPlane>>,
    request: &ControlRequest,
    request_id: u64,
) -> ControlResponse {
    let ControlRequest::MailWait {
        client_id,
        timeout_ms,
        ..
    } = request
    else {
        return ControlResponse::error(
            request_id,
            ControlError::new(
                ControlErrorCode::Internal,
                "handle_mail_wait: wrong variant",
            ),
        );
    };
    let (agent, watch) = {
        let guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.mail_wait_prepare(*client_id) {
            Ok(prepared) => prepared,
            Err(error) => return ControlResponse::error(request_id, error),
        }
    };
    let timeout = Duration::from_millis(u64::from(*timeout_ms)).min(MAX_MAIL_WAIT);
    let waited = watch.wait_until(timeout, || -> Result<Option<()>, ControlError> {
        let guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (open, _) = guard.mail_depth_for(&agent)?;
        Ok((open > 0).then_some(()))
    });
    let result = match waited {
        Ok(arrived) => arrived.is_some(),
        Err(error) => return ControlResponse::error(request_id, error),
    };
    let _ = result;
    let guard = plane
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.mail_depth_for(&agent) {
        Ok((open, held)) => {
            ControlResponse::ok(request_id, ControlResponseData::MailDepth { open, held })
        }
        Err(error) => ControlResponse::error(request_id, error),
    }
}

const MAX_PANE_LOG_BATCH: usize = 256;

/// Serve `SubscribePane` on the client thread. Writes one or more NDJSON
/// frames for the same request_id, then returns. Idle `timeout_ms` ends
/// the stream (`done: true`). Zero timeout is catch-up only.
#[cfg(unix)]
fn control_peer_closed(stream: &UnixStream) -> bool {
    let mut fds = [rustix::event::PollFd::new(
        stream,
        rustix::event::PollFlags::IN
            | rustix::event::PollFlags::OUT
            | rustix::event::PollFlags::HUP,
    )];
    let zero = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if let Ok(n) = rustix::event::poll(&mut fds, Some(&zero)) {
        if n > 0 {
            let ready = fds[0].revents();
            if ready.intersects(
                rustix::event::PollFlags::HUP
                    | rustix::event::PollFlags::ERR
                    | rustix::event::PollFlags::NVAL,
            ) {
                return true;
            }
        }
    }
    let mut buf = [0u8; 1];
    match rustix::net::recv(
        stream,
        &mut buf,
        rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
    ) {
        Ok((0, _)) => true,
        Err(rustix::io::Errno::AGAIN) => false,
        Err(
            rustix::io::Errno::CONNRESET | rustix::io::Errno::PIPE | rustix::io::Errno::NOTCONN,
        ) => true,
        Ok(_) => false,
        Err(_) => true,
    }
}

fn handle_subscribe_pane(
    plane: &Arc<Mutex<ControlPlane>>,
    request: &ControlRequest,
    request_id: u64,
    stop: &Arc<AtomicBool>,
    writer: &mut UnixStream,
) -> io::Result<()> {
    let ControlRequest::SubscribePane {
        client_id,
        pane_id,
        from_seq,
        timeout_ms,
        ..
    } = request
    else {
        return write_response(
            writer,
            &ControlResponse::error(
                request_id,
                ControlError::new(
                    ControlErrorCode::Internal,
                    "handle_subscribe_pane: wrong variant",
                ),
            ),
            stop,
        );
    };
    let timeout = Duration::from_millis(u64::from(*timeout_ms)).min(MAX_MAIL_WAIT);
    let (watch, mut through_seq, first) = {
        let guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.subscribe_frame(*client_id, *pane_id, *from_seq, timeout.is_zero(), false) {
            Ok((watch, through, frame)) => (watch, through, frame),
            Err(error) => {
                return write_response(writer, &ControlResponse::error(request_id, error), stop);
            }
        }
    };
    let skip_empty = timeout > Duration::ZERO
        && matches!(
            &first,
            ControlResponseData::PaneSubscribe {
                events,
                gap: false,
                ..
            } if events.is_empty()
        );
    if !skip_empty {
        write_response(writer, &ControlResponse::ok(request_id, first), stop)?;
    }
    if timeout.is_zero() {
        return Ok(());
    }
    loop {
        if stop.load(Ordering::Acquire) || control_peer_closed(writer) {
            return Ok(());
        }
        let pane_id = *pane_id;
        let client_id = *client_id;
        let deadline = Instant::now() + timeout;
        let woke = loop {
            if stop.load(Ordering::Acquire) || control_peer_closed(writer) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Ok(None);
            }
            let slice = remaining.min(Duration::from_millis(50));
            match watch.wait_until(slice, || -> Result<Option<()>, ControlError> {
                let guard = plane
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.subscribe_has_work(client_id, pane_id, through_seq)
            }) {
                Ok(Some(())) => break Ok(Some(())),
                Ok(None) => {}
                Err(error) => break Err(error),
            }
        };
        match woke {
            Ok(Some(())) => {}
            Ok(None) => {
                let done = ControlResponse::ok(
                    request_id,
                    ControlResponseData::PaneSubscribe {
                        pane_id,
                        gap: false,
                        snapshot: None,
                        events: Vec::new(),
                        through_seq,
                        done: true,
                    },
                );
                return write_response(writer, &done, stop);
            }
            Err(error) => {
                return write_response(writer, &ControlResponse::error(request_id, error), stop);
            }
        }
        let next = {
            let guard = plane
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.subscribe_frame(client_id, pane_id, through_seq, false, true)
        };
        match next {
            Ok((_, through, frame)) => {
                through_seq = through;
                let done = matches!(
                    &frame,
                    ControlResponseData::PaneSubscribe { done: true, .. }
                );
                write_response(writer, &ControlResponse::ok(request_id, frame), stop)?;
                if done {
                    return Ok(());
                }
            }
            Err(error) => {
                return write_response(writer, &ControlResponse::error(request_id, error), stop);
            }
        }
    }
}

fn handle_client_loop(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    plane: &Arc<Mutex<ControlPlane>>,
    stop: &Arc<AtomicBool>,
    shutdown: &Arc<ShutdownGate>,
    state: &mut ClientConnState,
) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        match read_frame(reader, &mut state.frame) {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let response = ControlResponse::error(
                    0,
                    ControlError::new(ControlErrorCode::FrameTooLarge, error.to_string()),
                );
                let _ = write_response(writer, &response, stop);
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                // A quiet attached GUI is still attached. The timeout bounds a
                // single read and lets server shutdown be observed; it is not
                // an inactivity lease and must not clear controller identity.
                // EINTR (a signal landed on this thread mid-read) is not a
                // client error either: retry, do not drop the client (PT-134).
                continue;
            }
            Err(error) => return Err(error),
        }
        let request = match serde_json::from_slice::<ControlRequest>(&state.frame) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    writer,
                    &ControlResponse::error(
                        0,
                        ControlError::new(ControlErrorCode::InvalidJson, error.to_string()),
                    ),
                    stop,
                )?;
                continue;
            }
        };
        let (version, request_id) = request.header();
        let is_snapshot = matches!(request, ControlRequest::Snapshot { .. });
        let is_events = matches!(request, ControlRequest::Events { .. });
        let is_register = matches!(request, ControlRequest::RegisterClient { .. });
        let is_mail_wait = matches!(request, ControlRequest::MailWait { .. });
        let is_subscribe = matches!(request, ControlRequest::SubscribePane { .. });
        let claimed_client = request.claimed_client_id();
        let response = if request_id != 0 && request_id <= state.last_request_id {
            ControlResponse::error(
                request_id,
                ControlError::new(
                    ControlErrorCode::StaleRequestId,
                    format!(
                        "request_id {request_id} is not newer than {} on this connection",
                        state.last_request_id
                    ),
                ),
            )
        } else if request_id != 0
            && version == PROTOCOL_VERSION
            && is_register
            && state.connection_client.is_some()
        {
            state.last_request_id = request_id;
            ControlResponse::error(
                request_id,
                ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    "this connection already has a registered client identity",
                ),
            )
        } else if request_id != 0
            && version == PROTOCOL_VERSION
            && claimed_client.is_some()
            && claimed_client != state.connection_client
        {
            state.last_request_id = request_id;
            ControlResponse::error(
                request_id,
                stale_id(
                    "client for this connection",
                    claimed_client.unwrap_or_default(),
                ),
            )
        } else if request_id != 0
            && version == PROTOCOL_VERSION
            && is_events
            && !state.snapshot_established
        {
            if request_id != 0 {
                state.last_request_id = request_id;
            }
            let guard = plane
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ControlResponse::error(request_id, guard.snapshot_required_error())
        } else if is_mail_wait {
            if request_id != 0 {
                state.last_request_id = request_id;
            }
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // Park on the mail watch WITHOUT holding the plane lock; the
            // predicate re-locks briefly per check.
            handle_mail_wait(plane, &request, request_id)
        } else if is_subscribe {
            if request_id == 0 {
                ControlResponse::error(
                    request_id,
                    ControlError::new(
                        ControlErrorCode::InvalidRequest,
                        "request_id must be non-zero",
                    ),
                )
            } else if version != PROTOCOL_VERSION {
                state.last_request_id = request_id;
                ControlResponse::error(
                    request_id,
                    ControlError::new(
                        ControlErrorCode::IncompatibleVersion,
                        format!("protocol {version} is incompatible with {PROTOCOL_VERSION}"),
                    ),
                )
            } else {
                state.last_request_id = request_id;
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                handle_subscribe_pane(plane, &request, request_id, stop, writer)?;
                continue;
            }
        } else {
            if request_id != 0 {
                state.last_request_id = request_id;
            }
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let response = {
                let mut guard = plane
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.handle(request)
            };
            // Wake watchers only after the plane lock drops: a waiter
            // holds the watch lock while its predicate takes the plane
            // lock, so ringing under the plane lock would deadlock.
            if matches!(
                &response.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::MailSent { .. }
                        | ControlResponseData::MailBroadcasted { .. }
                        | ControlResponseData::MailReleased { .. }
                }
            ) {
                let watch = plane
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .mail_watch();
                watch.ring();
            }
            response
        };
        {
            let watch = plane
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pane_log_watch();
            watch.ring_pending();
        }
        if is_snapshot
            && matches!(
                &response.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::Snapshot { .. }
                }
            )
        {
            state.snapshot_established = true;
        }
        if is_register {
            if let ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } = &response.body
            {
                state.connection_client = Some(*client_id);
            }
        }
        if matches!(
            &response.body,
            ControlResponseBody::Error {
                error: ControlError {
                    resnapshot_required: true,
                    ..
                }
            }
        ) {
            state.snapshot_established = false;
        }
        write_response(writer, &response, stop)?;
        if matches!(
            &response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::ShutdownAccepted
            }
        ) {
            shutdown.request();
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_same_user(stream: &UnixStream) -> io::Result<()> {
    let credentials = rustix::net::sockopt::socket_peercred(stream)?;
    if credentials.uid != rustix::process::geteuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control connection peer uid does not match server uid",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_same_user(_stream: &UnixStream) -> io::Result<()> {
    // Access is gated by the socket and runtime directory permissions.
    // Windows requires an inheritable user-only parent DACL before binding.
    Ok(())
}

fn read_frame(reader: &mut BufReader<UnixStream>, frame: &mut Vec<u8>) -> io::Result<bool> {
    frame.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            // Read timeout mid-frame: the request is still arriving in
            // fragments (macOS delivers small unbuffered writes piecewise, so
            // the bounded read can fire between fragments). Keep waiting rather
            // than returning — the caller's `continue` would call us again and
            // `frame.clear()` would discard the bytes already consumed here,
            // corrupting the framing. Only surface the timeout when nothing is
            // buffered yet, so server shutdown can still be observed between
            // frames. EINTR mid-frame is the same hole (PT-134): the caller
            // retries on `Interrupted` too, so it must not see it while bytes
            // sit in `frame`.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) && !frame.is_empty() =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(!frame.is_empty());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(take) > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("control request exceeds {MAX_REQUEST_BYTES} bytes"),
            ));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            while matches!(frame.last(), Some(b'\n' | b'\r')) {
                frame.pop();
            }
            return Ok(true);
        }
    }
}

fn write_response(
    writer: &mut UnixStream,
    response: &ControlResponse,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bytes = serde_json::to_vec(&ControlResponse::error(
            response.request_id,
            ControlError::new(
                ControlErrorCode::Internal,
                "control response exceeds bounded wire size",
            ),
        ))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    bytes.push(b'\n');
    write_all_timeout_aware(writer, &bytes, stop)
}

/// Give up on a response whose client makes no progress for this long.
///
/// The bounded write timeout drops a stalled write after `IO_TIMEOUT`, but a
/// client that is merely *slow* (draining a large frame between paints) keeps
/// making progress and must not be dropped mid-frame. We only evict a client
/// that buffers zero bytes for a continuous window this wide — long enough that
/// a live client always advances within it, short enough that a dead or wedged
/// client cannot pin a server thread indefinitely.
const RESPONSE_STALL_LIMIT: Duration = Duration::from_secs(3);

/// Write a full response frame across transient write timeouts.
///
/// The client socket has a bounded write timeout (`IO_TIMEOUT`) so server
/// shutdown stays observable. A response larger than the socket send buffer
/// (8 KiB on macOS) needs several `write` calls; if the client drains slowly,
/// an intermediate call returns `WouldBlock`/`TimedOut` after a *partial*
/// write. A bare `write_all` surfaces that as an error and the caller drops
/// the connection mid-frame — the client then reads a truncated line, fails to
/// parse it, and its next request hits a closed socket (EPIPE). Instead we keep
/// writing the remaining bytes while the client keeps accepting them, so a
/// live-but-slow client always receives the whole frame.
///
/// A client that stops draining entirely is still evicted: if no byte is
/// buffered for `RESPONSE_STALL_LIMIT`, or the server begins shutting down, the
/// write returns an error and the caller drops the connection — preserving the
/// backpressure guarantee that one wedged client cannot pin its handler thread.
fn write_all_timeout_aware(
    writer: &mut UnixStream,
    bytes: &[u8],
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut written = 0;
    let mut last_progress = Instant::now();
    while written < bytes.len() {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "server shutting down before the response was fully written",
            ));
        }
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write the whole response",
                ));
            }
            Ok(count) => {
                written += count;
                last_progress = Instant::now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                // A slow client did not drain within the write timeout. The
                // frame is still valid and partially buffered; keep going until
                // the client stalls for good.
                if last_progress.elapsed() >= RESPONSE_STALL_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "client stopped draining the response",
                    ));
                }
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::test_time_budget;
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn classify_control_request_id_splits_stale_awaited_ahead() {
        assert_eq!(
            classify_control_request_id(225525, 225527),
            ControlIdMatch::Stale
        );
        assert_eq!(
            classify_control_request_id(225527, 225527),
            ControlIdMatch::Awaited
        );
        assert_eq!(
            classify_control_request_id(225528, 225527),
            ControlIdMatch::Ahead
        );
    }

    #[test]
    fn next_stale_skip_increments_until_the_bound() {
        assert_eq!(next_stale_skip(0), Some(1));
        assert_eq!(next_stale_skip(31), Some(32));
        assert_eq!(next_stale_skip(32), None);
        assert_eq!(next_stale_skip(u32::MAX), None);
    }

    #[test]
    fn resize_without_cell_pixels_still_deserializes() {
        let raw =
            r#"{"type":"resize","version":1,"request_id":1,"window_id":1,"cols":80,"rows":24}"#;
        let req: ControlRequest = serde_json::from_str(raw).expect("old resize JSON");
        match req {
            ControlRequest::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                host,
                ..
            } => {
                assert_eq!((cols, rows), (80, 24));
                assert_eq!((cell_width_px, cell_height_px), (None, None));
                assert!(!host);
            }
            other => panic!("expected resize, got {other:?}"),
        }
    }

    #[test]
    fn pane_content_omits_cursor_shape_on_old_snapshots() {
        let raw = r#"{
            "pane_id":1,"revision":1,"cols":8,"rows":1,
            "cursor_row":0,"cursor_col":0,"cursor_visible":true,
            "alt_active":false,"child_alive":true,"lines":["abcd"]
        }"#;
        let content: PaneContent = serde_json::from_str(raw).expect("legacy PaneContent");
        assert_eq!(content.cursor_shape, None);
        let encoded = serde_json::to_string(&content).unwrap();
        assert!(
            !encoded.contains("cursor_shape"),
            "None must stay omitted: {encoded}"
        );
        let with_bar = PaneContent {
            cursor_shape: Some(CursorShapeWire::Bar),
            ..content
        };
        let encoded = serde_json::to_string(&with_bar).unwrap();
        assert!(encoded.contains("\"bar\""), "{encoded}");
    }

    fn fixture(capacity: usize) -> (ControlPlane, u64, u64) {
        let domain = Domain::bootstrap("test").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let plane = ControlPlane::new(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(capacity),
        )
        .unwrap();
        (plane, window.get(), pane.get())
    }

    fn socket_path() -> PathBuf {
        let serial = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).expect("test socket nonce");
        let nonce = u64::from_ne_bytes(nonce);
        PathBuf::from(format!(
            "/tmp/prism-control-test-{}-{serial}-{nonce:016x}.sock",
            std::process::id()
        ))
    }

    /// the accept loop runs the listener non-blocking, and macOS
    /// accepted sockets inherit `O_NONBLOCK`, which overrides `SO_RCVTIMEO`.
    /// Without clearing it, an idle client's bounded read returns instantly and
    /// `handle_client_loop` busy-spins at 100% CPU per attached client.
    /// `configure_accepted_stream` must restore blocking reads so an idle read
    /// blocks for ~`IO_TIMEOUT` instead of returning at once.
    #[test]
    fn accepted_stream_blocks_for_read_timeout_not_spin() {
        let path = socket_path();
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        // Mirror the server: the accept loop polls a non-blocking listener.
        listener.set_nonblocking(true).unwrap();

        // Client connects and stays idle (sends nothing).
        let client = UnixStream::connect(&path).unwrap();

        // Accept (poll until the pending connection lands).
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };

        configure_accepted_stream(&accepted).unwrap();

        // A blocking read on the idle socket must wait ~IO_TIMEOUT then time
        // out. With the inherited O_NONBLOCK bug it returns WouldBlock at once
        // (the 100% CPU spin), so the elapsed time collapses toward zero.
        let mut probe = accepted;
        let mut buf = [0_u8; 64];
        let started = Instant::now();
        // A signal (SIGCHLD from a sibling test, a profiler tick) can land on
        // this thread mid-read and surface as EINTR; that is neither the spin
        // nor the timeout under test, so read again (PT-134). Cap the retries
        // so a signal storm fails the test instead of hanging it.
        let mut interrupted = 0;
        let result = loop {
            match probe.read(&mut buf) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted && interrupted < 64 => {
                    interrupted += 1;
                    continue;
                }
                other => break other,
            }
        };
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "idle read should time out, got {result:?}"
        );
        assert!(
            elapsed >= IO_TIMEOUT / 2,
            "idle read returned in {elapsed:?}; expected a blocking wait near \
             {IO_TIMEOUT:?} (O_NONBLOCK not cleared -> 100% CPU spin)"
        );

        drop(client);
        let _ = fs::remove_file(&path);
    }

    fn request(stream: &mut UnixStream, request: &ControlRequest) -> ControlResponse {
        serde_json::to_writer(&mut *stream, request).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn error_code(response: &ControlResponse) -> ControlErrorCode {
        match &response.body {
            ControlResponseBody::Error { error } => error.code,
            ControlResponseBody::Ok { .. } => panic!("expected error response"),
        }
    }

    fn mutation_sequence(response: &ControlResponse) -> u64 {
        match &response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Mutation { ack },
            } => ack.sequence,
            other => panic!("expected mutation response, got {other:?}"),
        }
    }

    fn registered_client(response: ControlResponse) -> u64 {
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("expected registered client, got {other:?}"),
        }
    }

    /// Register a client and bind an agent identity (test helper).
    fn mail_hello(plane: &mut ControlPlane, request_id: u64, agent: &str) -> u64 {
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        }));
        let seated = plane.handle(ControlRequest::MailHello {
            version: PROTOCOL_VERSION,
            request_id: request_id + 1000,
            client_id: client,
            agent: agent.into(),
        });
        assert!(
            matches!(
                seated.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::MailSeated { .. }
                }
            ),
            "expected MailSeated, got {seated:?}"
        );
        client
    }

    fn spawn() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec!["-l".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::from([("TERM".into(), "prism".into())]),
        }
    }

    #[test]
    fn socket_is_private_and_protocol_errors_are_structured() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mode = fs::metadata(server.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let mut stream = UnixStream::connect(server.path()).unwrap();
        let incompatible = request(
            &mut stream,
            &ControlRequest::Ping {
                version: 99,
                request_id: 1,
            },
        );
        assert_eq!(
            error_code(&incompatible),
            ControlErrorCode::IncompatibleVersion
        );
        let stale = request(
            &mut stream,
            &ControlRequest::Ping {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        assert_eq!(error_code(&stale), ControlErrorCode::StaleRequestId);

        let mut zero_stream = UnixStream::connect(server.path()).unwrap();
        let zero = request(
            &mut zero_stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 0,
                after_sequence: 0,
                limit: None,
            },
        );
        assert_eq!(error_code(&zero), ControlErrorCode::InvalidRequest);
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn raw_json_client_can_take_snapshot_without_rust_types() {
        let (plane, _, pane) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        stream
            .write_all(b"{\"type\":\"snapshot\",\"version\":1,\"request_id\":41}\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["request_id"], 41);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["response"]["kind"], "snapshot");
        assert_eq!(
            value["response"]["snapshot"]["sessions"][0]["windows"][0]["panes"][0]["id"],
            pane
        );
    }

    #[test]
    fn bind_refuses_live_socket_and_preserves_non_socket_path() {
        let path = socket_path();
        fs::write(&path, b"sentinel").unwrap();
        let (plane, _, _) = fixture(8);
        let error = ControlServer::bind(&path, plane).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&path).unwrap(), b"sentinel");
        fs::remove_file(&path).unwrap();

        let (plane, _, _) = fixture(8);
        let server = ControlServer::bind(&path, plane).unwrap();
        let (second_plane, _, _) = fixture(8);
        let error = ControlServer::bind(&path, second_plane).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(UnixStream::connect(server.path()).is_ok());
        assert_eq!(probe_socket_liveness(&path), SocketLiveness::Live);
    }

    #[test]
    fn phase2a_bind_replaces_stale_same_uid_socket_leftover() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists());
        assert_eq!(probe_socket_liveness(&path), SocketLiveness::Stale);

        let (plane, _, _) = fixture(8);
        let server = ControlServer::bind(&path, plane).unwrap();
        assert_eq!(probe_socket_liveness(server.path()), SocketLiveness::Live);
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let pong = request(
            &mut stream,
            &ControlRequest::Ping {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        assert!(matches!(
            pong.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Pong
            }
        ));
    }

    #[test]
    fn connectable_socket_without_control_peer_is_stale() {
        // A same-uid socket we can connect to but that never answers the control
        // handshake is not Live. macOS leaves a crashed server's socket briefly
        // connectable while its accept queue drains, so a bare connect succeeds
        // against a dead peer. Classifying that Live makes `bind` refuse to
        // reclaim the socket and restart fails with AddrInUse.
        let path = socket_path();
        // A listener with no accept loop: connections land in the backlog and
        // are never answered — the deterministic form of the macOS drain race.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(
            UnixStream::connect(&path).is_ok(),
            "socket must be connectable"
        );
        assert_eq!(probe_socket_liveness(&path), SocketLiveness::Stale);
        drop(listener);
    }

    #[test]
    fn probe_classifies_missing_and_foreign_paths() {
        let missing = socket_path();
        assert_eq!(probe_socket_liveness(&missing), SocketLiveness::Missing);
        let foreign = socket_path();
        fs::write(&foreign, b"not-a-socket").unwrap();
        assert_eq!(probe_socket_liveness(&foreign), SocketLiveness::Foreign);
        fs::remove_file(&foreign).unwrap();
    }

    #[test]
    fn runtime_dir_miss_requires_xdg_disagreement_and_other_live_socket() {
        let user_runtime = PathBuf::from("/run/user/1000");
        let resolved = PathBuf::from("/tmp/prismattyc-1000/pmux.sock");
        let live = vec![PathBuf::from("/run/user/1000/pmux-work.sock")];

        assert!(diagnose_runtime_dir_miss(
            &resolved,
            Some(user_runtime.as_path()),
            &user_runtime,
            &live,
        )
        .is_none());

        assert!(diagnose_runtime_dir_miss(&resolved, None, &user_runtime, &[]).is_none());

        let only_resolved = vec![resolved.clone()];
        assert!(
            diagnose_runtime_dir_miss(&resolved, None, &user_runtime, &only_resolved).is_none()
        );

        let miss = diagnose_runtime_dir_miss(&resolved, None, &user_runtime, &live)
            .expect("unset XDG plus a live user-runtime socket");
        assert_eq!(miss.resolved_socket, resolved);
        assert_eq!(miss.xdg_runtime_dir, None);
        assert_eq!(miss.user_runtime, user_runtime);
        assert_eq!(miss.live_sockets, live);

        let ssh_xdg = PathBuf::from("/run/user/1000/ssh-xyz");
        let miss =
            diagnose_runtime_dir_miss(&resolved, Some(ssh_xdg.as_path()), &user_runtime, &live)
                .expect("SSH XDG plus a live user-runtime socket");
        let text = miss.to_string();
        assert!(text.contains("/tmp/prismattyc-1000/pmux.sock"), "{text}");
        assert!(text.contains("/run/user/1000/pmux-work.sock"), "{text}");
        assert!(text.contains("PMUX_SOCKET"), "{text}");
        assert!(text.contains("XDG_RUNTIME_DIR=/run/user/1000"), "{text}");
        assert!(text.contains("/run/user/1000/ssh-xyz"), "{text}");
    }

    #[test]
    fn live_pmux_sockets_in_skips_non_matching_names_and_missing_dirs() {
        let missing = PathBuf::from(format!(
            "/tmp/pmux-rtdiag-missing-{}-nope",
            std::process::id()
        ));
        assert!(live_pmux_sockets_in(&missing).is_empty());

        let dir = PathBuf::from(format!(
            "/tmp/pmux-rtdiag-{}-{:016x}",
            std::process::id(),
            {
                let mut nonce = [0_u8; 8];
                getrandom::fill(&mut nonce).expect("dir nonce");
                u64::from_ne_bytes(nonce)
            }
        ));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("other.sock"), b"x").unwrap();
        fs::write(dir.join("pmux-work.txt"), b"x").unwrap();
        fs::write(dir.join("pmux-dead.sock"), b"not-a-socket").unwrap();
        assert!(
            live_pmux_sockets_in(&dir).is_empty(),
            "regular files and non-pmux names must not count as live"
        );

        let (plane, _, _) = fixture(8);
        let live_path = dir.join("pmux-work.sock");
        let server = ControlServer::bind(&live_path, plane).unwrap();
        assert_eq!(live_pmux_sockets_in(&dir), vec![live_path.clone()]);
        drop(server);
        let _ = fs::remove_dir_all(&dir);
    }

    fn session_ids(response: &ControlResponse) -> (u64, u64, u64) {
        match &response.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::Session {
                        session_id,
                        window_id,
                        pane_id,
                        ..
                    },
            } => (*session_id, *window_id, *pane_id),
            other => panic!("expected session response, got {other:?}"),
        }
    }

    #[test]
    fn phase2a_create_and_switch_session_are_ordered_events() {
        let (mut plane, _window, first_pane) = fixture(8);
        let register = plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        });
        let client = registered_client(register);
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 2,
            name: "work".into(),
            spawn: spawn(),
            cols: Some(80),
            rows: Some(24),
            agent_id: None,
            headless: false,
        });
        let (work_session, work_window, work_pane) = session_ids(&created);
        assert_ne!(work_pane, first_pane);
        let snapshot = plane.snapshot().unwrap();
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.sessions[1].name, "work");
        assert_eq!(snapshot.sessions[1].id, work_session);
        assert_eq!(snapshot.sessions[1].windows[0].id, work_window);

        let switched = plane.handle(ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            session_id: work_session,
        });
        assert_eq!(session_ids(&switched).0, work_session);

        let returned = plane.handle(ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            session_id: snapshot.sessions[0].id,
        });
        assert_eq!(session_ids(&returned).2, first_pane);

        let dup = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 5,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: None,
            headless: false,
        });
        assert_eq!(error_code(&dup), ControlErrorCode::InvalidRequest);
        let empty = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 6,
            name: "   ".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: None,
            headless: false,
        });
        assert_eq!(error_code(&empty), ControlErrorCode::InvalidRequest);
        let stale = plane.handle(ControlRequest::SwitchSession {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: client,
            session_id: 99,
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);

        let events = plane.events_after(0, None).unwrap();
        let kinds: Vec<_> = events
            .events
            .iter()
            .map(|envelope| match &envelope.event {
                Event::SessionCreated { name, .. } => format!("created:{name}"),
                Event::SessionSwitched { session_id, .. } => format!("switched:{session_id}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "created:work".into(),
                format!("switched:{work_session}"),
                format!("switched:{}", snapshot.sessions[0].id),
            ]
        );
    }

    #[test]
    fn destroy_session_removes_topology_and_rejects_stale() {
        let (mut plane, _, first_pane) = fixture(8);
        let bootstrap = plane.snapshot().unwrap().sessions[0].id;
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 1,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: None,
            headless: false,
        });
        let work = session_ids(&created).0;
        let work_pane = session_ids(&created).2;
        assert_eq!(plane.snapshot().unwrap().sessions.len(), 2);
        assert_ne!(work_pane, first_pane);

        let encoded = serde_json::to_string(&ControlRequest::DestroySession {
            version: PROTOCOL_VERSION,
            request_id: 2,
            session_id: work,
        })
        .unwrap();
        assert!(encoded.contains("\"type\":\"destroy_session\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let destroyed = plane.handle(decoded);
        assert!(matches!(
            destroyed.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Mutation { .. }
            }
        ));
        let remaining = plane.snapshot().unwrap();
        assert_eq!(remaining.sessions.len(), 1);
        assert_eq!(remaining.sessions[0].id, bootstrap);
        assert_eq!(remaining.sessions[0].windows[0].panes[0].id, first_pane);

        let stale = plane.handle(ControlRequest::DestroySession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            session_id: work,
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);

        let kinds: Vec<_> = plane
            .events_after(0, None)
            .unwrap()
            .events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::SessionCreated { name, .. } => Some(format!("created:{name}")),
                Event::SessionDestroyed { name, .. } => Some(format!("destroyed:{name}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["created:work".to_string(), "destroyed:work".to_string()]
        );
    }

    #[test]
    fn create_session_agent_id_is_unique_and_snapshotted() {
        let prior = std::env::var_os("PMUX_SESSION_AGENTS");
        let path = std::env::temp_dir().join(format!(
            "pmux-agents-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::env::set_var("PMUX_SESSION_AGENTS", &path);
        let (mut plane, _, _) = fixture(8);
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 1,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        assert!(matches!(created.body, ControlResponseBody::Ok { .. }));
        assert_eq!(
            plane.snapshot().unwrap().sessions[1].agent_id.as_deref(),
            Some("operator-a")
        );
        assert_eq!(
            plane
                .domain
                .session_by_agent("operator-a")
                .map(|id| id.get()),
            Some(plane.snapshot().unwrap().sessions[1].id)
        );
        let dup = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 2,
            name: "other".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        assert_eq!(error_code(&dup), ControlErrorCode::InvalidRequest);
        match prior {
            Some(value) => std::env::set_var("PMUX_SESSION_AGENTS", value),
            None => std::env::remove_var("PMUX_SESSION_AGENTS"),
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn create_session_headless_has_no_windows() {
        let (mut plane, _, _) = fixture(8);
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 1,
            name: "worker-bot".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("worker-bot".into()),
            headless: true,
        });
        let ControlResponseBody::Ok {
            response:
                ControlResponseData::Session {
                    session_id,
                    window_id,
                    pane_id,
                    ..
                },
        } = created.body
        else {
            panic!("expected session {created:?}");
        };
        assert_eq!((window_id, pane_id), (0, 0));
        let session = plane
            .snapshot()
            .unwrap()
            .sessions
            .into_iter()
            .find(|s| s.id == session_id)
            .unwrap();
        assert!(session.windows.is_empty());
        assert_eq!(session.agent_id.as_deref(), Some("worker-bot"));
        assert_eq!(
            plane
                .domain
                .session_by_agent("worker-bot")
                .map(|id| id.get()),
            Some(session_id)
        );
    }

    #[test]
    fn headless_agent_queues_mail_without_doorbell() {
        let (mut plane, _, _) = fixture(8);
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 1,
            name: "worker-bot".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("worker-bot".into()),
            headless: true,
        });
        assert!(matches!(created.body, ControlResponseBody::Ok { .. }));
        let cursor = mail_hello(&mut plane, 2, "operator-b");
        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: cursor,
            to: "worker-bot".into(),
            summary: "queued".into(),
            body: "hello".into(),
        });
        assert!(matches!(
            sent.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailSent { depth: 1, .. }
            }
        ));
        assert!(
            plane.mail_attention.is_empty(),
            "headless session has no pane; doorbell must not arm attention"
        );
        let worker = mail_hello(&mut plane, 4, "worker-bot");
        let claimed = plane.handle(ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: worker,
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::MailLetters { letters },
        } = claimed.body
        else {
            panic!("expected letters {claimed:?}");
        };
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].summary, "queued");
        assert_eq!(letters[0].body, "hello");
        assert_eq!(letters[0].to, "worker-bot");
    }

    #[test]
    fn mail_lifecycle_send_claim_commit() {
        let (mut plane, _, _) = fixture(8);
        let peer = mail_hello(&mut plane, 1, "operator-a");
        let cursor = mail_hello(&mut plane, 2, "operator-b");

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "hello".into(),
            body: "world".into(),
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::MailSent { id, depth },
        } = sent.body
        else {
            panic!("expected MailSent: {sent:?}");
        };
        assert_eq!(depth, 1);

        let inbox = plane.handle(ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: peer,
        });
        assert!(matches!(
            inbox.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 1, held: 0 }
            }
        ));

        let claimed = plane.handle(ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: peer,
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::MailLetters { letters },
        } = claimed.body
        else {
            panic!("expected MailLetters: {claimed:?}");
        };
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].id, id);
        assert_eq!(
            letters[0].from, "operator-b",
            "from is the connection identity"
        );
        assert_eq!(letters[0].to, "operator-a");
        assert_eq!(letters[0].summary, "hello");

        let committed = plane.handle(ControlRequest::MailCommit {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: peer,
            ids: vec![id],
        });
        assert!(matches!(
            committed.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailCommitted { committed: 1 }
            }
        ));
        let inbox = plane.handle(ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: peer,
        });
        assert!(matches!(
            inbox.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 0, held: 0 }
            }
        ));
    }

    #[test]
    fn mail_requires_identity_and_unknown_recipients_queue() {
        let (mut plane, _, _) = fixture(8);
        let anon = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let refused = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: anon,
            to: "operator-a".into(),
            summary: "s".into(),
            body: "b".into(),
        });
        assert_eq!(error_code(&refused), ControlErrorCode::InvalidRequest);

        let cursor = mail_hello(&mut plane, 3, "operator-b");
        // No live session is bound to future-agent: the letter still queues.
        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            to: "future-agent".into(),
            summary: "queued".into(),
            body: String::new(),
        });
        assert!(matches!(
            sent.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailSent { depth: 1, .. }
            }
        ));
        // ...and is claimable once that agent says hello.
        let future = mail_hello(&mut plane, 5, "future-agent");
        let claimed = plane.handle(ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: future,
        });
        assert!(matches!(
            &claimed.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailLetters { letters }
            } if letters.len() == 1 && letters[0].summary == "queued"
        ));
    }

    #[test]
    fn mail_alias_binds_resolves_and_refuses_conflicts() {
        let (mut plane, _, _) = fixture(8);
        let peer = mail_hello(&mut plane, 1, "operator-a");
        let cursor = mail_hello(&mut plane, 2, "operator-b");

        let aliased = plane.handle(ControlRequest::MailAlias {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: peer,
            name: "pm".into(),
        });
        assert!(matches!(
            aliased.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailAliased { .. }
            }
        ));

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            to: "pm".into(),
            summary: "via-alias".into(),
            body: String::new(),
        });
        assert!(matches!(
            sent.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailSent { depth: 1, .. }
            }
        ));

        // Taken alias refused for another agent.
        let taken = plane.handle(ControlRequest::MailAlias {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: cursor,
            name: "pm".into(),
        });
        assert!(matches!(
            taken.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailRefused { .. }
            }
        ));

        // A live session's bound agent id cannot become an alias.
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 6,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("bound-agent".into()),
            headless: false,
        });
        assert!(matches!(created.body, ControlResponseBody::Ok { .. }));
        let refused = plane.handle(ControlRequest::MailAlias {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: cursor,
            name: "bound-agent".into(),
        });
        assert!(matches!(
            refused.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailRefused { .. }
            }
        ));
    }

    #[test]
    fn mail_who_lists_agent_bound_sessions_only() {
        let (mut plane, _, _) = fixture(8);
        let peer = mail_hello(&mut plane, 1, "operator-a");
        plane.handle(ControlRequest::MailAlias {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: peer,
            name: "pm".into(),
        });
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        assert!(matches!(created.body, ControlResponseBody::Ok { .. }));

        let who = plane.handle(ControlRequest::MailWho {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: peer,
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::MailPeers { peers },
        } = who.body
        else {
            panic!("expected MailPeers: {who:?}");
        };
        assert_eq!(
            peers.len(),
            1,
            "the agent-less default session is not listed"
        );
        assert_eq!(peers[0].agent_id, "operator-a");
        assert_eq!(peers[0].session, "work");
        assert!(!peers[0].pane_live, "no live runtime in this fixture");
        assert_eq!(peers[0].aliases, vec!["pm"]);
    }

    #[test]
    fn mail_broadcast_reaches_live_agents_except_caller() {
        let (mut plane, _, _) = fixture(8);
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        let peer = mail_hello(&mut plane, 2, "operator-a");
        for (name, agent) in [("work", "operator-b"), ("other", "operator-a")] {
            let created = plane.handle(ControlRequest::CreateSession {
                version: PROTOCOL_VERSION,
                request_id: 3,
                name: name.into(),
                spawn: spawn(),
                cols: None,
                rows: None,
                agent_id: Some(agent.into()),
                headless: false,
            });
            assert!(matches!(created.body, ControlResponseBody::Ok { .. }));
        }
        let broadcast = plane.handle(ControlRequest::MailBroadcast {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            summary: "standup in 5".into(),
            body: String::new(),
        });
        assert!(matches!(
            &broadcast.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailBroadcasted {
                    delivered: 1,
                    recipients,
                }
            } if recipients == &vec!["operator-a".to_string()]
        ));
        let inbox = plane.handle(ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: peer,
        });
        assert!(matches!(
            inbox.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 1, held: 0 }
            }
        ));
    }

    #[test]
    fn mail_wait_wakes_on_send_and_times_out_clean() {
        use std::sync::mpsc;
        let (plane, _, _) = fixture(8);
        let plane = Arc::new(Mutex::new(plane));
        let peer = mail_hello(&mut plane.lock().unwrap(), 1, "operator-a");
        let cursor = mail_hello(&mut plane.lock().unwrap(), 2, "operator-b");

        // Timeout path: no mail, short wait.
        let response = handle_mail_wait(
            &plane,
            &ControlRequest::MailWait {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: peer,
                timeout_ms: 50,
            },
            3,
        );
        assert!(matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 0, held: 0 }
            }
        ));

        // Arrival path: a sender on "another connection" stores and rings.
        let (done_tx, done_rx) = mpsc::channel();
        let waiter_plane = Arc::clone(&plane);
        let waiter = std::thread::spawn(move || {
            let response = handle_mail_wait(
                &waiter_plane,
                &ControlRequest::MailWait {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: peer,
                    timeout_ms: 30_000,
                },
                4,
            );
            done_tx.send(response).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut guard = plane.lock().unwrap();
            let sent = guard.handle(ControlRequest::MailSend {
                version: PROTOCOL_VERSION,
                request_id: 5,
                client_id: cursor,
                to: "operator-a".into(),
                summary: "wake".into(),
                body: String::new(),
            });
            assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));
        }
        // The client loop rings after the plane lock drops; mirror that.
        // Temporaries in a chained `lock().mail_watch().ring()` live through
        // `ring()`, which inverts waiter order (watch then plane) and deadlocks.
        let watch = plane.lock().unwrap().mail_watch();
        watch.ring();
        let response = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter must wake");
        waiter.join().unwrap();
        assert!(matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 1, .. }
            }
        ));
    }

    #[test]
    fn mail_wait_under_plane_lock_is_a_guard_rail_error() {
        let (mut plane, _, _) = fixture(8);
        let peer = mail_hello(&mut plane, 1, "operator-a");
        let response = plane.handle(ControlRequest::MailWait {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: peer,
            timeout_ms: 10,
        });
        assert_eq!(error_code(&response), ControlErrorCode::Internal);
    }

    #[test]
    fn subscribe_pane_under_plane_lock_is_a_guard_rail_error() {
        let (mut plane, _, pane) = live_fixture("sub");
        let client = register_live_client(&mut plane);
        let response = plane.handle(ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            from_seq: 0,
            timeout_ms: 0,
        });
        assert_eq!(error_code(&response), ControlErrorCode::Internal);
    }

    #[test]
    fn subscribe_pane_catch_up_includes_status_and_marks_done() {
        let (mut plane, _, pane) = live_fixture("sub");
        let client = register_live_client(&mut plane);
        let _ = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 2,
            pane_id: pane,
            text: Some("busy".into()),
        });
        let (_, through, frame) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("catch-up");
        let ControlResponseData::PaneSubscribe {
            gap,
            events,
            done,
            through_seq,
            ..
        } = frame
        else {
            panic!("expected PaneSubscribe: {frame:?}");
        };
        assert!(!gap);
        assert!(done);
        assert_eq!(through_seq, through);
        assert!(events.iter().any(|frame| {
            matches!(
                &frame.event,
                crate::pane_log::PaneEvent::Status { text } if text.as_deref() == Some("busy")
            )
        }));
    }

    fn subscribe_output_bytes(events: &[crate::pane_log::PaneLogFrame]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|frame| match &frame.event {
                crate::pane_log::PaneEvent::Output { bytes } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    #[test]
    fn subscribe_pane_from_seq_receives_exactly_later_output_bytes() {
        let (mut plane, _, pane) = live_fixture("subout");
        let client = register_live_client(&mut plane);
        wait_live_echo(&mut plane, pane, "subout");
        let lease = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 20,
            client_id: client,
            pane_id: pane,
        });
        assert!(matches!(lease.body, ControlResponseBody::Ok { .. }));
        let (_, through, before_frame) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("catch-up before write");
        let before = match &before_frame {
            ControlResponseData::PaneSubscribe { events, .. } => subscribe_output_bytes(events),
            other => panic!("expected PaneSubscribe: {other:?}"),
        };
        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 21,
            client_id: client,
            pane_id: pane,
            data: "pt116-marker\n".into(),
        });
        assert!(matches!(
            wrote.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        wait_live_echo(&mut plane, pane, "ECHO:pt116-marker");
        let (_, _, after_all_frame) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("full log after write");
        let after_all = match &after_all_frame {
            ControlResponseData::PaneSubscribe { events, .. } => subscribe_output_bytes(events),
            other => panic!("expected PaneSubscribe: {other:?}"),
        };
        let (_, _, later_frame) = plane
            .subscribe_frame(client, pane, through, true, false)
            .expect("catch-up from seq N");
        let later = match &later_frame {
            ControlResponseData::PaneSubscribe { events, .. } => subscribe_output_bytes(events),
            other => panic!("expected PaneSubscribe: {other:?}"),
        };
        assert!(
            after_all.starts_with(&before),
            "log must keep earlier bytes: before={} after={}",
            before.len(),
            after_all.len()
        );
        assert_eq!(
            later,
            after_all[before.len()..],
            "subscriber from seq {through} must receive exactly the Output bytes appended after N"
        );
        assert!(
            later
                .windows(b"ECHO:pt116-marker".len())
                .any(|w| w == b"ECHO:pt116-marker"),
            "later bytes must include the post-N write: {later:?}"
        );
        assert!(!later.is_empty(), "post-N Output must not be empty");
    }

    #[test]
    fn subscribe_pane_reconnect_from_seq_receives_output_exactly_once() {
        let (mut plane, _, pane) = live_fixture("subre");
        let client = register_live_client(&mut plane);
        wait_live_echo(&mut plane, pane, "subre");
        let lease = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 20,
            client_id: client,
            pane_id: pane,
        });
        assert!(matches!(lease.body, ControlResponseBody::Ok { .. }));
        let (_, through, _) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("seed");
        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 21,
            client_id: client,
            pane_id: pane,
            data: "pt113-once\n".into(),
        });
        assert!(matches!(wrote.body, ControlResponseBody::Ok { .. }));
        wait_live_echo(&mut plane, pane, "ECHO:pt113-once");
        let (_, _, first) = plane
            .subscribe_frame(client, pane, through, true, false)
            .expect("after disconnect window");
        let once = match &first {
            ControlResponseData::PaneSubscribe { events, .. } => subscribe_output_bytes(events),
            other => panic!("expected PaneSubscribe: {other:?}"),
        };
        let marker = b"ECHO:pt113-once";
        let hits = once.windows(marker.len()).filter(|w| *w == marker).count();
        assert_eq!(
            hits, 1,
            "catch-up after disconnect must deliver the write once: {once:?}"
        );
        let (_, _, again) = plane
            .subscribe_frame(client, pane, through, true, false)
            .expect("second catch-up");
        let again_bytes = match &again {
            ControlResponseData::PaneSubscribe { events, .. } => subscribe_output_bytes(events),
            other => panic!("expected PaneSubscribe: {other:?}"),
        };
        assert_eq!(
            again_bytes, once,
            "replay from the same seq must be identical"
        );
    }

    #[test]
    fn subscribe_pane_waits_for_status_then_idles_done() {
        use std::io::BufRead;
        let (mut plane, _, pane) = live_fixture("subw");
        let client = register_live_client(&mut plane);
        let (_, through, _) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("seed catch-up");
        let plane = Arc::new(Mutex::new(plane));
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writer
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let req = ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id: 9,
            client_id: client,
            pane_id: pane,
            from_seq: through,
            timeout_ms: 2_000,
        };
        let waiter_plane = Arc::clone(&plane);
        let waiter_stop = Arc::clone(&stop);
        let waiter = std::thread::spawn(move || {
            handle_subscribe_pane(&waiter_plane, &req, 9, &waiter_stop, &mut writer)
        });
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut guard = plane.lock().unwrap();
            let set = guard.handle(ControlRequest::SetPaneStatus {
                version: PROTOCOL_VERSION,
                request_id: 10,
                pane_id: pane,
                text: Some("go".into()),
            });
            assert!(matches!(set.body, ControlResponseBody::Ok { .. }));
        }
        // Drop the plane guard before ringing: waiters hold the watch lock
        // then take the plane (PaneLogWatch). A chained
        // `lock().pane_log_watch().ring_pending()` keeps the guard live.
        let watch = plane.lock().unwrap().pane_log_watch();
        watch.ring_pending();
        let mut buf = String::new();
        let mut r = BufReader::new(reader);
        r.read_line(&mut buf).expect("first frame");
        let first: ControlResponse = serde_json::from_str(buf.trim()).unwrap();
        let ControlResponseBody::Ok {
            response: ControlResponseData::PaneSubscribe { events, done, .. },
        } = first.body
        else {
            panic!("first frame: {first:?}");
        };
        assert!(!done);
        assert!(events.iter().any(|frame| {
            matches!(
                &frame.event,
                crate::pane_log::PaneEvent::Status { text } if text.as_deref() == Some("go")
            )
        }));
        buf.clear();
        r.read_line(&mut buf).expect("done frame");
        let last: ControlResponse = serde_json::from_str(buf.trim()).unwrap();
        assert!(matches!(
            last.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::PaneSubscribe { done: true, .. }
            }
        ));
        waiter.join().unwrap().unwrap();
    }

    #[test]
    fn subscribe_pane_stop_cancels_wait() {
        let (mut plane, _, pane) = live_fixture("substop");
        let client = register_live_client(&mut plane);
        let (_, through, _) = plane
            .subscribe_frame(client, pane, 0, true, false)
            .expect("seed catch-up");
        let plane = Arc::new(Mutex::new(plane));
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let req = ControlRequest::SubscribePane {
            version: PROTOCOL_VERSION,
            request_id: 9,
            client_id: client,
            pane_id: pane,
            from_seq: through,
            timeout_ms: 30_000,
        };
        let waiter_plane = Arc::clone(&plane);
        let waiter_stop = Arc::clone(&stop);
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            handle_subscribe_pane(&waiter_plane, &req, 9, &waiter_stop, &mut writer)
        });
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Release);
        // Drop the plane guard before ringing (PT-272). Chaining
        // `lock().pane_log_watch().ring()` inverts waiter order and deadlocks.
        let watch = plane.lock().unwrap().pane_log_watch();
        watch.ring();
        waiter.join().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop must cancel SubscribePane wait"
        );
        drop(reader);
    }

    #[test]
    fn subscribe_pane_socket_rejects_zero_request_id_and_wrong_version() {
        let (plane, _, pane) = live_fixture("subbad");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let zero = request(
            &mut stream,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 0,
                client_id: client,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            },
        );
        assert_eq!(error_code(&zero), ControlErrorCode::InvalidRequest);
        let wrong = request(
            &mut stream,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION + 1,
                request_id: 2,
                client_id: client,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            },
        );
        assert_eq!(error_code(&wrong), ControlErrorCode::IncompatibleVersion);
        let reuse = request(
            &mut stream,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: client,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            },
        );
        assert_eq!(error_code(&reuse), ControlErrorCode::StaleRequestId);
        let next = request(
            &mut stream,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: client,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            },
        );
        assert!(matches!(next.body, ControlResponseBody::Ok { .. }));
        drop(server);
    }

    #[test]
    fn subscribe_pane_socket_disconnect_releases_lease() {
        use std::io::BufRead;
        let (plane, _, pane) = live_fixture("subdisc");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut holder = UnixStream::connect(server.path()).unwrap();
        let holder_id = registered_client(request(
            &mut holder,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let acquired = request(
            &mut holder,
            &ControlRequest::AcquireLease {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: holder_id,
                pane_id: pane,
            },
        );
        assert!(matches!(acquired.body, ControlResponseBody::Ok { .. }));
        serde_json::to_writer(
            &mut holder,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: holder_id,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 30_000,
            },
        )
        .unwrap();
        holder.write_all(b"\n").unwrap();
        holder.flush().unwrap();
        holder
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(holder.try_clone().unwrap())
            .read_line(&mut line)
            .expect("catch-up frame");
        let _ = holder.shutdown(std::net::Shutdown::Both);
        drop(holder);

        let mut other = UnixStream::connect(server.path()).unwrap();
        let other_id = registered_client(request(
            &mut other,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let started = Instant::now();
        let mut leased = false;
        let mut req_id = 2u64;
        while started.elapsed() < Duration::from_secs(3) {
            let response = request(
                &mut other,
                &ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: req_id,
                    client_id: other_id,
                    pane_id: pane,
                },
            );
            req_id += 1;
            if matches!(response.body, ControlResponseBody::Ok { .. }) {
                leased = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            leased,
            "disconnect must unblock SubscribePane and drop the lease"
        );
        drop(server);
    }

    #[test]
    fn subscribe_pane_socket_pane_close_sends_done() {
        use std::io::BufRead;
        let (plane, window, pane) = live_fixture("subclose");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut control = UnixStream::connect(server.path()).unwrap();
        let _controller = registered_client(request(
            &mut control,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let _ = request(
            &mut control,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 2,
            },
        );
        let split = request(
            &mut control,
            &ControlRequest::Split {
                version: PROTOCOL_VERSION,
                request_id: 3,
                window_id: window,
                target_pane_id: pane,
                axis: AxisWire::Horizontal,
                ratio: 0.5,
                spawn: live_shell_spawn("subclose2"),
                client_id: None,
            },
        );
        assert!(matches!(split.body, ControlResponseBody::Ok { .. }));
        let snapshot = request(
            &mut control,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 4,
            },
        );
        let new_pane = match snapshot.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => {
                snapshot.sessions[0].windows[0]
                    .panes
                    .iter()
                    .find(|candidate| candidate.id != pane)
                    .unwrap()
                    .id
            }
            other => panic!("expected snapshot, got {other:?}"),
        };

        let mut waiter = UnixStream::connect(server.path()).unwrap();
        let waiter_id = registered_client(request(
            &mut waiter,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        serde_json::to_writer(
            &mut waiter,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: waiter_id,
                pane_id: new_pane,
                from_seq: 0,
                timeout_ms: 5_000,
            },
        )
        .unwrap();
        waiter.write_all(b"\n").unwrap();
        waiter.flush().unwrap();
        waiter
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(waiter.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).expect("catch-up");
        let first: ControlResponse = serde_json::from_str(line.trim()).unwrap();
        assert!(matches!(
            first.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::PaneSubscribe { done: false, .. }
            }
        ));

        let closed = request(
            &mut control,
            &ControlRequest::Close {
                version: PROTOCOL_VERSION,
                request_id: 5,
                window_id: window,
                pane_id: new_pane,
                prior_focus_id: pane,
                client_id: None,
            },
        );
        assert!(matches!(closed.body, ControlResponseBody::Ok { .. }));

        line.clear();
        reader.read_line(&mut line).expect("terminal frame");
        let last: ControlResponse = serde_json::from_str(line.trim()).unwrap();
        match last.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::PaneSubscribe { done, events, .. },
            } => {
                assert!(done);
                assert!(events.is_empty());
            }
            other => panic!("expected done PaneSubscribe, got {other:?}"),
        }
        drop(server);
    }

    #[test]
    fn subscribe_pane_socket_gap_snapshot_is_terminal() {
        let (mut plane, _, pane) = live_fixture("subgap");
        plane.shrink_pane_log_for_test(pane, 48);
        let client = register_live_client(&mut plane);
        for i in 0..12 {
            let set = plane.handle(ControlRequest::SetPaneStatus {
                version: PROTOCOL_VERSION,
                request_id: 10 + i,
                pane_id: pane,
                text: Some(format!("status-{i}-xxxxxxxx")),
            });
            assert!(matches!(set.body, ControlResponseBody::Ok { .. }));
        }
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let socket_client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let response = request(
            &mut stream,
            &ControlRequest::SubscribePane {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: socket_client,
                pane_id: pane,
                from_seq: 0,
                timeout_ms: 0,
            },
        );
        match response.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::PaneSubscribe {
                        gap,
                        done,
                        snapshot,
                        events,
                        ..
                    },
            } => {
                assert!(gap, "from_seq 0 after wrap must be a gap");
                assert!(done);
                assert!(snapshot.is_some());
                assert!(events.is_empty());
            }
            other => panic!("expected gap PaneSubscribe, got {other:?}"),
        }
        drop(server);
        let _ = client;
    }

    #[test]
    fn mail_wait_served_over_socket_wakes_on_send() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut waiter_stream = UnixStream::connect(server.path()).unwrap();
        let mut sender_stream = UnixStream::connect(server.path()).unwrap();

        let peer = registered_client(request(
            &mut waiter_stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let cursor = registered_client(request(
            &mut sender_stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        for (stream, client, agent) in [
            (&mut waiter_stream, peer, "operator-a"),
            (&mut sender_stream, cursor, "operator-b"),
        ] {
            let seated = request(
                stream,
                &ControlRequest::MailHello {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    agent: agent.into(),
                },
            );
            assert!(matches!(
                seated.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::MailSeated { .. }
                }
            ));
        }

        let waiter = std::thread::spawn(move || {
            request(
                &mut waiter_stream,
                &ControlRequest::MailWait {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: peer,
                    timeout_ms: 30_000,
                },
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        let sent = request(
            &mut sender_stream,
            &ControlRequest::MailSend {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: cursor,
                to: "operator-a".into(),
                summary: "over the socket".into(),
                body: String::new(),
            },
        );
        assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));
        let started = Instant::now();
        let response = waiter.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "socket wait must wake on send, not run to timeout"
        );
        assert!(matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 1, .. }
            }
        ));
        drop(server);
    }

    #[test]
    fn disconnect_drops_agent_binding() {
        let (mut plane, _, _) = fixture(8);
        let peer = mail_hello(&mut plane, 1, "operator-a");
        let cursor = mail_hello(&mut plane, 2, "operator-b");
        let _ = plane.handle(ControlRequest::DisconnectClient {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: peer,
        });
        let refused = plane.handle(ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: peer,
        });
        assert_eq!(error_code(&refused), ControlErrorCode::StaleId);
        // A fresh connection can still claim operator-a's held mail (recovery).
        let _ = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "durable".into(),
            body: String::new(),
        });
        let peer2 = mail_hello(&mut plane, 6, "operator-a");
        let claimed = plane.handle(ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: peer2,
        });
        assert!(matches!(
            &claimed.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailLetters { letters }
            } if letters.len() == 1 && letters[0].summary == "durable"
        ));
    }

    #[test]
    fn doorbell_arms_mail_attention_on_send() {
        let (mut plane, _, _) = fixture(8);
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        let _peer = mail_hello(&mut plane, 2, "operator-a");
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        let (_, _, peer_pane) = session_ids(&created);

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "ring".into(),
            body: String::new(),
        });
        assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));

        let attention = plane
            .mail_attention
            .get(&peer_pane)
            .expect("doorbell must arm attention on the recipient pane");
        assert_eq!(attention.cell, MAIL_ATTENTION_CELL);
        assert_eq!(attention.depth, 1);
        assert_eq!(attention.queue_rev, 1);
        assert!(matches!(
            mail_events(&plane, 0).last(),
            Some(Event::MailAttentionChanged {
                pane_id,
                depth: 1,
                ..
            }) if *pane_id == peer_pane
        ));
    }

    #[test]
    fn doorbell_skips_unknown_agents_mail_queues() {
        let (mut plane, _, _) = fixture(8);
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: cursor,
            to: "offline-agent".into(),
            summary: "queued".into(),
            body: String::new(),
        });
        assert!(matches!(
            sent.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailSent { depth: 1, .. }
            }
        ));
        assert!(
            plane.mail_attention.is_empty(),
            "no session, no doorbell — the letter queues silently"
        );
    }

    #[test]
    fn doorbell_attention_follows_claim_release_depth() {
        let (mut plane, _, _) = fixture(8);
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        let peer = mail_hello(&mut plane, 2, "operator-a");
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        let (_, _, peer_pane) = session_ids(&created);

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "ring".into(),
            body: String::new(),
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::MailSent { id, .. },
        } = sent.body
        else {
            panic!("expected MailSent: {sent:?}");
        };
        assert_eq!(plane.mail_attention[&peer_pane].depth, 1);

        // Claim reads everything open: the indicator clears.
        let _ = plane.handle(ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: peer,
        });
        assert!(
            !plane.mail_attention.contains_key(&peer_pane),
            "claim with nothing left open must clear the indicator"
        );

        // Release re-queues: the indicator re-lights without injecting.
        let _ = plane.handle(ControlRequest::MailRelease {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: peer,
            ids: vec![id],
        });
        let attention = plane
            .mail_attention
            .get(&peer_pane)
            .expect("release must re-arm the indicator");
        assert_eq!(attention.depth, 1);
        assert_eq!(attention.wake, None, "refresh never re-arms the wake");
        assert_eq!(
            attention.queue_rev, 3,
            "rev moves monotonically per refresh"
        );
    }

    #[test]
    fn doorbell_broadcast_rings_each_live_recipient() {
        let (mut plane, _, _) = fixture(8);
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        let _peer = mail_hello(&mut plane, 2, "operator-a");
        for (name, agent) in [("work", "operator-b"), ("other", "operator-a")] {
            let created = plane.handle(ControlRequest::CreateSession {
                version: PROTOCOL_VERSION,
                request_id: 3,
                name: name.into(),
                spawn: spawn(),
                cols: None,
                rows: None,
                agent_id: Some(agent.into()),
                headless: false,
            });
            assert!(matches!(created.body, ControlResponseBody::Ok { .. }));
        }
        let broadcast = plane.handle(ControlRequest::MailBroadcast {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: cursor,
            summary: "standup".into(),
            body: String::new(),
        });
        assert!(matches!(broadcast.body, ControlResponseBody::Ok { .. }));
        // Only operator-a's pane lights: the caller's own agent is excluded.
        assert_eq!(plane.mail_attention.len(), 1);
        let attention = plane.mail_attention.values().next().unwrap();
        assert_eq!(attention.cell, MAIL_ATTENTION_CELL);
        assert_eq!(attention.depth, 1);
    }

    #[test]
    fn doorbell_injects_pmux_token_into_live_pane() {
        // 10 bytes = "PMUX_MAIL\r" — the child consumes exactly the
        // doorbell payload, then prints WOKE.
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let session = plane.snapshot().unwrap().sessions[0].id;
        let session = session_id(session).unwrap();
        plane
            .domain
            .set_agent_id(session, Some("operator-a".into()))
            .unwrap();
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        quiet_unattended(&mut plane, pane);

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "ring".into(),
            body: String::new(),
        });
        assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));
        let attention = plane
            .mail_attention
            .get(&pane)
            .expect("doorbell must arm attention");
        assert_eq!(attention.cell, MAIL_ATTENTION_CELL);
        assert_eq!(
            plane.mail_inject_writes(pane),
            1,
            "in-process doorbell must inject without any external caller"
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_woke = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("WOKE"))
            {
                saw_woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            saw_woke,
            "child should consume the PMUX_MAIL doorbell payload"
        );
    }

    #[test]
    fn doorbell_inject_completes_under_10ms() {
        // (1b.3): in-process send → PMUX_MAIL write is
        // synchronous inside MailSend. The external watcher path worst
        // case was ~15s (death-watch). After the quiet stamp,
        // this call must finish in under 10ms. Busy-defer is a
        // different case (dogfood); this is the happy path.
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; exec sleep 999",
        );
        let session = plane.snapshot().unwrap().sessions[0].id;
        let session = session_id(session).unwrap();
        plane
            .domain
            .set_agent_id(session, Some("operator-a".into()))
            .unwrap();
        let cursor = mail_hello(&mut plane, 1, "operator-b");
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        quiet_unattended(&mut plane, pane);

        // Warm the store + inject path so the timed send is not the
        // first SQLite write or PTY setup.
        let warmup = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "warmup".into(),
            body: String::new(),
        });
        assert!(matches!(warmup.body, ControlResponseBody::Ok { .. }));
        assert_eq!(plane.mail_inject_writes(pane), 1);
        quiet_unattended(&mut plane, pane);

        let started = Instant::now();
        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: cursor,
            to: "operator-a".into(),
            summary: "latency".into(),
            body: String::new(),
        });
        let elapsed = started.elapsed();
        assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));
        assert_eq!(
            plane.mail_inject_writes(pane),
            2,
            "in-process path must inject during MailSend"
        );
        // Debug builds plus SQLite plus a PTY write miss 10ms on a
        // loaded host. The product bar is <10ms vs ~15s external
        // death-watch; 50ms still proves the in-process path.
        let budget = test_time_budget(Duration::from_millis(if cfg!(debug_assertions) {
            50
        } else {
            10
        }));
        assert!(
            elapsed < budget,
            "send → inject must be <{budget:?}, was {elapsed:?} (external doorbell worst case ~15s)"
        );
    }

    #[test]
    fn create_session_rearms_mail_queued_while_session_gone() {
        let (mut plane, _) = live_inject_fixture("exec sleep 999");
        let sender = mail_hello(&mut plane, 1, "operator-b");
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 2,
            name: "seat".into(),
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), "exec sleep 999".into()],
                cwd: Some(PathBuf::from("/tmp")),
                env: BTreeMap::new(),
            },
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        let (session, _, gone_pane) = session_ids(&created);
        let destroyed = plane.handle(ControlRequest::DestroySession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            session_id: session,
        });
        assert!(matches!(destroyed.body, ControlResponseBody::Ok { .. }));

        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: sender,
            to: "operator-a".into(),
            summary: "queued while gone".into(),
            body: "rearm after recreate".into(),
        });
        assert!(matches!(
            sent.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailSent { depth: 1, .. }
            }
        ));
        assert!(
            !plane.mail_attention.contains_key(&gone_pane),
            "destroyed pane must not keep attention"
        );

        let recreated = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 5,
            name: "seat".into(),
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec![
                    "-c".into(),
                    "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999".into(),
                ],
                cwd: Some(PathBuf::from("/tmp")),
                env: BTreeMap::new(),
            },
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        let (_, _, pane) = session_ids(&recreated);
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        quiet_unattended(&mut plane, pane);
        plane.retry_mail_nudges();

        let attention = plane
            .mail_attention
            .get(&pane)
            .expect("recreate must re-arm attention for queued mail");
        assert_eq!(attention.cell, MAIL_ATTENTION_CELL);
        assert_eq!(attention.depth, 1);
        assert!(
            plane.mail_inject_writes(pane) >= 1,
            "recreate must inject PMUX_MAIL for mail queued while gone"
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_woke = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("WOKE"))
            {
                saw_woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_woke, "recreated child must consume PMUX_MAIL");
    }

    #[test]
    fn shutdown_server_round_trip_rejects_unregistered_client() {
        let (mut plane, _, _) = fixture(8);
        let wire = ControlRequest::ShutdownServer {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: 3,
        };
        let encoded = serde_json::to_string(&wire).unwrap();
        assert!(encoded.contains("\"type\":\"shutdown_server\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, wire);

        let rejected = plane.handle(ControlRequest::ShutdownServer {
            version: PROTOCOL_VERSION,
            request_id: 1,
            client_id: 3,
        });
        assert_eq!(error_code(&rejected), ControlErrorCode::StaleId);

        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let accepted = plane.handle(ControlRequest::ShutdownServer {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
        });
        assert!(matches!(
            accepted.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::ShutdownAccepted
            }
        ));
        assert_eq!(plane.sequence(), 0, "shutdown must not emit an event");
    }

    #[test]
    fn idle_shutdown_fences_session_creation_and_rejects_live_sessions() {
        let (mut plane, _, _) = fixture(8);
        let client_id = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let request = ControlRequest::ShutdownIdle {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id,
        };
        assert_eq!(
            error_code(&plane.handle(request.clone())),
            ControlErrorCode::InvalidRequest
        );
        let session_id = plane.domain.sessions().next().unwrap().id.get();
        plane.destroy_session(session_id).unwrap();
        assert!(matches!(
            plane.handle(request).body,
            ControlResponseBody::Ok {
                response: ControlResponseData::ShutdownAccepted
            }
        ));
        // Every later control operation is fenced while the reply is flushed.
        assert_eq!(
            error_code(&plane.handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 3,
            })),
            ControlErrorCode::InvalidRequest
        );
        assert!(plane.domain.sessions().next().is_none());
    }

    #[test]
    fn phase2b_shutdown_server_is_accepted_then_server_exits_cleanly() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let started = Instant::now();
        let accepted = request(
            &mut stream,
            &ControlRequest::ShutdownServer {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: client,
            },
        );
        assert!(
            matches!(
                accepted.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::ShutdownAccepted
                }
            ),
            "ShutdownAccepted must be flushed before teardown: {accepted:?}"
        );
        server.wait_shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown signal must be observable after the response is written"
        );
        drop(server);
        assert!(!path.exists(), "server Drop must remove the owned socket");
    }

    #[test]
    fn snapshot_mutations_and_events_are_ordered() {
        let (plane, window, first) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();

        let snapshot = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        match snapshot.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => {
                assert_eq!(snapshot.sequence, 0);
                assert_eq!(snapshot.sessions[0].windows[0].panes[0].id, first);
                assert_eq!(snapshot.sessions[0].windows[0].panes[0].geometry.cols, 80);
            }
            other => panic!("unexpected snapshot response: {other:?}"),
        }

        let split = request(
            &mut stream,
            &ControlRequest::Split {
                version: PROTOCOL_VERSION,
                request_id: 2,
                window_id: window,
                target_pane_id: first,
                axis: AxisWire::Horizontal,
                ratio: 0.25,
                spawn: spawn(),
                client_id: None,
            },
        );
        assert_eq!(mutation_sequence(&split), 1);
        let resize = request(
            &mut stream,
            &ControlRequest::Resize {
                version: PROTOCOL_VERSION,
                request_id: 3,
                window_id: window,
                cols: 100,
                rows: 30,
                cell_width_px: None,
                cell_height_px: None,
                fit: false,
                host: false,
                client_id: None,
            },
        );
        assert_eq!(mutation_sequence(&resize), 2);
        let focus = request(
            &mut stream,
            &ControlRequest::SuggestFocus {
                version: PROTOCOL_VERSION,
                request_id: 4,
                window_id: window,
                pane_id: first,
                reason: None,
            },
        );
        assert_eq!(mutation_sequence(&focus), 3);
        let events = request(
            &mut stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 5,
                after_sequence: 0,
                limit: None,
            },
        );
        match events.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Events { batch },
            } => {
                assert_eq!(batch.current_sequence, 3);
                assert_eq!(batch.through_sequence, 3);
                assert!(!batch.has_more);
                assert_eq!(
                    batch
                        .events
                        .iter()
                        .map(|event| event.sequence)
                        .collect::<Vec<_>>(),
                    vec![1, 2, 3]
                );
                let Event::PaneSplit { geometry, .. } = &batch.events[0].event else {
                    panic!("first event was not split")
                };
                assert_eq!(geometry[0].cols, 20);
                assert_eq!(geometry[1].cols, 60);
                let Event::GeometryChanged { geometry, .. } = &batch.events[1].event else {
                    panic!("second event was not resize")
                };
                assert_eq!(geometry[0].cols, 25);
                assert_eq!(geometry[1].cols, 75);
            }
            other => panic!("unexpected events response: {other:?}"),
        }
    }

    #[test]
    fn replay_gap_and_ahead_sequence_require_resnapshot() {
        let (mut plane, window, pane) = fixture(2);
        for request in [
            ControlRequest::SuggestFocus {
                version: PROTOCOL_VERSION,
                request_id: 1,
                window_id: window,
                pane_id: pane,
                reason: None,
            },
            ControlRequest::Resize {
                version: PROTOCOL_VERSION,
                request_id: 2,
                window_id: window,
                cols: 81,
                rows: 24,
                cell_width_px: None,
                cell_height_px: None,
                fit: false,
                host: false,
                client_id: None,
            },
            ControlRequest::Resize {
                version: PROTOCOL_VERSION,
                request_id: 3,
                window_id: window,
                cols: 82,
                rows: 24,
                cell_width_px: None,
                cell_height_px: None,
                fit: false,
                host: false,
                client_id: None,
            },
        ] {
            assert!(matches!(
                plane.handle(request).body,
                ControlResponseBody::Ok { .. }
            ));
        }
        let gap = plane.handle(ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id: 4,
            after_sequence: 0,
            limit: None,
        });
        match gap.body {
            ControlResponseBody::Error { error } => {
                assert_eq!(error.code, ControlErrorCode::EventGap);
                assert!(error.resnapshot_required);
                assert_eq!(error.oldest_available_sequence, Some(2));
                assert_eq!(error.current_sequence, Some(3));
            }
            other => panic!("expected event gap, got {other:?}"),
        }
        let ahead = plane.handle(ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id: 5,
            after_sequence: 4,
            limit: None,
        });
        assert_eq!(error_code(&ahead), ControlErrorCode::StaleSequence);
    }

    #[test]
    fn bounded_event_batch_exposes_resume_cursor() {
        let (mut plane, window, pane) = fixture(8);
        for (request_id, cols) in [(1, 81), (2, 82), (3, 83)] {
            let response = plane.handle(ControlRequest::Resize {
                version: PROTOCOL_VERSION,
                request_id,
                window_id: window,
                cols,
                rows: 24,
                cell_width_px: None,
                cell_height_px: None,
                fit: false,
                host: false,
                client_id: None,
            });
            assert!(matches!(response.body, ControlResponseBody::Ok { .. }));
        }
        let first = plane.handle(ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id: 4,
            after_sequence: 0,
            limit: Some(2),
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::Events { batch },
        } = first.body
        else {
            panic!("expected event batch")
        };
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.through_sequence, 2);
        assert_eq!(batch.current_sequence, 3);
        assert!(batch.has_more);

        let second = plane.handle(ControlRequest::Events {
            version: PROTOCOL_VERSION,
            request_id: 5,
            after_sequence: batch.through_sequence,
            limit: Some(2),
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::Events { batch },
        } = second.body
        else {
            panic!("expected resumed event batch")
        };
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.through_sequence, 3);
        assert!(!batch.has_more);
        assert_eq!(
            pane,
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0].id
        );
    }

    #[test]
    fn phase2a_close_removes_spawn_metadata_and_unknown_ids_are_stale() {
        let (mut plane, window, first) = fixture(8);
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            target_pane_id: first,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: None,
        });
        let sequence = mutation_sequence(&split);
        assert_eq!(sequence, 1);
        let snapshot = plane.snapshot().unwrap();
        let panes = &snapshot.sessions[0].windows[0].panes;
        let second = panes.iter().find(|pane| pane.id != first).unwrap().id;
        assert!(panes
            .iter()
            .find(|pane| pane.id == second)
            .unwrap()
            .spawn
            .is_some());

        let close = plane.handle(ControlRequest::Close {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            pane_id: second,
            prior_focus_id: second,
            client_id: None,
        });
        assert_eq!(mutation_sequence(&close), 2);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes.len(),
            1
        );
        let stale = plane.handle(ControlRequest::SuggestFocus {
            version: PROTOCOL_VERSION,
            request_id: 3,
            window_id: window,
            pane_id: second,
            reason: None,
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
    }

    #[test]
    fn close_rejects_stale_prior_focus_without_mutation() {
        let (mut plane, window, first) = fixture(8);
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            target_pane_id: first,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: None,
        });
        assert_eq!(mutation_sequence(&split), 1);
        let second = plane.snapshot().unwrap().sessions[0].windows[0].panes[1].id;
        let response = plane.handle(ControlRequest::Close {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            pane_id: second,
            prior_focus_id: u64::MAX,
            client_id: None,
        });
        assert_eq!(error_code(&response), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 1);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes.len(),
            2
        );
    }

    #[test]
    fn failed_geometry_mutations_are_atomic_and_unsequenced() {
        let (mut plane, window, _pane) = fixture(8);
        let too_small = plane.handle(ControlRequest::Resize {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            cols: 1,
            rows: 24,
            cell_width_px: None,
            cell_height_px: None,
            fit: false,
            host: false,
            client_id: None,
        });
        assert_eq!(error_code(&too_small), ControlErrorCode::TooSmall);
    }

    fn resize_ok(
        plane: &mut ControlPlane,
        request_id: u64,
        window: u64,
        size: (u32, u32),
        client_id: u64,
        flags: (bool, bool),
    ) {
        assert!(matches!(
            plane
                .handle(ControlRequest::Resize {
                    version: PROTOCOL_VERSION,
                    request_id,
                    window_id: window,
                    cols: size.0,
                    rows: size.1,
                    cell_width_px: None,
                    cell_height_px: None,
                    fit: flags.0,
                    host: flags.1,
                    client_id: Some(client_id),
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
    }

    #[test]
    fn latest_remote_shrinks_then_disconnect_restores_host() {
        let (mut plane, window, _pane) = fixture(8);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        resize_ok(&mut plane, 4, window, (40, 20), remote, (false, false));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (40, 20)
        );
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 5,
                    client_id: remote,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (80, 24),
            "TTY detach must restore the last host geometry"
        );
    }

    #[test]
    fn host_policy_ignores_remote_unless_fit() {
        let (mut plane, window, _pane) = fixture(8);
        plane.set_remote_size(RemoteSizePolicy::Host);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        resize_ok(&mut plane, 4, window, (40, 20), remote, (false, false));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (80, 24)
        );
        resize_ok(&mut plane, 5, window, (40, 20), remote, (true, false));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (40, 20)
        );
    }

    #[test]
    fn host_geometry_pass_reclaims_size_after_remote() {
        let (mut plane, window, _pane) = fixture(8);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        resize_ok(&mut plane, 4, window, (40, 20), remote, (false, false));
        resize_ok(&mut plane, 5, window, (80, 24), host, (false, true));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (80, 24)
        );
    }

    #[test]
    fn fit_resize_applies_this_terminal_even_when_smaller() {
        let (mut plane, window, _pane) = fixture(8);
        let large = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let small = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::Resize {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    window_id: window,
                    cols: 80,
                    rows: 24,
                    cell_width_px: None,
                    cell_height_px: None,
                    fit: false,
                    host: false,
                    client_id: Some(large),
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        assert!(matches!(
            plane
                .handle(ControlRequest::Resize {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    window_id: window,
                    cols: 40,
                    rows: 20,
                    cell_width_px: None,
                    cell_height_px: None,
                    fit: true,
                    host: false,
                    client_id: Some(small),
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let bounds = plane.bounds[&window];
        assert_eq!((bounds.cols, bounds.rows), (40, 20));
    }

    #[test]
    fn disconnect_of_unmarked_clients_restores_seeded_host_bounds() {
        let (mut plane, window, _pane) = fixture(8);
        let large = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let small = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (90, 30), large, (false, false));
        resize_ok(&mut plane, 4, window, (40, 20), small, (false, false));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (40, 20)
        );
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 5,
                    client_id: small,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (80, 24),
            "disconnect restores the seeded host geometry, not the leftover remote"
        );
    }

    #[test]
    fn same_size_resize_does_not_advance_sequence() {
        let (mut plane, window, _pane) = fixture(8);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        let seq = plane.sequence;
        resize_ok(&mut plane, 4, window, (80, 24), host, (false, true));
        assert_eq!(
            plane.sequence, seq,
            "same-size host report must not emit GeometryChanged"
        );
        resize_ok(&mut plane, 5, window, (40, 20), remote, (false, false));
        let seq = plane.sequence;
        resize_ok(&mut plane, 6, window, (40, 20), remote, (false, false));
        assert_eq!(
            plane.sequence, seq,
            "same-size remote report must not emit GeometryChanged"
        );
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (40, 20)
        );
    }

    #[test]
    fn host_echo_of_applied_keeps_host_chosen() {
        let (mut plane, window, _pane) = fixture(8);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        resize_ok(&mut plane, 4, window, (40, 20), remote, (false, false));
        let seq = plane.sequence;
        resize_ok(&mut plane, 5, window, (40, 20), host, (false, true));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (40, 20),
            "echo of the remote-applied size must not change bounds"
        );
        assert_eq!(
            plane.sequence,
            seq + 1,
            "echo transfers SizeOwner to the host without GeometryChanged"
        );
        let owner = snapshot_size_owner(&mut plane, 6).expect("owner after echo");
        assert_eq!(owner.client_id, host);
        assert_eq!(owner.kind, SizeOwnerKind::Host);
        let host_chosen = plane.host_viewport(window).expect("host-chosen");
        assert_eq!(
            (host_chosen.cols, host_chosen.rows),
            (80, 24),
            "host-chosen stays the desktop size; echo must not overwrite it"
        );
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 7,
                    client_id: remote,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        assert_eq!(
            (plane.bounds[&window].cols, plane.bounds[&window].rows),
            (80, 24)
        );
    }

    fn snapshot_size_owner(plane: &mut ControlPlane, request_id: u64) -> Option<SizeOwner> {
        match plane
            .handle(ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => snapshot.sessions[0].windows[0].panes[0].size_owner,
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn size_owner_round_trips_on_remote_attach_and_detach() {
        let (mut plane, window, _pane) = fixture(8);
        let host = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let remote = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        resize_ok(&mut plane, 3, window, (80, 24), host, (false, true));
        let owner = snapshot_size_owner(&mut plane, 4).expect("host owner");
        assert_eq!(owner.client_id, host);
        assert_eq!(owner.kind, SizeOwnerKind::Host);
        resize_ok(&mut plane, 5, window, (40, 20), remote, (false, false));
        let owner = snapshot_size_owner(&mut plane, 6).expect("remote owner");
        assert_eq!(owner.client_id, remote);
        assert_eq!(owner.kind, SizeOwnerKind::Remote);
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 7,
                    client_id: remote,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let owner = snapshot_size_owner(&mut plane, 8).expect("host owns after detach");
        assert_eq!(owner.client_id, host);
        assert_eq!(owner.kind, SizeOwnerKind::Host);
        assert_eq!(
            crate::remote_size::remote_size_chip(Some(owner), Some(host), (80, 24)),
            None,
            "chip gone after detach"
        );
    }

    #[test]
    fn failed_geometry_mutations_continue_after_too_small() {
        let (mut plane, window, pane) = fixture(8);
        let bad_ratio = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Horizontal,
            ratio: 1.0,
            spawn: spawn(),
            client_id: None,
        });
        assert_eq!(error_code(&bad_ratio), ControlErrorCode::InvalidRequest);
        let last_pane = plane.handle(ControlRequest::Close {
            version: PROTOCOL_VERSION,
            request_id: 3,
            window_id: window,
            pane_id: pane,
            prior_focus_id: pane,
            client_id: None,
        });
        assert_eq!(error_code(&last_pane), ControlErrorCode::LastPane);
        let snapshot = plane.snapshot().unwrap();
        assert_eq!(snapshot.sequence, 0);
        assert_eq!(snapshot.sessions[0].windows[0].bounds.cols, 80);
        assert_eq!(snapshot.sessions[0].windows[0].panes.len(), 1);
    }

    #[test]
    fn structured_spawn_rejects_shellish_ambiguity_before_mutation() {
        let (mut plane, window, pane) = fixture(8);
        let response = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: SpawnSpec {
                program: String::new(),
                argv: vec!["echo bad".into()],
                cwd: Some(PathBuf::from("relative")),
                env: BTreeMap::new(),
            },
            client_id: None,
        });
        assert_eq!(error_code(&response), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 0);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes.len(),
            1
        );

        let oversized = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["x".repeat(MAX_SPAWN_BYTES)],
                cwd: None,
                env: BTreeMap::new(),
            },
            client_id: None,
        });
        assert_eq!(error_code(&oversized), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 0);
    }

    #[test]
    fn slow_partial_client_does_not_starve_an_independent_client() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut slow = UnixStream::connect(server.path()).unwrap();
        slow.write_all(b"{\"type\":\"snapshot\"").unwrap();

        let start = Instant::now();
        let mut fast = UnixStream::connect(server.path()).unwrap();
        let response = request(
            &mut fast,
            &ControlRequest::Ping {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        assert!(matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Pong
            }
        ));
        assert!(start.elapsed() < test_time_budget(IO_TIMEOUT));
    }

    #[test]
    fn large_response_survives_a_slow_client_across_write_timeouts() {
        // A response larger than the socket send buffer (8 KiB on macOS) needs
        // several `write` calls. With the production write timeout, a client
        // that drains slowly makes an intermediate call return WouldBlock after
        // a partial write. The frame must still complete intact, not be dropped
        // mid-write (which truncated the client's read and caused EPIPE).
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        let stop = Arc::new(AtomicBool::new(false));

        let expected = payload.clone();
        let consumer = thread::spawn(move || {
            // Stall long enough to force several write timeouts before draining.
            thread::sleep(IO_TIMEOUT * 3);
            let mut received = Vec::with_capacity(expected.len());
            let mut buf = [0u8; 4096];
            while received.len() < expected.len() {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            received
        });

        write_all_timeout_aware(&mut writer, &payload, &stop).unwrap();
        drop(writer);
        let received = consumer.join().unwrap();
        assert_eq!(received, payload);
    }

    #[test]
    fn large_response_write_aborts_on_shutdown() {
        // A client that never drains must not pin a server thread forever; the
        // stop flag ends the write loop.
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        writer.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        let payload = vec![0u8; 4 * 1024 * 1024];
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let raiser = thread::spawn(move || {
            thread::sleep(IO_TIMEOUT * 2);
            flag.store(true, Ordering::Release);
        });
        let result = write_all_timeout_aware(&mut writer, &payload, &stop);
        raiser.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn reconnect_must_resnapshot_before_event_replay() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let response = request(
            &mut stream,
            &ControlRequest::Events {
                version: 99,
                request_id: 1,
                after_sequence: 0,
                limit: None,
            },
        );
        assert_eq!(error_code(&response), ControlErrorCode::IncompatibleVersion);
        let response = request(
            &mut stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 2,
                after_sequence: 0,
                limit: None,
            },
        );
        assert_eq!(error_code(&response), ControlErrorCode::SnapshotRequired);
        let ControlResponseBody::Error { error } = &response.body else {
            unreachable!()
        };
        assert!(error.resnapshot_required);
        assert_eq!(error.current_sequence, Some(0));
        let snapshot = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 3,
            },
        );
        assert!(matches!(
            snapshot.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { .. }
            }
        ));
        let events = request(
            &mut stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 4,
                after_sequence: 0,
                limit: None,
            },
        );
        assert!(matches!(
            events.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Events { .. }
            }
        ));
    }

    #[test]
    fn phase2a_wire_event_gap_revokes_snapshot_until_resync() {
        let (plane, window, _) = fixture(1);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let snapshot = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        assert!(matches!(snapshot.body, ControlResponseBody::Ok { .. }));
        for (request_id, cols) in [(2, 81), (3, 82)] {
            let mutation = request(
                &mut stream,
                &ControlRequest::Resize {
                    version: PROTOCOL_VERSION,
                    request_id,
                    window_id: window,
                    cols,
                    rows: 24,
                    cell_width_px: None,
                    cell_height_px: None,
                    fit: false,
                    host: false,
                    client_id: None,
                },
            );
            assert!(matches!(mutation.body, ControlResponseBody::Ok { .. }));
        }
        let gap = request(
            &mut stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 4,
                after_sequence: 0,
                limit: None,
            },
        );
        assert_eq!(error_code(&gap), ControlErrorCode::EventGap);
        let requires_snapshot = request(
            &mut stream,
            &ControlRequest::Events {
                version: PROTOCOL_VERSION,
                request_id: 5,
                after_sequence: 1,
                limit: None,
            },
        );
        assert_eq!(
            error_code(&requires_snapshot),
            ControlErrorCode::SnapshotRequired
        );
    }

    #[test]
    fn phase2a_slow_observer_response_backpressure_does_not_block_fast_client() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut slow = UnixStream::connect(server.path()).unwrap();
        rustix::net::sockopt::set_socket_recv_buffer_size(&slow, 1024).unwrap();
        let mut pipelined = Vec::new();
        for request_id in 1..=2048 {
            serde_json::to_writer(
                &mut pipelined,
                &ControlRequest::Snapshot {
                    version: PROTOCOL_VERSION,
                    request_id,
                },
            )
            .unwrap();
            pipelined.push(b'\n');
        }
        // Saturate the slow observer from its own thread. On macOS the small
        // socket send buffer cannot hold all 2048 requests, so `write_all`
        // blocks; meanwhile the server's bounded write timeout can close the
        // throttled connection and surface EPIPE here. That is fine — the test
        // only needs the slow observer's server-side handler backed up while a
        // fast client is served on its own thread. Keep `slow` alive in the
        // thread so the connection is not dropped before the fast ping.
        let slow_writer = thread::spawn(move || {
            let _ = slow.write_all(&pipelined);
            slow
        });
        thread::sleep(Duration::from_millis(25));

        let start = Instant::now();
        let mut fast = UnixStream::connect(server.path()).unwrap();
        let response = request(
            &mut fast,
            &ControlRequest::Ping {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        );
        assert!(matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Pong
            }
        ));
        assert!(start.elapsed() < test_time_budget(IO_TIMEOUT));
        let _ = slow_writer.join();
    }

    #[test]
    fn phase2a_unregistered_and_disconnected_client_ids_are_stale() {
        let (mut plane, _, pane) = fixture(16);
        let unregistered = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 1,
            client_id: 77,
            pane_id: pane,
        });
        assert_eq!(error_code(&unregistered), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 0);

        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: client,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let stale_write = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: client,
            pane_id: pane,
            data: "must not revive".into(),
        });
        assert_eq!(error_code(&stale_write), ControlErrorCode::StaleId);
    }

    #[test]
    fn phase2a_socket_client_identity_cannot_be_re_registered_or_spoofed() {
        let (plane, _, pane) = fixture(16);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut first = UnixStream::connect(server.path()).unwrap();
        let mut second = UnixStream::connect(server.path()).unwrap();
        let first_id = registered_client(request(
            &mut first,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let second_id = registered_client(request(
            &mut second,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        assert_ne!(first_id, second_id);

        let duplicate = request(
            &mut first,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 2,
            },
        );
        assert_eq!(error_code(&duplicate), ControlErrorCode::InvalidRequest);
        let acquired = request(
            &mut first,
            &ControlRequest::AcquireLease {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: first_id,
                pane_id: pane,
            },
        );
        assert!(matches!(acquired.body, ControlResponseBody::Ok { .. }));

        let spoof = request(
            &mut second,
            &ControlRequest::WritePane {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: first_id,
                pane_id: pane,
                data: "spoof".into(),
            },
        );
        assert_eq!(error_code(&spoof), ControlErrorCode::StaleId);
        let observer = request(
            &mut second,
            &ControlRequest::WritePane {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: second_id,
                pane_id: pane,
                data: "observer".into(),
            },
        );
        assert_eq!(error_code(&observer), ControlErrorCode::NotController);
    }

    #[test]
    fn lease_exclusivity_takeover_and_observer_write_rejection() {
        let (mut plane, _window, pane) = fixture(16);
        let c1 = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register c1: {other:?}"),
        };
        let c2 = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 2,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register c2: {other:?}"),
        };

        let acquire = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: c1,
            pane_id: pane,
        });
        assert!(matches!(
            acquire.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Lease {
                    controller_id: Some(id),
                    ..
                }
            } if id == c1
        ));

        let held = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: c2,
            pane_id: pane,
        });
        assert_eq!(error_code(&held), ControlErrorCode::LeaseHeld);

        let observer_write = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: c2,
            pane_id: pane,
            data: "secret".into(),
        });
        assert_eq!(error_code(&observer_write), ControlErrorCode::NotController);
        match &observer_write.body {
            ControlResponseBody::Error { error } => assert_eq!(error.holder, Some(c1)),
            other => panic!("expected NotController holder, got {other:?}"),
        }

        let controller_write = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: c1,
            pane_id: pane,
            data: "ok".into(),
        });
        assert_eq!(
            error_code(&controller_write),
            ControlErrorCode::InputRouteUnavailable
        );
        assert_eq!(plane.sequence(), 1);

        let takeover = plane.handle(ControlRequest::TakeoverLease {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: c2,
            pane_id: pane,
        });
        assert!(matches!(
            takeover.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Lease {
                    controller_id: Some(id),
                    previous_controller_id: Some(prev),
                    ..
                }
            } if id == c2 && prev == c1
        ));

        let snap = plane.snapshot().unwrap();
        assert_eq!(snap.sessions[0].windows[0].panes[0].controller_id, Some(c2));

        let release = plane.handle(ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id: 8,
            client_id: c2,
            pane_id: pane,
        });
        assert!(matches!(
            release.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Lease {
                    controller_id: None,
                    ..
                }
            }
        ));
    }

    #[test]
    fn write_pane_lease_free_when_no_controller() {
        let (mut plane, _window, pane) = fixture(16);
        let client = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register: {other:?}"),
        };
        let write = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            data: "ok-free".into(),
        });
        assert_eq!(
            error_code(&write),
            ControlErrorCode::InputRouteUnavailable,
            "lease-free write must pass controller check: {write:?}"
        );
    }

    #[test]
    fn disconnect_client_clears_all_held_leases() {
        let (mut plane, window, pane) = fixture(16);
        let c1 = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("{other:?}"),
        };
        let acq = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: c1,
            pane_id: pane,
        });
        assert!(matches!(acq.body, ControlResponseBody::Ok { .. }));
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 3,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: Some(c1),
        });
        let new_pane = match plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .map(|p| p.id)
            .filter(|id| *id != pane)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [id] => *id,
            other => panic!("expected one new pane after split, got {other:?} (split={split:?})"),
        };
        let acq2 = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: c1,
            pane_id: new_pane,
        });
        assert!(matches!(acq2.body, ControlResponseBody::Ok { .. }));
        let disc = plane.handle(ControlRequest::DisconnectClient {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: c1,
        });
        match disc.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::LeasesReleased { pane_ids, .. },
            } => {
                assert!(pane_ids.contains(&pane));
                assert!(pane_ids.contains(&new_pane));
            }
            other => panic!("{other:?}"),
        }
        let snap = plane.snapshot().unwrap();
        for p in &snap.sessions[0].windows[0].panes {
            assert_eq!(p.controller_id, None);
        }
    }

    #[test]
    fn oversized_frame_is_rejected_without_unbounded_read() {
        let (plane, _, _) = fixture(8);
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        stream
            .set_read_timeout(Some(test_time_budget(Duration::from_secs(2))))
            .unwrap();
        // Crossing the byte cap must trigger a response without a delimiter.
        // A later newline write races the server's rejection and close.
        stream
            .write_all(&vec![b'x'; MAX_REQUEST_BYTES + 1])
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        let response: ControlResponse = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(error_code(&response), ControlErrorCode::FrameTooLarge);
    }

    fn live_shell_spawn(label: &str) -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                format!(
                    "printf '{label}\\n'; while IFS= read -r line; do printf 'ECHO:%s\\n' \"$line\"; done"
                ),
            ],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        }
    }

    fn live_fixture(label: &str) -> (ControlPlane, u64, u64) {
        let domain = Domain::bootstrap("live").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let mut plane = ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(pane.get(), live_shell_spawn(label))],
            None,
        )
        .unwrap();
        // The fixture child is `/bin/sh`; inject tests model an agent pane.
        plane.set_inject_agent_override(Some(crate::InjectAgent::Claude));
        (plane, window.get(), pane.get())
    }

    fn pane_content(response: ControlResponse) -> PaneContent {
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::PaneContent { content },
            } => content,
            other => panic!("expected pane content, got {other:?}"),
        }
    }

    fn wait_for_pane_text(
        stream: &mut UnixStream,
        request_id: &mut u64,
        client_id: u64,
        pane_id: u64,
        needle: &str,
    ) -> PaneContent {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let content = pane_content(request(
                stream,
                &ControlRequest::ReadPane {
                    version: PROTOCOL_VERSION,
                    request_id: *request_id,
                    client_id,
                    pane_id,
                },
            ));
            *request_id += 1;
            if content.lines.join("\n").contains(needle) {
                return content;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("pane {pane_id} never contained {needle:?}");
    }

    fn env_report_spawn() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                "printf 'PANE=%s\\nSOCK=%s\\n' \"$PRISMATTYC_PANE_ID\" \"$PMUX_SOCKET\"; if env | grep -q '^PRISMATTYC_SESSION_ID='; then printf 'HAS_SESSION=1\\n'; else printf 'HAS_SESSION=0\\n'; fi; exec sleep 30".into(),
            ],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        }
    }

    fn register_live_client(plane: &mut ControlPlane) -> u64 {
        match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register: {other:?}"),
        }
    }

    fn wait_live_text(plane: &mut ControlPlane, client: u64, pane: u64, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut request_id = 100;
        while Instant::now() < deadline {
            plane.drain_for_test();
            let content = pane_content(plane.handle(ControlRequest::ReadPane {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: client,
                pane_id: pane,
            }));
            request_id += 1;
            let text = content.lines.join("\n");
            if text.contains(needle) {
                return text;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("pane {pane} never contained {needle:?}");
    }

    #[test]
    fn new_live_rejects_relative_mux_socket() {
        let domain = Domain::bootstrap("rel").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let err = match ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(pane.get(), env_report_spawn())],
            Some(PathBuf::from("relative.sock")),
        ) {
            Err(error) => error,
            Ok(_) => panic!("relative socket must be rejected"),
        };
        assert_eq!(err.code, ControlErrorCode::InvalidRequest);
    }

    #[test]
    fn live_spawn_and_split_stamp_prism_guest_env() {
        let domain = Domain::bootstrap("guest").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let pane_id = pane.get();
        let socket = PathBuf::from("/tmp/prism-pm122-guest.sock");
        let mut plane = ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(pane_id, env_report_spawn())],
            Some(socket.clone()),
        )
        .unwrap();
        let client = register_live_client(&mut plane);
        // Wait for the last printf. Returning on PANE= races the HAS_SESSION line
        // under act (slower drain than a host cargo test).
        let text = wait_live_text(&mut plane, client, pane_id, "HAS_SESSION=0");
        assert!(text.contains(&format!("PANE={pane_id}")), "{text:?}");
        assert!(
            text.contains(&format!("SOCK={}", socket.display())),
            "{text:?}"
        );

        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window.get(),
            target_pane_id: pane_id,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: env_report_spawn(),
            client_id: None,
        });
        assert!(
            matches!(split.body, ControlResponseBody::Ok { .. }),
            "{split:?}"
        );
        let new_pane = plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .find(|candidate| candidate.id != pane_id)
            .unwrap()
            .id;
        assert_ne!(new_pane, pane_id);
        let split_text = wait_live_text(&mut plane, client, new_pane, "HAS_SESSION=0");
        assert!(
            split_text.contains(&format!("PANE={new_pane}")),
            "{split_text:?}"
        );
        assert!(
            split_text.contains(&format!("SOCK={}", socket.display())),
            "{split_text:?}"
        );
        assert!(
            !split_text.contains(&format!("PANE={pane_id}\n")),
            "split child must not keep the parent pane id: {split_text:?}"
        );
    }

    #[test]
    fn stale_checkpoint_capture_is_discarded_before_submission() {
        let (mut plane, window, pane) = live_fixture("STALE");
        let client = register_live_client(&mut plane);
        wait_live_text(&mut plane, client, pane, "STALE");
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: None,
        });
        assert!(
            matches!(split.body, ControlResponseBody::Ok { .. }),
            "{split:?}"
        );

        let path = socket_path().with_extension("json");
        plane.set_pane_log_path(path.clone()).unwrap();
        plane.maybe_persist_pane_logs();
        assert!(plane.pane_log_capture.is_some());

        let session_id = plane.snapshot().unwrap().sessions[0].id;
        let renamed = plane.handle(ControlRequest::NameSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            session_id,
            name: "renamed".into(),
        });
        assert!(
            matches!(renamed.body, ControlResponseBody::Ok { .. }),
            "{renamed:?}"
        );
        plane.maybe_persist_pane_logs();
        assert!(plane.pane_log_capture.is_none());
        assert!(!path.exists());

        plane.last_pane_log_persist = Some(Instant::now() - PERSIST_CADENCE);
        plane.maybe_persist_pane_logs();
        plane.maybe_persist_pane_logs();
        let deadline = Instant::now() + Duration::from_secs(2);
        while plane.pane_log_pending_mark.is_some() {
            plane.maybe_persist_pane_logs();
            assert!(
                Instant::now() < deadline,
                "replacement checkpoint did not complete"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let persisted = read_persist(&path).unwrap();
        assert!(persisted.panes.iter().all(|pane| pane.session == "renamed"));
        assert!(persisted.panes.iter().any(|pane| {
            pane.tail.iter().any(|frame| {
                matches!(&frame.event, PaneEvent::Output { bytes } if bytes.windows(5).any(|window| window == b"STALE"))
            })
        }));
        drop(plane);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn move_pane_across_sessions_keeps_pane_and_socket_env() {
        let domain = Domain::bootstrap("guest-move").unwrap();
        let session = domain.sessions().next().unwrap();
        let src_session = session.id.get();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let pane_id = pane.get();
        let socket = PathBuf::from("/tmp/prism-pm122-move.sock");
        let mut plane = ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(pane_id, env_report_spawn())],
            Some(socket.clone()),
        )
        .unwrap();
        let client = register_live_client(&mut plane);
        let before = wait_live_text(&mut plane, client, pane_id, "HAS_SESSION=0");
        assert!(before.contains(&format!("PANE={pane_id}")), "{before:?}");
        assert!(
            before.contains(&format!("SOCK={}", socket.display())),
            "{before:?}"
        );
        let pid = pane_content(plane.handle(ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id,
        }))
        .child_pid
        .expect("live pane has a child pid");

        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 4,
            name: "other".into(),
            spawn: live_shell_spawn("DST"),
            cols: Some(80),
            rows: Some(24),
            agent_id: None,
            headless: false,
        });
        let (dst_session, dst_window, dst_pane) = session_ids(&created);
        assert_ne!(dst_session, src_session);

        let moved = plane.handle(ControlRequest::MovePane {
            version: PROTOCOL_VERSION,
            request_id: 5,
            from_window_id: window.get(),
            to_window_id: dst_window,
            pane_id,
            target_pane_id: dst_pane,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            client_id: None,
        });
        assert!(
            matches!(moved.body, ControlResponseBody::Ok { .. }),
            "{moved:?}"
        );

        let after = pane_content(plane.handle(ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: client,
            pane_id,
        }));
        assert!(after.child_alive, "{after:?}");
        assert_eq!(after.child_pid, Some(pid));
        let after_text = after.lines.join("\n");
        assert!(
            after_text.contains(&format!("PANE={pane_id}")),
            "{after_text:?}"
        );
        assert!(
            after_text.contains(&format!("SOCK={}", socket.display())),
            "{after_text:?}"
        );
        assert!(after_text.contains("HAS_SESSION=0"), "{after_text:?}");

        let snapshot = plane.snapshot().unwrap();
        let dest = snapshot
            .sessions
            .iter()
            .find(|session| session.id == dst_session)
            .unwrap();
        assert!(dest
            .windows
            .iter()
            .any(|window| window.panes.iter().any(|candidate| candidate.id == pane_id)));
    }

    #[test]
    fn phase2b_live_server_client_disconnect_preserves_pty_and_emulator() {
        let (plane, _, pane) = live_fixture("READY");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();

        let mut first = UnixStream::connect(server.path()).unwrap();
        let first_client = registered_client(request(
            &mut first,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        assert!(matches!(
            request(
                &mut first,
                &ControlRequest::Snapshot {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        assert!(matches!(
            request(
                &mut first,
                &ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: first_client,
                    pane_id: pane,
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        let queued = request(
            &mut first,
            &ControlRequest::WritePane {
                version: PROTOCOL_VERSION,
                request_id: 4,
                client_id: first_client,
                pane_id: pane,
                data: "alpha\n".into(),
            },
        );
        assert!(matches!(
            queued.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { nbytes: 6, .. }
            }
        ));
        let mut request_id = 5;
        let before_detach = wait_for_pane_text(
            &mut first,
            &mut request_id,
            first_client,
            pane,
            "ECHO:alpha",
        );
        assert!(before_detach.child_alive);
        drop(first);
        thread::sleep(Duration::from_millis(30));

        let mut second = UnixStream::connect(server.path()).unwrap();
        let second_client = registered_client(request(
            &mut second,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        assert_ne!(first_client, second_client);
        let snapshot = request(
            &mut second,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 2,
            },
        );
        let controller = match snapshot.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => snapshot.sessions[0].windows[0].panes[0].controller_id,
            other => panic!("expected snapshot, got {other:?}"),
        };
        assert_eq!(
            controller, None,
            "disconnect must clear, not transfer, lease"
        );
        let after_detach = pane_content(request(
            &mut second,
            &ControlRequest::ReadPane {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: second_client,
                pane_id: pane,
            },
        ));
        assert!(after_detach.child_alive);
        assert!(after_detach.lines.join("\n").contains("ECHO:alpha"));
        assert!(after_detach.revision >= before_detach.revision);
    }

    #[test]
    fn phase2b_live_server_topology_mutations_keep_runtime_in_lockstep() {
        let (plane, window, pane) = live_fixture("FIRST");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        let _ = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 2,
            },
        );
        let _ = request(
            &mut stream,
            &ControlRequest::AcquireLease {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: client,
                pane_id: pane,
            },
        );
        let split = request(
            &mut stream,
            &ControlRequest::Split {
                version: PROTOCOL_VERSION,
                request_id: 4,
                window_id: window,
                target_pane_id: pane,
                axis: AxisWire::Horizontal,
                ratio: 0.5,
                spawn: live_shell_spawn("SECOND"),
                client_id: Some(client),
            },
        );
        assert!(matches!(split.body, ControlResponseBody::Ok { .. }));
        let snapshot = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id: 5,
            },
        );
        let new_pane = match snapshot.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => {
                snapshot.sessions[0].windows[0]
                    .panes
                    .iter()
                    .find(|candidate| candidate.id != pane)
                    .unwrap()
                    .id
            }
            other => panic!("expected snapshot, got {other:?}"),
        };
        let mut request_id = 6;
        let content = wait_for_pane_text(&mut stream, &mut request_id, client, new_pane, "SECOND");
        assert_eq!((content.cols, content.rows), (40, 24));

        let resize = request(
            &mut stream,
            &ControlRequest::Resize {
                version: PROTOCOL_VERSION,
                request_id,
                window_id: window,
                cols: 100,
                rows: 30,
                cell_width_px: None,
                cell_height_px: None,
                fit: false,
                host: false,
                client_id: None,
            },
        );
        request_id += 1;
        assert!(matches!(resize.body, ControlResponseBody::Ok { .. }));
        let resized = pane_content(request(
            &mut stream,
            &ControlRequest::ReadPane {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: client,
                pane_id: new_pane,
            },
        ));
        request_id += 1;
        assert_eq!((resized.cols, resized.rows), (50, 30));

        let close = request(
            &mut stream,
            &ControlRequest::Close {
                version: PROTOCOL_VERSION,
                request_id,
                window_id: window,
                pane_id: new_pane,
                prior_focus_id: pane,
                client_id: None,
            },
        );
        request_id += 1;
        assert!(matches!(close.body, ControlResponseBody::Ok { .. }));
        let stale = request(
            &mut stream,
            &ControlRequest::ReadPane {
                version: PROTOCOL_VERSION,
                request_id,
                client_id: client,
                pane_id: new_pane,
            },
        );
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
    }

    #[test]
    fn phase2b_idle_attach_survives_bounded_read_timeouts() {
        let (plane, _, pane) = live_fixture("IDLE");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        thread::sleep(IO_TIMEOUT + Duration::from_millis(100));
        let acquire = request(
            &mut stream,
            &ControlRequest::AcquireLease {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: client,
                pane_id: pane,
            },
        );
        assert!(matches!(acquire.body, ControlResponseBody::Ok { .. }));
    }

    fn event_batch(response: ControlResponse) -> EventBatch {
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Events { batch },
            } => batch,
            other => panic!("expected events, got {other:?}"),
        }
    }

    fn wait_for_output_activity(
        stream: &mut UnixStream,
        request_id: &mut u64,
        pane_id: u64,
    ) -> (u64, u64, bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let batch = event_batch(request(
                stream,
                &ControlRequest::Events {
                    version: PROTOCOL_VERSION,
                    request_id: *request_id,
                    after_sequence: 0,
                    limit: None,
                },
            ));
            *request_id += 1;
            let hits: Vec<_> = batch
                .events
                .iter()
                .filter_map(|envelope| match envelope.event {
                    Event::OutputActivity {
                        pane_id: id,
                        revision,
                        child_alive,
                    } if id == pane_id => Some((envelope.sequence, revision, child_alive)),
                    _ => None,
                })
                .collect();
            if let Some(&hit) = hits.last() {
                assert_eq!(hits.len(), 1, "activity must coalesce per pane: {hits:?}");
                return hit;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("pane {pane_id} never published OutputActivity");
    }

    #[test]
    fn phase2b_output_activity_is_emitted_and_coalesced() {
        let (plane, window, pane) = live_fixture("ACT");
        let path = socket_path();
        let server = ControlServer::bind(&path, plane).unwrap();
        let mut stream = UnixStream::connect(server.path()).unwrap();
        let client = registered_client(request(
            &mut stream,
            &ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            },
        ));
        assert!(matches!(
            request(
                &mut stream,
                &ControlRequest::Snapshot {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        assert!(matches!(
            request(
                &mut stream,
                &ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: client,
                    pane_id: pane,
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        assert!(matches!(
            request(
                &mut stream,
                &ControlRequest::WritePane {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: client,
                    pane_id: pane,
                    data: "one\n".into(),
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        let mut request_id = 5;
        let _ = wait_for_pane_text(&mut stream, &mut request_id, client, pane, "ECHO:one");
        let (seq1, rev1, alive1) = wait_for_output_activity(&mut stream, &mut request_id, pane);
        assert!(alive1);
        assert!(rev1 > 0);

        assert!(matches!(
            request(
                &mut stream,
                &ControlRequest::WritePane {
                    version: PROTOCOL_VERSION,
                    request_id,
                    client_id: client,
                    pane_id: pane,
                    data: "two\n".into(),
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        request_id += 1;
        let _ = wait_for_pane_text(&mut stream, &mut request_id, client, pane, "ECHO:two");
        let (seq2, rev2, _) = wait_for_output_activity(&mut stream, &mut request_id, pane);
        assert_eq!(seq2, seq1, "second burst must update the same ring entry");
        assert!(rev2 >= rev1);

        assert!(matches!(
            request(
                &mut stream,
                &ControlRequest::Split {
                    version: PROTOCOL_VERSION,
                    request_id,
                    window_id: window,
                    target_pane_id: pane,
                    axis: AxisWire::Horizontal,
                    ratio: 0.5,
                    spawn: live_shell_spawn("SECOND"),
                    client_id: Some(client),
                },
            )
            .body,
            ControlResponseBody::Ok { .. }
        ));
        request_id += 1;
        let snapshot = request(
            &mut stream,
            &ControlRequest::Snapshot {
                version: PROTOCOL_VERSION,
                request_id,
            },
        );
        request_id += 1;
        let new_pane = match snapshot.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => {
                snapshot.sessions[0].windows[0]
                    .panes
                    .iter()
                    .find(|candidate| candidate.id != pane)
                    .unwrap()
                    .id
            }
            other => panic!("expected snapshot, got {other:?}"),
        };
        let _ = wait_for_pane_text(&mut stream, &mut request_id, client, new_pane, "SECOND");
        let (seq_new, _, alive_new) =
            wait_for_output_activity(&mut stream, &mut request_id, new_pane);
        assert!(alive_new);
        assert!(seq_new > seq1);
    }

    fn window_ids(response: &ControlResponse) -> (u64, u64, u64) {
        match &response.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::Window {
                        session_id,
                        window_id,
                        pane_id,
                        ..
                    },
            } => (*session_id, *window_id, *pane_id),
            other => panic!("expected window response, got {other:?}"),
        }
    }

    #[test]
    fn create_window_raw_json_round_trip_and_stale_ids_are_unsequenced() {
        let (mut plane, _, _) = fixture(8);
        let session = plane.snapshot().unwrap().sessions[0].id;
        let encoded = serde_json::to_string(&ControlRequest::CreateWindow {
            version: PROTOCOL_VERSION,
            request_id: 1,
            session_id: session,
            title: "work".into(),
            spawn: spawn(),
            cols: Some(80),
            rows: Some(24),
        })
        .unwrap();
        assert!(encoded.contains("\"type\":\"create_window\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let created = plane.handle(decoded);
        let (_, window, pane) = window_ids(&created);
        assert_eq!(plane.snapshot().unwrap().sessions[0].windows.len(), 2);
        assert_ne!(
            pane,
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0].id
        );
        assert!(plane.snapshot().unwrap().sessions[0]
            .windows
            .iter()
            .any(|w| w.id == window));

        let stale = plane.handle(ControlRequest::DestroyWindow {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: 99,
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 1);

        let stale_session = plane.handle(ControlRequest::CreateWindow {
            version: PROTOCOL_VERSION,
            request_id: 3,
            session_id: 99,
            title: "ghost".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
        });
        assert_eq!(error_code(&stale_session), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 1);
    }

    #[test]
    fn rename_window_raw_json_round_trip_and_stale_ids_are_unsequenced() {
        let (mut plane, window, _) = fixture(8);
        let encoded = serde_json::to_string(&ControlRequest::RenameWindow {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            title: "  work  ".into(),
        })
        .unwrap();
        assert!(encoded.contains("\"type\":\"rename_window\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let renamed = plane.handle(decoded);
        match renamed.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::Window {
                        window_id, title, ..
                    },
            } => {
                assert_eq!(window_id, window);
                assert_eq!(title, "work");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].title,
            "work"
        );
        assert_eq!(plane.sequence(), 1);

        let stale = plane.handle(ControlRequest::RenameWindow {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: 99,
            title: "ghost".into(),
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 1);

        let empty = plane.handle(ControlRequest::RenameWindow {
            version: PROTOCOL_VERSION,
            request_id: 3,
            window_id: window,
            title: "   ".into(),
        });
        assert_eq!(error_code(&empty), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 1);

        let huge = plane.handle(ControlRequest::RenameWindow {
            version: PROTOCOL_VERSION,
            request_id: 4,
            window_id: window,
            title: "x".repeat(65),
        });
        assert_eq!(error_code(&huge), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 1);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].title,
            "work"
        );
    }

    #[test]
    fn rename_pane_sets_clears_and_rejects_bad_titles() {
        let (mut plane, _, pane) = fixture(8);
        let before = plane.sequence();
        let renamed = plane.handle(ControlRequest::RenamePane {
            version: PROTOCOL_VERSION,
            request_id: 1,
            pane_id: pane,
            title: "  build  ".into(),
        });
        assert!(
            matches!(renamed.body, ControlResponseBody::Ok { .. }),
            "{renamed:?}"
        );
        assert_eq!(
            plane.domain.pane(pane_id(pane).unwrap()).unwrap().title,
            "build"
        );
        assert!(
            plane
                .domain
                .pane(pane_id(pane).unwrap())
                .unwrap()
                .title_pinned,
            "rename pins OSC 0/2 (PT-230)"
        );
        assert!(plane.sequence() > before, "rename is sequenced");
        let snapshot = plane.handle(ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 2,
        });
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(text.contains("\"title\":\"build\""), "{text}");
        assert!(text.contains("\"title_pinned\":true"), "{text}");

        let cleared = plane.handle(ControlRequest::RenamePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            pane_id: pane,
            title: String::new(),
        });
        assert!(matches!(cleared.body, ControlResponseBody::Ok { .. }));
        assert_eq!(plane.domain.pane(pane_id(pane).unwrap()).unwrap().title, "");
        assert!(
            !plane
                .domain
                .pane(pane_id(pane).unwrap())
                .unwrap()
                .title_pinned,
            "empty rename unpins"
        );

        let mid = plane.sequence();
        let control = plane.handle(ControlRequest::RenamePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            pane_id: pane,
            title: "bad\u{7}".into(),
        });
        assert_eq!(error_code(&control), ControlErrorCode::InvalidRequest);
        let long = plane.handle(ControlRequest::RenamePane {
            version: PROTOCOL_VERSION,
            request_id: 5,
            pane_id: pane,
            title: "x".repeat(65),
        });
        assert_eq!(error_code(&long), ControlErrorCode::InvalidRequest);
        let stale = plane.handle(ControlRequest::RenamePane {
            version: PROTOCOL_VERSION,
            request_id: 6,
            pane_id: 999_999,
            title: "ghost".into(),
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), mid, "rejections are unsequenced");
    }

    #[test]
    fn set_pane_status_round_trip_and_rejections_are_unsequenced() {
        let (mut plane, _, pane) = fixture(8);
        let encoded = serde_json::to_string(&ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 1,
            pane_id: pane,
            text: Some("  build ok  ".into()),
        })
        .unwrap();
        assert!(encoded.contains("\"type\":\"set_pane_status\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let set = plane.handle(decoded);
        assert!(matches!(
            set.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Mutation { .. }
            }
        ));
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
                .status
                .as_deref(),
            Some("build ok")
        );
        assert_eq!(plane.sequence(), 1);

        let cleared = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 2,
            pane_id: pane,
            text: None,
        });
        assert!(matches!(cleared.body, ControlResponseBody::Ok { .. }));
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0].status,
            None
        );
        assert_eq!(plane.sequence(), 2);

        let _ = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 3,
            pane_id: pane,
            text: Some("keep".into()),
        });
        assert_eq!(plane.sequence(), 3);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
                .status
                .as_deref(),
            Some("keep")
        );

        let stale = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 4,
            pane_id: 99,
            text: Some("ghost".into()),
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 3);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
                .status
                .as_deref(),
            Some("keep")
        );

        let huge = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 5,
            pane_id: pane,
            text: Some("x".repeat(65)),
        });
        assert_eq!(error_code(&huge), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 3);

        let esc = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 6,
            pane_id: pane,
            text: Some("a\u{1b}[31mb".into()),
        });
        assert_eq!(error_code(&esc), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 3);

        let tab = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 7,
            pane_id: pane,
            text: Some("a\tb".into()),
        });
        assert_eq!(error_code(&tab), ControlErrorCode::InvalidRequest);
        assert_eq!(plane.sequence(), 3);
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
                .status
                .as_deref(),
            Some("keep")
        );

        let empty = plane.handle(ControlRequest::SetPaneStatus {
            version: PROTOCOL_VERSION,
            request_id: 8,
            pane_id: pane,
            text: Some("   ".into()),
        });
        assert!(matches!(empty.body, ControlResponseBody::Ok { .. }));
        assert_eq!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0].status,
            None
        );
    }

    #[test]
    fn set_sync_input_round_trip_and_stale_window_is_unsequenced() {
        let (mut plane, window, _) = fixture(8);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let encoded = serde_json::to_string(&ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            window_id: window,
            enabled: true,
        })
        .unwrap();
        assert!(encoded.contains("\"type\":\"set_sync_input\""));
        let decoded: ControlRequest = serde_json::from_str(&encoded).unwrap();
        let set = plane.handle(decoded);
        assert!(matches!(
            set.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Mutation { .. }
            }
        ));
        assert!(plane.snapshot().unwrap().sessions[0].windows[0].sync_input);
        assert_eq!(plane.sequence(), 1);

        let off = plane.handle(ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            window_id: window,
            enabled: false,
        });
        assert!(matches!(off.body, ControlResponseBody::Ok { .. }));
        assert!(!plane.snapshot().unwrap().sessions[0].windows[0].sync_input);
        assert_eq!(plane.sequence(), 2);

        let stale = plane.handle(ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            window_id: 99,
            enabled: true,
        });
        assert_eq!(error_code(&stale), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 2);
    }

    fn wait_live_echo(plane: &mut ControlPlane, pane: u64, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains(needle))
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("pane {pane} never contained {needle:?}");
    }

    #[test]
    fn write_pane_sync_fans_out_to_siblings_and_off_stays_focused() {
        let (mut plane, window, pane) = live_fixture("A");
        let client = register_live_client(&mut plane);
        wait_live_echo(&mut plane, pane, "A");
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 10,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Vertical,
            ratio: 0.5,
            spawn: live_shell_spawn("B"),
            client_id: None,
        });
        assert!(matches!(split.body, ControlResponseBody::Ok { .. }));
        let sibling = plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .map(|p| p.id)
            .find(|id| *id != pane)
            .expect("sibling pane");
        wait_live_echo(&mut plane, sibling, "B");

        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 11,
            client_id: client,
            pane_id: pane,
        });
        let _ = plane.handle(ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 12,
            client_id: client,
            window_id: window,
            enabled: true,
        });
        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 13,
            client_id: client,
            pane_id: pane,
            data: "hi\n".into(),
        });
        assert!(matches!(
            wrote.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        wait_live_echo(&mut plane, pane, "ECHO:hi");
        wait_live_echo(&mut plane, sibling, "ECHO:hi");

        let _ = plane.handle(ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 14,
            client_id: client,
            window_id: window,
            enabled: false,
        });
        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 15,
            client_id: client,
            pane_id: pane,
            data: "one\n".into(),
        });
        assert!(matches!(wrote.body, ControlResponseBody::Ok { .. }));
        wait_live_echo(&mut plane, pane, "ECHO:one");
        plane.drain_for_test();
        let sibling_text = plane
            .live
            .as_ref()
            .and_then(|live| live.content(sibling))
            .map(|content| content.lines.join("\n"))
            .unwrap_or_default();
        assert!(
            !sibling_text.contains("ECHO:one"),
            "sync off must not fan out: {sibling_text}"
        );
    }

    #[test]
    fn team_attention_coalesces_and_rejects_stale_ownership_and_revisions() {
        use crate::team_attention::AttentionAction;
        let (mut plane, _window, pane) = live_fixture("A");
        let client = register_live_client(&mut plane);
        let session = plane.domain.sessions().next().unwrap().id;
        let owner = "a".repeat(32);
        plane
            .domain
            .transfer_space_sessions(&[session], None, Some(&owner))
            .unwrap();
        plane
            .raise_attention(client, pane, "Choose a branch".into())
            .unwrap();
        let first = plane.team_attention[&pane].clone();
        let seq = plane.sequence();
        plane
            .raise_attention(client, pane, "Choose a branch".into())
            .unwrap();
        assert_eq!(plane.team_attention[&pane], first);
        assert_eq!(plane.sequence(), seq);
        assert!(plane
            .update_team_attention(
                client,
                pane,
                first.revision,
                "b".repeat(32),
                session.get(),
                AttentionAction::Resolve
            )
            .is_err());
        assert!(plane
            .update_team_attention(
                client,
                pane,
                first.revision,
                owner.clone(),
                session.get(),
                AttentionAction::Snooze { seconds: 0 }
            )
            .is_err());
        plane
            .update_team_attention(
                client,
                pane,
                first.revision,
                owner.clone(),
                session.get(),
                AttentionAction::Snooze { seconds: 10 },
            )
            .unwrap();
        assert!(!plane.team_attention[&pane].needs_input(now_unix_ms()));
        plane
            .raise_attention(client, pane, "Review changes".into())
            .unwrap();
        assert!(plane
            .update_team_attention(
                client,
                pane,
                first.revision,
                owner.clone(),
                session.get(),
                AttentionAction::Resolve
            )
            .is_err());
        let revision = plane.team_attention[&pane].revision;
        plane
            .attention
            .insert(pane, "mail replaced this hint".into());
        plane
            .update_team_attention(
                client,
                pane,
                revision,
                owner,
                session.get(),
                AttentionAction::Resolve,
            )
            .unwrap();
        assert!(plane.team_attention.is_empty());
        assert_eq!(plane.attention[&pane], "mail replaced this hint");
        let (mut restarted, _, other_pane) = live_fixture("A");
        let other_client = register_live_client(&mut restarted);
        restarted
            .raise_attention(other_client, other_pane, "Choose a branch".into())
            .unwrap();
        assert_ne!(
            restarted.team_attention[&other_pane].revision,
            first.revision
        );
        assert!(restarted.team_attention[&other_pane].revision < (1u64 << 53));
    }

    #[test]
    fn write_pane_clears_generic_attention_and_emits_event() {
        let (mut plane, _window, pane) = live_fixture("A");
        let client = register_live_client(&mut plane);
        wait_live_echo(&mut plane, pane, "A");
        let lease = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 10,
            client_id: client,
            pane_id: pane,
        });
        assert!(matches!(lease.body, ControlResponseBody::Ok { .. }));

        let raised = plane.handle(ControlRequest::RaiseAttention {
            version: PROTOCOL_VERSION,
            request_id: 11,
            client_id: client,
            pane_id: pane,
            message: "needs input".into(),
        });
        assert!(matches!(raised.body, ControlResponseBody::Ok { .. }));
        let raised_sequence = plane.sequence();
        assert_eq!(
            plane.attention.get(&pane).map(String::as_str),
            Some("needs input")
        );

        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 12,
            client_id: client,
            pane_id: pane,
            data: "y".into(),
        });
        assert!(matches!(
            wrote.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        assert!(!plane.attention.contains_key(&pane));
        assert!(!plane.team_attention.contains_key(&pane));
        let events = plane.events_after(raised_sequence, None).unwrap().events;
        assert!(events.iter().any(|envelope| matches!(
            envelope.event,
            Event::PaneAttentionCleared { pane_id } if pane_id == pane
        )));
    }

    #[test]
    fn write_pane_sync_rejects_observer_and_swallows_dead_sibling() {
        let (mut plane, window, pane) = live_fixture("A");
        let controller = register_live_client(&mut plane);
        wait_live_echo(&mut plane, pane, "A");
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 10,
            window_id: window,
            target_pane_id: pane,
            axis: AxisWire::Vertical,
            ratio: 0.5,
            spawn: live_shell_spawn("B"),
            client_id: None,
        });
        assert!(matches!(split.body, ControlResponseBody::Ok { .. }));
        let sibling = plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .map(|p| p.id)
            .find(|id| *id != pane)
            .expect("sibling pane");
        wait_live_echo(&mut plane, sibling, "B");
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 11,
            client_id: controller,
            pane_id: pane,
        });
        let _ = plane.handle(ControlRequest::SetSyncInput {
            version: PROTOCOL_VERSION,
            request_id: 12,
            client_id: controller,
            window_id: window,
            enabled: true,
        });

        let observer = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 13,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register observer: {other:?}"),
        };
        let observer_write = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 14,
            client_id: observer,
            pane_id: pane,
            data: "secret\n".into(),
        });
        assert_eq!(error_code(&observer_write), ControlErrorCode::NotController);

        if let Some(live) = plane.live.as_mut() {
            live.remove_pane(sibling);
        }
        let wrote = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 15,
            client_id: controller,
            pane_id: pane,
            data: "still\n".into(),
        });
        assert!(
            matches!(
                wrote.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::WriteQueued { .. }
                }
            ),
            "dead sibling must not fail the focused write: {wrote:?}"
        );
        wait_live_echo(&mut plane, pane, "ECHO:still");
    }

    #[test]
    fn move_pane_emits_one_event_and_rejects_same_window_unsequenced() {
        let (mut plane, src, first) = fixture(8);
        let session = plane.snapshot().unwrap().sessions[0].id;
        let created = plane.handle(ControlRequest::CreateWindow {
            version: PROTOCOL_VERSION,
            request_id: 1,
            session_id: session,
            title: "dst".into(),
            spawn: spawn(),
            cols: Some(80),
            rows: Some(24),
        });
        let (_, dst, target) = window_ids(&created);
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: src,
            target_pane_id: first,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: None,
        });
        assert_eq!(mutation_sequence(&split), 2);
        let moved = plane.snapshot().unwrap().sessions[0].windows[0].panes[1].id;

        let before = plane.sequence();
        let same = plane.handle(ControlRequest::MovePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            from_window_id: src,
            to_window_id: src,
            pane_id: moved,
            target_pane_id: first,
            axis: AxisWire::Vertical,
            ratio: 0.5,
            client_id: None,
        });
        assert_eq!(error_code(&same), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), before);

        let moved_resp = plane.handle(ControlRequest::MovePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            from_window_id: src,
            to_window_id: dst,
            pane_id: moved,
            target_pane_id: target,
            axis: AxisWire::Vertical,
            ratio: 0.5,
            client_id: None,
        });
        assert_eq!(mutation_sequence(&moved_resp), before + 1);
        let batch = plane.events_after(before, None).unwrap();
        assert_eq!(batch.events.len(), 1);
        match &batch.events[0].event {
            Event::PaneMoved {
                from_window_id,
                to_window_id,
                pane_id,
                geometry,
                ..
            } => {
                assert_eq!(*from_window_id, src);
                assert_eq!(*to_window_id, dst);
                assert_eq!(*pane_id, moved);
                assert!(geometry.iter().any(|pane| pane.pane_id == moved));
                assert!(geometry.iter().any(|pane| pane.pane_id == target));
            }
            other => panic!("expected single PaneMoved, got {other:?}"),
        }
        assert!(!matches!(
            batch.events[0].event,
            Event::PaneClosed { .. } | Event::PaneSplit { .. }
        ));
    }

    #[test]
    fn switch_window_is_client_local_and_destroy_collapses_session() {
        let (mut plane, first_window, _) = fixture(8);
        let session = plane.snapshot().unwrap().sessions[0].id;
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let created = plane.handle(ControlRequest::CreateWindow {
            version: PROTOCOL_VERSION,
            request_id: 2,
            session_id: session,
            title: "two".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
        });
        let (_, second, pane) = window_ids(&created);
        let switched = plane.handle(ControlRequest::SwitchWindow {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            window_id: second,
        });
        assert_eq!(window_ids(&switched).1, second);
        assert_eq!(window_ids(&switched).2, pane);
        assert_eq!(plane.snapshot().unwrap().sessions[0].windows.len(), 2);

        let _ = plane.handle(ControlRequest::DestroyWindow {
            version: PROTOCOL_VERSION,
            request_id: 4,
            window_id: first_window,
        });
        assert_eq!(plane.snapshot().unwrap().sessions[0].windows.len(), 1);
        let last = plane.handle(ControlRequest::DestroyWindow {
            version: PROTOCOL_VERSION,
            request_id: 5,
            window_id: second,
        });
        assert!(matches!(
            last.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::Mutation { .. }
            }
        ));
        assert!(plane.snapshot().unwrap().sessions.is_empty());
    }

    #[test]
    fn phase2b_live_runtime_preserves_pty_across_move_pane() {
        let (mut plane, src, pane) = live_fixture("MOVE");
        let session = plane.snapshot().unwrap().sessions[0].id;
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let created = plane.handle(ControlRequest::CreateWindow {
            version: PROTOCOL_VERSION,
            request_id: 2,
            session_id: session,
            title: "dst".into(),
            spawn: live_shell_spawn("DST"),
            cols: Some(80),
            rows: Some(24),
        });
        let (_, dst, target) = window_ids(&created);
        let before = pane_content(plane.handle(ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
        }));
        assert!(before.child_alive);
        let pid = before.child_pid.expect("live pane has a child pid");

        let moved = plane.handle(ControlRequest::MovePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            from_window_id: src,
            to_window_id: dst,
            pane_id: pane,
            target_pane_id: target,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            client_id: None,
        });
        assert!(
            matches!(
                moved.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::Mutation { .. }
                }
            ),
            "{moved:?}"
        );
        let after = pane_content(plane.handle(ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: client,
            pane_id: pane,
        }));
        assert!(after.child_alive, "{after:?}");
        assert_eq!(after.child_pid, Some(pid));
        let snapshot = plane.snapshot().unwrap();
        let dest = snapshot.sessions[0]
            .windows
            .iter()
            .find(|window| window.id == dst)
            .unwrap();
        assert!(dest.panes.iter().any(|candidate| candidate.id == pane));
        assert!(snapshot.sessions[0].windows.iter().all(|window| {
            window.id != src || window.panes.iter().all(|candidate| candidate.id != pane)
        }));
    }

    #[test]
    fn rle_style_runs_merge_same_style_and_split_on_color() {
        use prismattyc_core::{Color, Screen, Style};
        let red = Style {
            foreground: Color::Ansi(1),
            ..Style::default()
        };
        let mut screen = Screen::new(3, 1, 0);
        screen.put_char('A');
        screen.put_char('B');
        screen.set_style(red);
        screen.put_char('C');
        let cells: Vec<_> = screen.view_row(0).unwrap().collect();
        let runs = rle_style_runs(cells);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "AB");
        assert_eq!(runs[0].fg, ColorWire::Default);
        assert_eq!(runs[1].text, "C");
        assert_eq!(runs[1].fg, ColorWire::Ansi { n: 1 });
    }

    #[test]
    fn read_pane_styled_carries_sgr_and_read_pane_stays_text() {
        let (mut plane, _, pane) = live_fixture("READY");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_ready = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            let text = pane_content(plane.handle(ControlRequest::ReadPane {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: client,
                pane_id: pane,
            }));
            if text.lines.join("\n").contains("READY") {
                saw_ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_ready, "fixture never printed READY");
        let _ = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            data: "\u{1b}[31mRED\u{1b}[0m\n".into(),
        });
        let mut styled = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            plane.drain_for_test();
            let response = plane.handle(ControlRequest::ReadPaneStyled {
                version: PROTOCOL_VERSION,
                request_id: 5,
                client_id: client,
                pane_id: pane,
                view_offset: None,
            });
            if let ControlResponseBody::Ok {
                response: ControlResponseData::PaneStyled { content },
            } = response.body
            {
                if content
                    .runs
                    .iter()
                    .flatten()
                    .any(|run| run.text.contains("RED") && run.fg == ColorWire::Ansi { n: 1 })
                {
                    styled = Some(content);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        let styled = styled.expect("styled projection never saw red RED");
        let text = pane_content(plane.handle(ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: client,
            pane_id: pane,
        }));
        assert!(text.lines.join("\n").contains("RED"));
        let dumped = serde_json::to_string(&text).unwrap();
        assert!(
            !dumped.contains("\"runs\""),
            "ReadPane must stay text-only: {dumped}"
        );
        assert_eq!(styled.content.lines.len(), text.lines.len());
    }

    #[test]
    fn rich_focus_toggle_flag_off_is_noop_ok() {
        let (mut plane, _, pane) = live_fixture("READY");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        match plane
            .handle(ControlRequest::RichFocusToggle {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: client,
                pane_id: pane,
                revoke: false,
            })
            .body
        {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::RichFocus {
                        granted: false,
                        region_id: None,
                        ..
                    },
            } => {}
            other => panic!("flag-off toggle must be no-op Ok, got {other:?}"),
        }
        let styled = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            view_offset: None,
        }));
        assert!(!styled.experimental_rich);
        assert!(styled.rich_focus_id.is_none());
        let json = serde_json::to_string(&styled).unwrap();
        assert!(
            !json.contains("experimental_rich"),
            "flag-off JSON must omit experimental_rich: {json}"
        );
    }

    #[test]
    fn rich_input_wire_is_intent_only_and_round_trips() {
        let request = ControlRequest::RichInput {
            version: PROTOCOL_VERSION,
            request_id: 9,
            client_id: 3,
            pane_id: 7,
            input: RichInputKind::Pointer {
                phase: RichPointerPhase::Release,
                row: 2,
                col: 4,
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"rich_input\""));
        assert!(json.contains("\"kind\":\"pointer\""));
        assert!(json.contains("\"phase\":\"release\""));
        assert!(!json.contains("viewer"));
        assert!(!json.contains("action"));
        assert_eq!(
            serde_json::from_str::<ControlRequest>(&json).unwrap(),
            request
        );
    }

    #[test]
    fn rich_input_rejects_observer_stale_client_and_wrong_pane_before_delivery() {
        let (mut plane, _, pane) = fixture(8);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let observer = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: holder,
            pane_id: pane,
        });

        let key = |request_id, client_id, pane_id| ControlRequest::RichInput {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
            input: RichInputKind::Key {
                key: "Enter".into(),
                modifiers: 0,
            },
        };
        assert_eq!(
            error_code(&plane.handle(key(4, observer, pane))),
            ControlErrorCode::NotController
        );
        assert_eq!(
            error_code(&plane.handle(key(5, u64::MAX, pane))),
            ControlErrorCode::StaleId
        );
        assert_eq!(
            error_code(&plane.handle(key(6, holder, u64::MAX))),
            ControlErrorCode::StaleId
        );
        assert_eq!(
            error_code(&plane.handle(key(7, holder, pane))),
            ControlErrorCode::InputRouteUnavailable
        );
    }

    #[test]
    fn rich_input_focus_is_viewer_local_and_controller_loss_revokes() {
        use prismattyc_protocol::{
            encode_capability_query, encode_workspace_snapshot, CapabilityQuery, ProtocolVersion,
            RequestId, TreeNode, TreeNodeKind, WorkspaceRows, WorkspaceSnapshot,
        };

        let (mut plane, _, pane) = live_fixture("READY");
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        let workspace = encode_workspace_snapshot(&WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: WorkspaceRows {
                min: 5,
                preferred: 8,
                max: 12,
            },
            nodes: vec![TreeNode {
                id: 1,
                parent: 0,
                kind: TreeNodeKind::Text,
                min: 5,
                preferred: 8,
                fill: 1,
                show_min_cols: 0,
                show_max_cols: 0,
                action_id: 7,
                text: "Runbook".into(),
            }],
        })
        .unwrap();
        let live = plane.live.as_mut().unwrap();
        live.feed_rich_for_test(pane, &query).unwrap();
        live.feed_rich_for_test(pane, &workspace).unwrap();

        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let observer = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: holder,
            pane_id: pane,
        });
        match plane
            .handle(ControlRequest::RichFocusToggle {
                version: PROTOCOL_VERSION,
                request_id: 4,
                client_id: holder,
                pane_id: pane,
                revoke: false,
            })
            .body
        {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::RichFocus {
                        granted: true,
                        region_id: Some(1),
                        structured: true,
                        ..
                    },
            } => {}
            other => panic!("expected structured focus grant, got {other:?}"),
        }

        let holder_view = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: holder,
            pane_id: pane,
            view_offset: None,
        }));
        let observer_view = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: observer,
            pane_id: pane,
            view_offset: None,
        }));
        assert!(holder_view.structured_focus);
        assert_eq!(holder_view.rich_focus_id, Some(1));
        assert!(!observer_view.structured_focus);
        assert!(observer_view.rich_focus_id.is_none());

        assert!(matches!(
            plane
                .handle(ControlRequest::RichInput {
                    version: PROTOCOL_VERSION,
                    request_id: 7,
                    client_id: holder,
                    pane_id: pane,
                    input: RichInputKind::Key {
                        key: "Enter".into(),
                        modifiers: 0,
                    },
                })
                .body,
            ControlResponseBody::Ok {
                response: ControlResponseData::RichInput {
                    delivered: true,
                    ..
                }
            }
        ));
        let _ = plane.handle(ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id: 8,
            client_id: holder,
            pane_id: pane,
        });
        let revoked = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 9,
            client_id: holder,
            pane_id: pane,
            view_offset: None,
        }));
        assert!(!revoked.structured_focus);
        assert!(revoked.rich_focus_id.is_none());
    }

    fn diag_copy_bytes() -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        use prismattyc_protocol::{
            encode_capability_query, encode_semantic_copy, encode_semantic_snapshot,
            encode_workspace_snapshot, CapabilityQuery, ProtocolVersion, RequestId, SemanticCopy,
            SemanticDocument, SemanticRange, SemanticRole, SemanticSpan, TreeNode, TreeNodeKind,
            WorkspaceRows, WorkspaceSnapshot,
        };

        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        let workspace = encode_workspace_snapshot(&WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: WorkspaceRows {
                min: 5,
                preferred: 8,
                max: 12,
            },
            nodes: vec![TreeNode {
                id: 1,
                parent: 0,
                kind: TreeNodeKind::Text,
                min: 5,
                preferred: 8,
                fill: 1,
                show_min_cols: 0,
                show_max_cols: 0,
                action_id: 7,
                text: "Runbook".into(),
            }],
        })
        .unwrap();
        let snapshot = encode_semantic_snapshot(&SemanticDocument {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 2,
            text: "error crates/foo.rs:10:1".into(),
            spans: vec![SemanticSpan {
                start: 6,
                end: 24,
                role: SemanticRole::Location,
            }],
            selection: Some(SemanticRange {
                rev: 2,
                start: 6,
                end: 24,
            }),
        })
        .unwrap();
        let copy = encode_semantic_copy(&SemanticCopy {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 2,
            start: 6,
            end: 24,
        })
        .unwrap();
        (query, workspace, snapshot, copy)
    }

    fn feed_diag_surface(plane: &mut ControlPlane, pane: u64) -> Vec<u8> {
        let (query, workspace, snapshot, copy) = diag_copy_bytes();
        let live = plane.live.as_mut().unwrap();
        live.feed_rich_for_test(pane, &query).unwrap();
        live.feed_rich_for_test(pane, &workspace).unwrap();
        live.feed_rich_for_test(pane, &snapshot).unwrap();
        copy
    }

    fn write_pane(plane: &mut ControlPlane, request_id: u64, client: u64, pane: u64, data: &str) {
        let _ = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client,
            pane_id: pane,
            data: data.into(),
        });
    }

    fn styled_for(plane: &mut ControlPlane, request_id: u64, client: u64, pane: u64) -> PaneStyled {
        styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client,
            pane_id: pane,
            view_offset: None,
        }))
    }

    fn ack_copy(
        plane: &mut ControlPlane,
        request_id: u64,
        client: u64,
        pane: u64,
        seq: Option<u64>,
    ) -> (Option<String>, Option<u64>) {
        match plane.handle(ControlRequest::CopySemantic {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client,
            pane_id: pane,
            seq,
        }) {
            ControlResponse {
                body:
                    ControlResponseBody::Ok {
                        response: ControlResponseData::SemanticCopy { text, seq, .. },
                    },
                ..
            } => (text, seq),
            other => panic!("expected SemanticCopy, got {other:?}"),
        }
    }

    #[test]
    fn observer_styled_read_does_not_steal_controller_clipboard() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let observer = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let successor = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 3,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: holder,
            pane_id: pane,
        });
        write_pane(&mut plane, 5, holder, pane, "y");
        let _ = plane.handle(ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: holder,
            pane_id: pane,
        });
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 7,
            client_id: successor,
            pane_id: pane,
        });
        write_pane(&mut plane, 8, successor, pane, "x");
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let successor_view = styled_for(&mut plane, 9, successor, pane);
        assert!(
            successor_view.semantic_clipboard.is_none(),
            "successor WritePane must not steal the initiating y request"
        );
        let _ = plane.handle(ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id: 10,
            client_id: successor,
            pane_id: pane,
        });
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 11,
            client_id: holder,
            pane_id: pane,
        });
        let first = styled_for(&mut plane, 12, holder, pane);
        write_pane(&mut plane, 13, holder, pane, "x");
        let retry = styled_for(&mut plane, 14, holder, pane);
        assert_eq!(
            first.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
        assert_eq!(first.semantic_clipboard_seq, Some(1));
        assert_eq!(
            retry.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
        assert_eq!(retry.semantic_clipboard_seq, first.semantic_clipboard_seq);
        write_pane(&mut plane, 15, holder, pane, "y");
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let (text, acked) = ack_copy(&mut plane, 16, holder, pane, first.semantic_clipboard_seq);
        assert_eq!(text.as_deref(), Some("crates/foo.rs:10:1"));
        assert_eq!(acked, first.semantic_clipboard_seq);
        let second = styled_for(&mut plane, 17, holder, pane);
        assert_eq!(
            second.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
        assert_eq!(second.semantic_clipboard_seq, Some(2));
        let observer_view = styled_for(&mut plane, 18, observer, pane);
        assert!(observer_view.semantic_clipboard.is_none());
    }

    #[test]
    fn semantic_copy_reconnect_cannot_replay_old_client() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: holder,
            pane_id: pane,
        });
        write_pane(&mut plane, 3, holder, pane, "y");
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let _ = plane.handle(ControlRequest::ReleaseLease {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: holder,
            pane_id: pane,
        });
        let reconnect = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 5,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 6,
            client_id: reconnect,
            pane_id: pane,
        });
        let reconnect_view = styled_for(&mut plane, 7, reconnect, pane);
        assert!(
            reconnect_view.semantic_clipboard.is_none(),
            "a new client_id cannot replay another client's pending copy"
        );
        let holder_view = styled_for(&mut plane, 8, holder, pane);
        assert_eq!(
            holder_view.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
    }

    #[test]
    fn semantic_copy_queue_drops_oldest_on_overflow() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: holder,
            pane_id: pane,
        });
        for i in 0..=crate::rich::MAX_PENDING_COPIES {
            write_pane(&mut plane, 10 + i as u64, holder, pane, "y");
            plane
                .live
                .as_mut()
                .unwrap()
                .feed_rich_for_test(pane, &copy)
                .unwrap();
        }
        let view = styled_for(&mut plane, 99, holder, pane);
        assert_eq!(
            view.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
        assert_eq!(view.semantic_clipboard_seq, Some(2));
    }

    #[test]
    fn batched_y_writes_keep_each_copy_request() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: holder,
            pane_id: pane,
        });
        write_pane(&mut plane, 3, holder, pane, "yy");
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let first = styled_for(&mut plane, 4, holder, pane);
        assert_eq!(first.semantic_clipboard_seq, Some(1));
        assert_eq!(
            first.semantic_clipboard.as_deref(),
            Some("crates/foo.rs:10:1")
        );
        let (text, acked) = ack_copy(&mut plane, 5, holder, pane, first.semantic_clipboard_seq);
        assert_eq!(text.as_deref(), Some("crates/foo.rs:10:1"));
        assert_eq!(acked, Some(1));
        let second = styled_for(&mut plane, 6, holder, pane);
        assert_eq!(second.semantic_clipboard_seq, Some(2));
        write_pane(&mut plane, 7, holder, pane, "xy");
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let mixed = styled_for(&mut plane, 8, holder, pane);
        assert_eq!(mixed.semantic_clipboard_seq, Some(2));
        let _ = ack_copy(&mut plane, 9, holder, pane, mixed.semantic_clipboard_seq);
        let leftover = styled_for(&mut plane, 10, holder, pane);
        assert_eq!(leftover.semantic_clipboard_seq, Some(3));
        let _ = ack_copy(
            &mut plane,
            11,
            holder,
            pane,
            leftover.semantic_clipboard_seq,
        );
        let empty = styled_for(&mut plane, 12, holder, pane);
        assert!(empty.semantic_clipboard_seq.is_none());
    }

    #[test]
    fn failed_write_does_not_queue_copy_request() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: holder,
            pane_id: pane,
        });
        plane
            .live
            .as_mut()
            .unwrap()
            .force_write_backpressure_for_test(pane);
        match plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: holder,
            pane_id: pane,
            data: "y".into(),
        }) {
            ControlResponse {
                body: ControlResponseBody::Error { error },
                ..
            } => assert_eq!(error.code, ControlErrorCode::Backpressure),
            other => panic!("expected backpressure, got {other:?}"),
        }
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let view = styled_for(&mut plane, 4, holder, pane);
        assert!(
            view.semantic_clipboard.is_none(),
            "a rejected write must not leave a ghost copy owner"
        );
    }

    #[test]
    fn rejected_rich_input_does_not_queue_copy_request() {
        let (mut plane, _, pane) = live_fixture("READY");
        let copy = feed_diag_surface(&mut plane, pane);
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::AcquireLease {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: holder,
            pane_id: pane,
        });
        match plane.handle(ControlRequest::RichInput {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: holder,
            pane_id: pane,
            input: RichInputKind::Key {
                key: "y".into(),
                modifiers: 0,
            },
        }) {
            ControlResponse {
                body: ControlResponseBody::Error { error },
                ..
            } => assert_eq!(error.code, ControlErrorCode::InputRouteUnavailable),
            other => panic!("expected rejected rich input, got {other:?}"),
        }
        plane
            .live
            .as_mut()
            .unwrap()
            .feed_rich_for_test(pane, &copy)
            .unwrap();
        let view = styled_for(&mut plane, 4, holder, pane);
        assert!(
            view.semantic_clipboard.is_none(),
            "rejected RichInput must not leave a ghost copy owner"
        );
    }

    fn numbered_scroll_spawn() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                "i=1; while [ \"$i\" -le 80 ]; do printf 'L%03d\\n' \"$i\"; i=$((i+1)); done; exec cat"
                    .into(),
            ],
            cwd: Some(PathBuf::from("/tmp")),
            env: BTreeMap::new(),
        }
    }

    fn styled_content(response: ControlResponse) -> PaneStyled {
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::PaneStyled { content },
            } => content,
            other => panic!("expected pane styled, got {other:?}"),
        }
    }

    #[test]
    fn read_pane_styled_view_offset_clamps_and_reports_position() {
        let domain = Domain::bootstrap("scroll").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let pane_id = pane.get();
        let mut plane = ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(pane_id, numbered_scroll_spawn())],
            None,
        )
        .unwrap();
        let client = match plane
            .handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::ClientRegistered { client_id },
            } => client_id,
            other => panic!("register: {other:?}"),
        };

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut live = None;
        while Instant::now() < deadline {
            plane.drain_for_test();
            let styled = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: client,
                pane_id,
                view_offset: None,
            }));
            if styled.content.lines.join("\n").contains("L080") {
                live = Some(styled);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let live = live.expect("fixture never printed L080");
        assert_eq!(live.view_offset, Some(0), "absent offset is live tail");
        assert_eq!(
            live.child_mouse_tracking,
            Some(false),
            "reports child mouse tracking"
        );
        assert_eq!(live.child_mouse_sgr, Some(false));
        let max = live.max_view_scroll.expect("reports max_view_scroll");
        assert!(max >= 56, "80 printed lines on 24 rows ⇒ scrollback {max}");
        assert!(
            live.content.lines.join("\n").contains("L080"),
            "live tail shows the newest line: {:?}",
            live.content.lines
        );
        assert!(
            !live.content.lines.join("\n").contains("L001"),
            "live tail must not include the oldest line"
        );

        let mid = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id,
            view_offset: Some(10),
        }));
        assert_eq!(mid.view_offset, Some(10));
        assert_eq!(mid.max_view_scroll, Some(max));
        let mid_text = mid.content.lines.join("\n");
        assert!(
            !mid_text.contains("L080"),
            "offset 10 must leave the live tail: {mid_text:?}"
        );
        assert!(
            mid_text.contains("L070"),
            "offset 10 should still see L070: {mid_text:?}"
        );

        let clamped = styled_content(plane.handle(ControlRequest::ReadPaneStyled {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id,
            view_offset: Some(u32::MAX),
        }));
        assert_eq!(clamped.view_offset, Some(max));
        assert_eq!(clamped.max_view_scroll, Some(max));
        let old = clamped.content.lines.join("\n");
        assert!(
            old.contains("L001"),
            "clamped offset should reach the oldest line: {old:?}"
        );
    }

    fn mail_attention(
        response: ControlResponse,
    ) -> (u64, Option<MailAttentionState>, PaneInputLedger) {
        match response.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::MailAttention {
                        pane_id,
                        attention,
                        ledger,
                    },
            } => (pane_id, attention, ledger),
            other => panic!("expected mail attention, got {other:?}"),
        }
    }

    fn mail_events(plane: &ControlPlane, after: u64) -> Vec<Event> {
        plane
            .events_after(after, None)
            .unwrap()
            .events
            .into_iter()
            .map(|envelope| envelope.event)
            .collect()
    }

    #[test]
    fn parse_hive_cell_address_accepts_lossless_and_padded() {
        assert_eq!(parse_hive_cell_address("1b@1").unwrap(), "1b@1");
        assert_eq!(parse_hive_cell_address("  12@1  ").unwrap(), "12@1");
        assert!(parse_hive_cell_address("cell:0000000000000000000000000000001b@1").is_ok());
        assert!(parse_hive_cell_address("cli").is_err());
        assert!(parse_hive_cell_address("review").is_err());
        assert!(parse_hive_cell_address("33").is_err());
        assert!(parse_hive_cell_address("cell:33@1").is_err());
        assert!(parse_hive_cell_address("cell:aa@1").is_err());
    }

    #[test]
    fn mail_attention_set_rejects_empty_cell() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let reply = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "  ".into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: None,
            bound_pid: None,
        });
        match reply.body {
            ControlResponseBody::Error { error } => {
                assert_eq!(error.code, ControlErrorCode::InvalidRequest);
                assert!(
                    error.message.contains("non-empty label"),
                    "{}",
                    error.message
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
        assert!(plane.lit_mail_cells().is_empty());
    }

    #[test]
    fn hive_drain_clears_letter_and_blocks_stale_set() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "43@1".into(),
            gen: 1,
            queue_rev: 10,
            depth: 1,
            wake: None,
            bound_pid: Some(99),
        });
        assert_eq!(plane.lit_mail_cells(), vec![(pane, "43@1".into())]);
        assert!(plane.clear_mail_after_hive_drain(pane));
        assert!(plane.lit_mail_cells().is_empty());
        assert!(!plane.clear_mail_after_hive_drain(pane));
        let resurrect = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            cell: "43@1".into(),
            gen: 1,
            queue_rev: 10,
            depth: 1,
            wake: None,
            bound_pid: Some(99),
        });
        let (_, attention, _) = mail_attention(resurrect);
        assert!(
            attention.is_none(),
            "stale Set must not relight after hive drain"
        );
        assert!(matches!(
            mail_events(&plane, 1).last(),
            Some(Event::MailAttentionChanged {
                depth: 0,
                queue_rev: Some(11),
                ..
            })
        ));
    }

    fn pane_mail_depths(plane: &ControlPlane, pane: u64) -> Vec<u32> {
        plane
            .live
            .as_ref()
            .and_then(|live| live.pane_log(pane))
            .map(|log| {
                log.events()
                    .into_iter()
                    .filter_map(|event| match event {
                        crate::pane_log::PaneEvent::MailDepth { depth } => Some(*depth),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn clear_mail_attention_logs_mail_depth_zero() {
        let (mut plane, _, pane) = live_fixture("MAIL");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "43@1".into(),
            gen: 1,
            queue_rev: 10,
            depth: 2,
            wake: None,
            bound_pid: None,
        });
        let raised = pane_mail_depths(&plane, pane);
        assert!(
            raised.contains(&2),
            "raise must log MailDepth 2, got {raised:?}"
        );
        let before = plane.sequence();
        let cleared = plane.handle(ControlRequest::MailAttentionClear {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            queue_rev: 11,
        });
        assert!(mail_attention(cleared).1.is_none());
        assert!(matches!(
            mail_events(&plane, before.saturating_sub(1)).last(),
            Some(Event::MailAttentionChanged {
                depth: 0,
                queue_rev: Some(11),
                ..
            })
        ));
        let depths = pane_mail_depths(&plane, pane);
        assert_eq!(
            depths.last().copied(),
            Some(0),
            "clear must log MailDepth 0, got {depths:?}"
        );
    }

    #[test]
    fn hive_drain_logs_mail_depth_zero() {
        let (mut plane, _, pane) = live_fixture("MAIL");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "43@1".into(),
            gen: 1,
            queue_rev: 10,
            depth: 1,
            wake: None,
            bound_pid: None,
        });
        assert!(plane.clear_mail_after_hive_drain(pane));
        let depths = pane_mail_depths(&plane, pane);
        assert_eq!(
            depths.last().copied(),
            Some(0),
            "hive drain must log MailDepth 0, got {depths:?}"
        );
        assert!(matches!(
            mail_events(&plane, 1).last(),
            Some(Event::MailAttentionChanged {
                depth: 0,
                queue_rev: Some(11),
                ..
            })
        ));
    }

    #[test]
    fn mail_attention_set_is_idempotent_on_pane_and_queue_rev() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let first = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 7,
            depth: 2,
            wake: Some(MailWake::Armed),
            bound_pid: Some(4242),
        });
        let (pane_id, attention, ledger) = mail_attention(first);
        assert_eq!(pane_id, pane);
        let attention = attention.expect("set");
        assert_eq!(attention.queue_rev, 7);
        assert_eq!(attention.depth, 2);
        assert_eq!(attention.wake, Some(MailWake::Armed));
        assert_eq!(attention.bound_pid, Some(4242));
        assert!(!ledger.focused);
        assert!(!ledger.dirty_input);
        assert_eq!(plane.sequence(), 1);
        assert!(matches!(
            &mail_events(&plane, 0)[0],
            Event::MailAttentionChanged {
                pane_id: id,
                depth: 2,
                queue_rev: Some(7),
                ..
            } if *id == pane
        ));

        let again = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 7,
            depth: 2,
            wake: Some(MailWake::Armed),
            bound_pid: Some(4242),
        });
        assert!(matches!(
            again.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailAttention { .. }
            }
        ));
        assert_eq!(plane.sequence(), 1, "identical set must not emit");

        let mismatch = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 7,
            depth: 3,
            wake: Some(MailWake::Rung),
            bound_pid: Some(1),
        });
        let (_, attention, _) = mail_attention(mismatch);
        let attention = attention.expect("same-rev mismatch keeps first write");
        assert_eq!(attention.depth, 2);
        assert_eq!(attention.wake, Some(MailWake::Armed));
        assert_eq!(attention.bound_pid, Some(4242));
        assert_eq!(
            plane.sequence(),
            1,
            "same-rev payload mismatch must not emit"
        );
    }

    #[test]
    fn mail_attention_depth_zero_and_clear_drop_the_bit() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: None,
            bound_pid: None,
        });
        let same_rev_clear = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 1,
            depth: 0,
            wake: None,
            bound_pid: None,
        });
        assert_eq!(
            mail_attention(same_rev_clear)
                .1
                .expect("equal rev clear is no-op")
                .depth,
            1
        );
        assert_eq!(plane.sequence(), 1);

        let cleared = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 2,
            depth: 0,
            wake: None,
            bound_pid: None,
        });
        let (_, attention, _) = mail_attention(cleared);
        assert!(attention.is_none());
        assert_eq!(plane.sequence(), 2);
        let last = mail_events(&plane, 1);
        assert!(matches!(
            last.last(),
            Some(Event::MailAttentionChanged {
                depth: 0,
                cell: None,
                ..
            })
        ));

        let again = plane.handle(ControlRequest::MailAttentionClear {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: client,
            pane_id: pane,
            queue_rev: 2,
        });
        assert!(mail_attention(again).1.is_none());
        assert_eq!(plane.sequence(), 2, "equal-rev clear of empty is silent");
    }

    #[test]
    fn mail_attention_stale_rev_cannot_erase_newer_attention() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 8,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        let newer = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 9,
            depth: 2,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        assert_eq!(mail_attention(newer).1.expect("rev 9").queue_rev, 9);
        assert_eq!(plane.sequence(), 2);

        let stale_set = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 8,
            depth: 0,
            wake: None,
            bound_pid: None,
        });
        let (_, attention, _) = mail_attention(stale_set);
        assert_eq!(
            attention.expect("stale depth-0 must not clear").queue_rev,
            9
        );
        assert_eq!(plane.sequence(), 2);

        let stale_clear = plane.handle(ControlRequest::MailAttentionClear {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: client,
            pane_id: pane,
            queue_rev: 8,
        });
        assert_eq!(
            mail_attention(stale_clear)
                .1
                .expect("stale clear kept")
                .queue_rev,
            9
        );
        assert_eq!(plane.sequence(), 2);
    }

    #[test]
    fn mail_attention_clear_watermark_blocks_older_set() {
        let (mut plane, _, pane) = fixture(16);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 50,
            depth: 2,
            wake: None,
            bound_pid: None,
        });
        let cleared = plane.handle(ControlRequest::MailAttentionClear {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            queue_rev: 51,
        });
        assert!(mail_attention(cleared).1.is_none());
        assert_eq!(plane.sequence(), 2);

        let resurrect = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 50,
            depth: 7,
            wake: None,
            bound_pid: None,
        });
        assert!(
            mail_attention(resurrect).1.is_none(),
            "older Set after newer Clear must not resurrect"
        );
        assert_eq!(plane.sequence(), 2);

        let same_as_clear = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 51,
            depth: 1,
            wake: None,
            bound_pid: None,
        });
        assert!(
            mail_attention(same_as_clear).1.is_none(),
            "Set at the Clear's rev is a no-op"
        );

        let empty_clear = {
            let (mut plane, _, pane) = fixture(8);
            let client = registered_client(plane.handle(ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id: 1,
            }));
            let _ = plane.handle(ControlRequest::MailAttentionClear {
                version: PROTOCOL_VERSION,
                request_id: 2,
                client_id: client,
                pane_id: pane,
                queue_rev: 51,
            });
            assert_eq!(plane.sequence(), 0, "empty-pane Clear only sets watermark");
            let stale = plane.handle(ControlRequest::MailAttentionSet {
                version: PROTOCOL_VERSION,
                request_id: 3,
                client_id: client,
                pane_id: pane,
                cell: "33@1".into(),
                gen: 1,
                queue_rev: 50,
                depth: 1,
                wake: None,
                bound_pid: None,
            });
            mail_attention(stale).1
        };
        assert!(empty_clear.is_none());
    }

    #[test]
    fn mail_attention_rejects_stale_client_and_pane() {
        let (mut plane, _, pane) = fixture(8);
        let missing_client = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 1,
            client_id: 99,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: None,
            bound_pid: None,
        });
        assert_eq!(error_code(&missing_client), ControlErrorCode::StaleId);

        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let missing_pane = plane.handle(ControlRequest::MailAttentionClear {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: 404,
            queue_rev: 1,
        });
        assert_eq!(error_code(&missing_pane), ControlErrorCode::StaleId);
        assert_eq!(plane.sequence(), 0);
    }

    #[test]
    fn suggest_focus_mail_stays_advisory() {
        let (mut plane, window, pane) = fixture(8);
        let before = plane.snapshot().unwrap();
        let response = plane.handle(ControlRequest::SuggestFocus {
            version: PROTOCOL_VERSION,
            request_id: 1,
            window_id: window,
            pane_id: pane,
            reason: Some("mail".into()),
        });
        assert_eq!(mutation_sequence(&response), 1);
        let after = plane.snapshot().unwrap();
        assert_eq!(after.sessions, before.sessions);
        assert_eq!(after.sequence, 1);
        assert!(matches!(
            &mail_events(&plane, 0)[0],
            Event::FocusSuggested {
                reason: Some(reason),
                ..
            } if reason == "mail"
        ));
    }

    #[test]
    fn report_focus_marks_ledger_focused() {
        let (mut plane, window, pane) = fixture(8);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let reported = plane.handle(ControlRequest::ReportFocus {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            window_id: window,
            pane_id: pane,
        });
        let (_, _, ledger) = mail_attention(reported);
        assert!(ledger.focused);
        assert_eq!(plane.sequence(), 1);
        let again = plane.handle(ControlRequest::ReportFocus {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            window_id: window,
            pane_id: pane,
        });
        assert!(mail_attention(again).2.focused);
        assert_eq!(plane.sequence(), 1, "same focus report is idempotent");
    }

    #[test]
    fn close_pane_clears_mail_attention() {
        let (mut plane, window, first) = fixture(8);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let split = plane.handle(ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id: 2,
            window_id: window,
            target_pane_id: first,
            axis: AxisWire::Horizontal,
            ratio: 0.5,
            spawn: spawn(),
            client_id: None,
        });
        assert!(matches!(split.body, ControlResponseBody::Ok { .. }));
        let second = plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .find(|pane| pane.id != first)
            .unwrap()
            .id;
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: second,
            cell: "36@1".into(),
            gen: 1,
            queue_rev: 9,
            depth: 1,
            wake: Some(MailWake::Rung),
            bound_pid: None,
        });
        let before_close = plane.sequence();
        let _ = plane.handle(ControlRequest::Close {
            version: PROTOCOL_VERSION,
            request_id: 4,
            window_id: window,
            pane_id: second,
            prior_focus_id: first,
            client_id: None,
        });
        let events = mail_events(&plane, before_close.saturating_sub(1));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::MailAttentionChanged {
                pane_id,
                depth: 0,
                ..
            } if *pane_id == second
        )));
        assert!(plane.snapshot().unwrap().sessions[0].windows[0]
            .panes
            .iter()
            .all(|pane| pane.mail.is_none()));
    }

    fn mail_inject(response: ControlResponse) -> (MailInjectOutcome, usize) {
        match response.body {
            ControlResponseBody::Ok {
                response:
                    ControlResponseData::MailInject {
                        outcome, nbytes, ..
                    },
            } => (outcome, nbytes),
            other => panic!("expected mail inject, got {other:?}"),
        }
    }

    fn arm_mail(plane: &mut ControlPlane, client: u64, pane: u64, request_id: u64) {
        let _ = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client,
            pane_id: pane,
            cell: "33@1".into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
    }

    fn quiet_unattended(plane: &mut ControlPlane, pane: u64) {
        plane.stamp_last_output_at_ms(
            pane,
            now_unix_ms().saturating_sub(MAIL_INJECT_QUIET_MS + 50),
        );
        plane.clear_output_bytes(pane);
    }

    fn inject_mail(
        plane: &mut ControlPlane,
        client: u64,
        pane: u64,
        request_id: u64,
    ) -> ControlResponse {
        plane.handle(ControlRequest::InjectMail {
            version: PROTOCOL_VERSION,
            request_id,
            client_id: client,
            pane_id: pane,
            queue_rev: 1,
            remaining_attempts: 3,
        })
    }

    #[test]
    fn inject_mail_payload_is_static_notification_plus_cr() {
        assert_eq!(MAIL_INJECT_PAYLOAD.as_bytes().last(), Some(&b'\r'));
        assert_eq!(MAIL_INJECT_PAYLOAD, format!("{PMUX_MAIL_NOTIFICATION}\r"));
    }

    #[test]
    fn mail_attention_set_ignores_legacy_channel_field() {
        // Retired `channel` (hive/switchboard) must not fail to parse.
        let json = r#"{"type":"mail_attention_set","version":1,"request_id":1,"client_id":7,"pane_id":3,"cell":"mail","gen":1,"queue_rev":1,"depth":1,"channel":"switchboard"}"#;
        let request: ControlRequest = serde_json::from_str(json).unwrap();
        let ControlRequest::MailAttentionSet { cell, wake, .. } = request else {
            panic!("expected MailAttentionSet, got {request:?}");
        };
        assert_eq!(cell, "mail");
        assert_eq!(wake, None);
    }

    #[test]
    fn doorbell_defers_when_foreground_is_not_a_known_agent() {
        let (mut plane, pane) = live_inject_fixture("printf 'READY\\n'; exec sleep 999");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        let set = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: MAIL_ATTENTION_CELL.into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        assert!(matches!(set.body, ControlResponseBody::Ok { .. }));
        quiet_unattended(&mut plane, pane);

        // A plain shell in the foreground: nothing is typed, attention stays lit.
        plane.set_inject_agent_override(Some(crate::InjectAgent::Unknown));
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::DeferredNoAgent);
        assert_eq!(nbytes, 0);
        assert!(plane.mail_attention.get(&pane).is_some_and(
            |attention| attention.depth == 1 && attention.wake == Some(MailWake::Armed)
        ));
        assert_eq!(plane.mail_inject_writes(pane), 0, "no write was recorded");

        // The agent CLI starts: the next attempt rings.
        plane.set_inject_agent_override(Some(crate::InjectAgent::Claude));
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 4));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, PMUX_MAIL_NOTIFICATION.len() + 1);
    }

    #[test]
    fn doorbell_defers_when_the_agent_is_only_a_background_job() {
        // A shell prompt in the foreground and a detached background process
        // named `claude`: the descendant walk calls it an agent, the terminal
        // foreground does not, and the doorbell must not type into the shell.
        let dir = std::env::temp_dir().join(format!("pt94-bg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("claude");
        let _ = std::fs::remove_file(&fake);
        std::os::unix::fs::symlink("/bin/sleep", &fake).unwrap();
        let script = format!(
            "python3 -c 'import os,sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' {} 30 >/dev/null 2>&1 & printf 'READY\\n'; exec sleep 999",
            fake.display()
        );
        let (mut plane, pane) = live_inject_fixture(&script);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        let set = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: MAIL_ATTENTION_CELL.into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        assert!(matches!(set.body, ControlResponseBody::Ok { .. }));
        quiet_unattended(&mut plane, pane);

        // Real detection, no override.
        plane.set_inject_agent_override(None);
        let root = plane.live.as_ref().unwrap().child_pid(pane).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while crate::detect_inject_agent(None, Some(root)) != crate::InjectAgent::Claude {
            assert!(
                std::time::Instant::now() < deadline,
                "background claude never appeared under the pane child"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            plane.foreground_agent_for(pane),
            crate::InjectAgent::Unknown
        );
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::DeferredNoAgent);
        assert_eq!(nbytes, 0);
        assert_eq!(plane.mail_inject_writes(pane), 0, "no write was recorded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doorbell_submit_chord_follows_the_foreground_agent_not_a_background_one() {
        // Foreground `claude` (one chunk) behind a shell, with a detached
        // background `codex` (two chunks) that the descendant walk finds
        // first: the gate and the write must both use the foreground agent.
        let dir = std::env::temp_dir().join(format!("pt94-mixed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let claude = dir.join("claude");
        let codex = dir.join("codex");
        for link in [&claude, &codex] {
            let _ = std::fs::remove_file(link);
            std::os::unix::fs::symlink("/bin/sleep", link).unwrap();
        }
        // The pane child sources a script file so its own cmdline names no
        // agent and dash cannot exec-optimise the tail. `codex` is a level-1
        // child; `claude` runs at level 2 behind a shell whose cmdline does
        // not name it, so the breadth-first walk always meets `codex` first.
        let run = dir.join("run.sh");
        std::fs::write(
            &run,
            format!(
                "python3 -c 'import os,sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' {} 30 >/dev/null 2>&1 &\nprintf 'READY\\n'\nA={} sh -c '\"$A\" 999'\nexit 0\n",
                codex.display(),
                claude.display()
            ),
        )
        .unwrap();
        let script = format!(". {}", run.display());
        let (mut plane, pane) = live_inject_fixture(&script);
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        let set = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: MAIL_ATTENTION_CELL.into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        assert!(matches!(set.body, ControlResponseBody::Ok { .. }));
        quiet_unattended(&mut plane, pane);

        plane.set_inject_agent_override(None);
        let root = plane.live.as_ref().unwrap().child_pid(pane).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while plane.foreground_agent_for(pane) != crate::InjectAgent::Claude {
            assert!(
                std::time::Instant::now() < deadline,
                "foreground claude never appeared under the pane child"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            crate::detect_inject_agent(None, Some(root)),
            crate::InjectAgent::Codex,
            "the descendant walk must find the background codex first"
        );
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(
            nbytes,
            PMUX_MAIL_NOTIFICATION.len() + 1,
            "one chunk: the claude chord, not codex's two"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn mail_attention_set_injects_pmux_token() {
        // PMUX_MAIL is 9 bytes; the fixture child consumes exactly
        // that many before printing WOKE.
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=9 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        let set = plane.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            pane_id: pane,
            cell: MAIL_ATTENTION_CELL.into(),
            gen: 1,
            queue_rev: 1,
            depth: 1,
            wake: Some(MailWake::Armed),
            bound_pid: None,
        });
        assert!(
            matches!(set.body, ControlResponseBody::Ok { .. }),
            "set failed: {:?}",
            set.body
        );
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, PMUX_MAIL_NOTIFICATION.len() + 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_woke = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("WOKE"))
            {
                saw_woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_woke, "child should consume the exact PMUX_MAIL payload");
    }

    #[test]
    fn inject_mail_spinner_ticks_are_not_busy() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=9 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 2);
        quiet_unattended(&mut plane, pane);
        for _ in 0..90 {
            plane.record_output_bytes(pane, 32);
        }
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
    }

    #[test]
    fn inject_mail_large_dump_is_busy() {
        let (mut plane, pane) =
            live_inject_fixture("printf 'READY\\n'; stty -echo 2>/dev/null; exec sleep 999");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 2);
        quiet_unattended(&mut plane, pane);
        plane.record_output_bytes(pane, MAIL_INJECT_BUSY_BYTES);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::DeferredBusy);
        assert_eq!(nbytes, 0);
        assert_eq!(plane.mail_inject_writes(pane), 0);
    }

    #[test]
    fn inject_mail_focused_writes_when_quiet() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let snapshot = plane.snapshot().unwrap();
        let window = snapshot.sessions[0].windows[0].id;
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        quiet_unattended(&mut plane, pane);
        let focused = plane.handle(ControlRequest::ReportFocus {
            version: PROTOCOL_VERSION,
            request_id: 2,
            client_id: client,
            window_id: window,
            pane_id: pane,
        });
        assert!(matches!(focused.body, ControlResponseBody::Ok { .. }));
        arm_mail(&mut plane, client, pane, 3);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 4));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
        assert_eq!(plane.mail_inject_writes(pane), 1);
    }

    #[test]
    fn delayed_mail_nudge_retries_after_busy_deferral() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 3);
        quiet_unattended(&mut plane, pane);
        plane.record_output_bytes(pane, MAIL_INJECT_BUSY_BYTES);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 4));
        assert_eq!(outcome, MailInjectOutcome::DeferredBusy);
        assert_eq!(nbytes, 0);
        assert_eq!(plane.mail_inject_writes(pane), 0);

        plane.clear_output_bytes(pane);
        quiet_unattended(&mut plane, pane);
        plane.mail_nudges.get_mut(&pane).unwrap().due_at_ms = 0;
        plane.retry_mail_nudges();
        assert_eq!(plane.mail_inject_writes(pane), 1);
        if let Some(nudge) = plane.mail_nudges.get_mut(&pane) {
            nudge.due_at_ms = 0;
        }
        if let Some(record) = plane.mail_inject.get_mut(&pane) {
            record.hide_until_ms = 0;
        }
        plane.retry_mail_nudges();
        assert_eq!(plane.mail_inject_writes(pane), 2);
        assert!(!plane.mail_nudges.contains_key(&pane));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_woke = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("WOKE"))
            {
                saw_woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_woke, "delayed nudge must reach the quiet pane");
    }

    #[test]
    fn mail_inbox_peek_cancels_delayed_nudge() {
        let (mut plane, _, _) = fixture(8);
        let sender = mail_hello(&mut plane, 1, "operator-b");
        let recipient = mail_hello(&mut plane, 2, "operator-a");
        let created = plane.handle(ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id: 3,
            name: "work".into(),
            spawn: spawn(),
            cols: None,
            rows: None,
            agent_id: Some("operator-a".into()),
            headless: false,
        });
        let (_, _, pane) = session_ids(&created);
        let sent = plane.handle(ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: sender,
            to: "operator-a".into(),
            summary: "peek cancels".into(),
            body: String::new(),
        });
        assert!(matches!(sent.body, ControlResponseBody::Ok { .. }));
        assert!(plane.mail_nudges.contains_key(&pane));

        let inbox = plane.handle(ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id: 5,
            client_id: recipient,
        });
        assert!(matches!(
            inbox.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::MailDepth { open: 1, held: 0 }
            }
        ));
        assert!(!plane.mail_nudges.contains_key(&pane));
    }

    #[test]
    fn inject_mail_lease_held_does_not_write() {
        let (mut plane, _, pane) = fixture(16);
        // Lease semantics under test; the fixture child is a shell, so model an agent pane.
        plane.set_inject_agent_override(Some(crate::InjectAgent::Claude));
        let holder = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let injector = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: holder,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        arm_mail(&mut plane, injector, pane, 4);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, injector, pane, 5));
        assert_eq!(outcome, MailInjectOutcome::DeferredLease);
        assert_eq!(nbytes, 0);
        assert_eq!(plane.mail_inject_writes(pane), 0);
    }

    fn wait_spawn_then_quiet(plane: &mut ControlPlane, pane: u64, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        // Phase 1: wait for the spawn marker to appear.
        loop {
            plane.drain_for_test();
            let present = plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains(needle));
            if present {
                break;
            }
            if Instant::now() >= deadline {
                panic!("spawn never printed {needle:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Phase 2: wait until the pane stops changing. The child's post-marker
        // setup (e.g. `stty -echo; exec sleep`) can emit output after the
        // marker; under load such a late revision bump can land inside
        // inject_mail's epoch wait and be misread as the child consuming the
        // injected payload (Wrote instead of the expected Stuck). Require the
        // revision to hold steady longer than that epoch window first.
        let quiet_window = Duration::from_millis(MAIL_INJECT_EPOCH_WAIT_MS + 100);
        let mut last_rev = plane.pane_revision(pane);
        let mut stable_since = Instant::now();
        while stable_since.elapsed() < quiet_window && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(MAIL_INJECT_EPOCH_SLICE_MS));
            plane.drain_for_test();
            let rev = plane.pane_revision(pane);
            if rev != last_rev {
                last_rev = rev;
                stable_since = Instant::now();
            }
        }
        quiet_unattended(plane, pane);
    }

    fn live_inject_fixture(script: &str) -> (ControlPlane, u64) {
        let domain = Domain::bootstrap("inject").unwrap();
        let session = domain.sessions().next().unwrap();
        let window = session.windows[0];
        let pane = domain.window(window).unwrap().layout.panes()[0];
        let mut plane = ControlPlane::new_live(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(32),
            [(
                pane.get(),
                SpawnSpec {
                    program: "/bin/sh".into(),
                    argv: vec!["-c".into(), script.into()],
                    cwd: Some(PathBuf::from("/tmp")),
                    env: BTreeMap::new(),
                },
            )],
            None,
        )
        .unwrap();
        plane.set_inject_agent_override(Some(crate::InjectAgent::Claude));
        (plane, pane.get())
    }

    #[test]
    fn inject_mail_quiet_unattended_writes_exact_bytes_once() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=9 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 2);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
        assert_eq!(nbytes, PMUX_MAIL_NOTIFICATION.len() + 1);
        assert_eq!(plane.mail_inject_writes(pane), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_woke = false;
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("WOKE"))
            {
                saw_woke = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_woke, "child should consume the exact inject payload");
        quiet_unattended(&mut plane, pane);
        let (again, n2) = mail_inject(inject_mail(&mut plane, client, pane, 4));
        assert_eq!(again, MailInjectOutcome::DeferredHide);
        assert_eq!(n2, 0);
        assert_eq!(plane.mail_inject_writes(pane), 1);
    }

    #[test]
    fn inject_mail_wrote_gets_one_bounded_verification_retry() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; stty -echo 2>/dev/null; (sleep 0.1; printf 'TICK\\n') & exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\\n").contains("READY"))
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        arm_mail(&mut plane, client, pane, 2);
        let (outcome, _) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(plane.mail_inject_writes(pane), 1);
        assert!(plane
            .mail_nudges
            .get(&pane)
            .is_some_and(|nudge| nudge.verification));
        let tick_deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_tick = false;
        while Instant::now() < tick_deadline {
            plane.drain_for_test();
            if plane
                .live
                .as_ref()
                .and_then(|live| live.content(pane))
                .is_some_and(|content| content.lines.join("\n").contains("TICK"))
            {
                saw_tick = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_tick, "child never printed TICK");

        // Make both gates due explicitly. Do not rely on wall-clock timing.
        plane.mail_nudges.get_mut(&pane).unwrap().due_at_ms = 0;
        plane.mail_inject.get_mut(&pane).unwrap().hide_until_ms = 0;
        plane.retry_mail_nudges();
        assert_eq!(plane.mail_inject_writes(pane), 2);
        assert!(!plane.mail_nudges.contains_key(&pane));

        plane.retry_mail_nudges();
        assert_eq!(plane.mail_inject_writes(pane), 2);
        let snapshot = plane.snapshot().unwrap();
        let diagnostic = snapshot.sessions[0].windows[0].panes[0]
            .mail_inject
            .as_ref()
            .expect("last mail inject diagnostic");
        assert_eq!(diagnostic.outcome, MailInjectOutcome::Stuck);
    }

    #[test]
    fn inject_mail_silent_draft_is_stuck_without_a_second_write() {
        let (mut plane, pane) =
            live_inject_fixture("printf 'READY\\n'; stty -echo 2>/dev/null; exec sleep 999");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 2);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Stuck);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
        assert_eq!(plane.mail_inject_writes(pane), 1);
        let (again, n2) = mail_inject(inject_mail(&mut plane, client, pane, 4));
        assert_eq!(again, MailInjectOutcome::SkippedStuck);
        assert_eq!(n2, 0);
        assert_eq!(plane.mail_inject_writes(pane), 1);
    }

    #[test]
    fn write_pane_updates_input_ledger_cr_and_dirty() {
        let (mut plane, _, pane) = live_fixture("LEDGER");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let dirty = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            data: "hello".into(),
        });
        assert!(matches!(
            dirty.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        let ledger = plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
            .ledger
            .clone();
        assert_eq!(ledger.controller_id, Some(client));
        assert!(ledger.last_controller_write_at_ms.is_some());
        assert!(!ledger.last_write_ended_with_cr);
        assert!(ledger.dirty_input);

        let clean = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: client,
            pane_id: pane,
            data: "line\r".into(),
        });
        assert!(matches!(
            clean.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        let ledger = plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
            .ledger
            .clone();
        assert!(ledger.last_write_ended_with_cr);
        assert!(!ledger.dirty_input);
    }

    #[test]
    fn release_lease_keeps_dirty_input() {
        let (mut plane, _, pane) = live_fixture("LEDGER");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let dirty = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            data: "half-typed".into(),
        });
        assert!(matches!(
            dirty.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        assert!(
            plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
                .ledger
                .dirty_input
        );
        assert!(matches!(
            plane
                .handle(ControlRequest::ReleaseLease {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let ledger = plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
            .ledger
            .clone();
        assert!(
            ledger.dirty_input,
            "lease release must not clear an unsubmitted line"
        );
        assert!(ledger.last_controller_write_at_ms.is_some());
        assert!(!ledger.last_write_ended_with_cr);
    }

    #[test]
    fn disconnect_client_keeps_dirty_input() {
        let (mut plane, _, pane) = live_fixture("LEDGER");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: client,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let dirty = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: client,
            pane_id: pane,
            data: "half-typed".into(),
        });
        assert!(matches!(
            dirty.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: client,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let ledger = plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
            .ledger
            .clone();
        assert!(
            ledger.dirty_input,
            "disconnect must not clear an unsubmitted line"
        );
        assert!(ledger.last_controller_write_at_ms.is_some());
        assert!(!ledger.last_write_ended_with_cr);
    }

    #[test]
    fn inject_mail_after_controller_disconnects_mid_line() {
        let (mut plane, pane) = live_inject_fixture("printf 'READY\\n'; exec cat");
        let typist = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let ringer = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let dirty = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: typist,
            pane_id: pane,
            data: "half-typed".into(),
        });
        assert!(matches!(
            dirty.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::WriteQueued { .. }
            }
        ));
        assert!(matches!(
            plane
                .handle(ControlRequest::DisconnectClient {
                    version: PROTOCOL_VERSION,
                    request_id: 5,
                    client_id: typist,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        arm_mail(&mut plane, ringer, pane, 6);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, ringer, pane, 7));
        assert_eq!(outcome, MailInjectOutcome::DeferredDirty);
        assert_eq!(nbytes, 0);
    }

    #[test]
    fn inject_mail_defers_while_partial_line_is_open_after_lease_release() {
        let (mut plane, pane) = live_inject_fixture("printf 'READY\\n'; exec cat");
        let typist = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let ringer = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 3,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let _ = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 4,
            client_id: typist,
            pane_id: pane,
            data: "hel".into(),
        });
        assert!(matches!(
            plane
                .handle(ControlRequest::ReleaseLease {
                    version: PROTOCOL_VERSION,
                    request_id: 5,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        arm_mail(&mut plane, ringer, pane, 6);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, ringer, pane, 7));
        assert_eq!(outcome, MailInjectOutcome::DeferredDirty);
        assert_eq!(nbytes, 0);
    }

    #[test]
    fn inject_mail_after_cr_waits_for_input_idle() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=9 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let typist = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        let _ = plane.handle(ControlRequest::WritePane {
            version: PROTOCOL_VERSION,
            request_id: 3,
            client_id: typist,
            pane_id: pane,
            data: "hello\r".into(),
        });
        assert!(matches!(
            plane
                .handle(ControlRequest::ReleaseLease {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        arm_mail(&mut plane, typist, pane, 5);
        quiet_unattended(&mut plane, pane);
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, typist, pane, 6));
        assert_eq!(outcome, MailInjectOutcome::DeferredTyping);
        assert_eq!(nbytes, 0);
        plane.stamp_last_input_at_ms(
            pane,
            now_unix_ms().saturating_sub(MAIL_INJECT_INPUT_IDLE_MS + 50),
        );
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, typist, pane, 7));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
    }

    #[test]
    fn inject_mail_never_fires_while_typing_continues() {
        let (mut plane, pane) =
            live_inject_fixture("printf 'READY\\n'; stty -echo 2>/dev/null; exec sleep 999");
        let typist = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        assert!(matches!(
            plane
                .handle(ControlRequest::AcquireLease {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    client_id: typist,
                    pane_id: pane,
                })
                .body,
            ControlResponseBody::Ok { .. }
        ));
        arm_mail(&mut plane, typist, pane, 3);
        for (i, chunk) in ["h", "e", "l"].iter().enumerate() {
            let _ = plane.handle(ControlRequest::WritePane {
                version: PROTOCOL_VERSION,
                request_id: 10 + i as u64,
                client_id: typist,
                pane_id: pane,
                data: (*chunk).into(),
            });
            quiet_unattended(&mut plane, pane);
            let (outcome, nbytes) =
                mail_inject(inject_mail(&mut plane, typist, pane, 20 + i as u64));
            assert_eq!(outcome, MailInjectOutcome::DeferredDirty);
            assert_eq!(nbytes, 0);
        }
    }

    #[test]
    fn inject_mail_spinner_only_after_input_idle_is_wrote() {
        let (mut plane, pane) = live_inject_fixture(
            "printf 'READY\\n'; dd bs=1 count=9 of=/dev/null 2>/dev/null; printf 'WOKE\\n'; exec sleep 999",
        );
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        wait_spawn_then_quiet(&mut plane, pane, "READY");
        arm_mail(&mut plane, client, pane, 2);
        quiet_unattended(&mut plane, pane);
        plane.stamp_last_input_at_ms(
            pane,
            now_unix_ms().saturating_sub(MAIL_INJECT_INPUT_IDLE_MS + 5_000),
        );
        for _ in 0..90 {
            plane.record_output_bytes(pane, 32);
        }
        let (outcome, nbytes) = mail_inject(inject_mail(&mut plane, client, pane, 3));
        assert_eq!(outcome, MailInjectOutcome::Wrote);
        assert_eq!(nbytes, MAIL_INJECT_PAYLOAD.len());
    }

    #[test]
    fn dirty_input_clears_after_idle_with_no_write_and_no_output() {
        let (mut plane, _, pane) = live_fixture("LEDGER");
        plane.stamp_write_ledger(
            pane,
            now_unix_ms().saturating_sub(MAIL_INJECT_DIRTY_IDLE_MS + 50),
            false,
        );
        plane.stamp_last_output_at_ms(
            pane,
            now_unix_ms().saturating_sub(MAIL_INJECT_DIRTY_IDLE_MS + 50),
        );
        let ledger = plane.snapshot().unwrap().sessions[0].windows[0].panes[0]
            .ledger
            .clone();
        assert!(!ledger.dirty_input);
        assert!(!ledger.last_write_ended_with_cr);
    }
    #[test]
    fn pane_write_guards_identity_space_lease_dirty_busy_and_backpressure() {
        let (mut plane, _, pane) = live_fixture("PANE-WRITE-GUARDS");
        let client = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 1,
        }));
        let other = registered_client(plane.handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 2,
        }));
        let pid = plane.live.as_ref().unwrap().child_pid(pane).unwrap();
        let attempt = |plane: &mut ControlPlane, client, pid| {
            plane.intentional_pane_write(client, pane, pid, "hello".into(), PaneWriteSubmit::Enter)
        };
        assert_eq!(
            attempt(&mut plane, u64::MAX, pid).unwrap_err().code,
            ControlErrorCode::StaleId
        );
        assert_eq!(
            attempt(&mut plane, client, 0).unwrap_err().code,
            ControlErrorCode::StaleId
        );
        assert_eq!(
            attempt(&mut plane, client, pid + 1).unwrap_err().code,
            ControlErrorCode::StaleId
        );
        plane
            .client_spaces
            .insert(client_id(client).unwrap(), "elsewhere".into());
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::InputRouteUnavailable
        );
        plane.client_spaces.clear();
        plane
            .domain
            .acquire_controller(pane_id(pane).unwrap(), client_id(other).unwrap())
            .unwrap();
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::NotController
        );
        plane
            .domain
            .release_controller(pane_id(pane).unwrap(), client_id(other).unwrap())
            .unwrap();
        plane
            .domain
            .acquire_controller(pane_id(pane).unwrap(), client_id(client).unwrap())
            .unwrap();
        plane.write_ledger.insert(pane, (now_unix_ms(), false));
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::InputDirty
        );
        plane.write_ledger.clear();
        plane.last_input_at_ms.insert(pane, now_unix_ms());
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::InputBusy
        );
        plane.last_input_at_ms.clear();
        plane.output_bytes.insert(
            pane,
            VecDeque::from([(now_unix_ms(), MAIL_INJECT_BUSY_BYTES)]),
        );
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::InputBusy
        );
        plane.output_bytes.clear();
        plane
            .live
            .as_mut()
            .unwrap()
            .force_write_backpressure_for_test(pane);
        assert_eq!(
            attempt(&mut plane, client, pid).unwrap_err().code,
            ControlErrorCode::Backpressure
        );
        assert!(
            !plane.write_ledger.contains_key(&pane),
            "a rejected write must not dirty input"
        );
    }
}

#[cfg(unix)]
fn owned_socket(_path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_socket() && metadata.uid() == rustix::process::geteuid().as_raw()
}
#[cfg(windows)]
fn owned_socket(path: &Path, _metadata: &fs::Metadata) -> bool {
    crate::platform::owned_socket(path)
}
#[cfg(windows)]
pub fn default_socket_path(instance: &str) -> io::Result<PathBuf> {
    validate_socket_instance(instance)?;
    Ok(crate::platform::user_directory()?.join(socket_filename(instance)))
}
#[cfg(windows)]
pub fn diagnose_runtime_dir_miss_from_env(_resolved_socket: &Path) -> Option<RuntimeDirMiss> {
    // This diagnostic is exclusively for systemd user runtime directories.
    None
}
#[cfg(windows)]
fn control_peer_closed(stream: &UnixStream) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::*;
    unsafe {
        let mut fd = WSAPOLLFD {
            fd: stream.as_raw_socket() as usize,
            events: POLLRDNORM,
            revents: 0,
        };
        let ready = WSAPoll(&mut fd, 1, 0);
        if ready == SOCKET_ERROR {
            return true;
        }
        if ready == 0 {
            return false;
        }
        if fd.revents & (POLLHUP | POLLERR | POLLNVAL) != 0 {
            return true;
        }
        let mut byte = 0u8;
        let result = recv(fd.fd, &mut byte, 1, MSG_PEEK);
        result == 0 || (result == SOCKET_ERROR && WSAGetLastError() != WSAEWOULDBLOCK)
    }
}
