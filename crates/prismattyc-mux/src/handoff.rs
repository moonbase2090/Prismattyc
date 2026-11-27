//! What one pmux server hands to the next in a live update.
//!
//! In a live update the server execs the new server binary in its own process.
//! Each pane's PTY master stays open across the exec, so the programs in the
//! panes keep running and stay children of the same process. This record
//! carries what lives only in the old server's memory: each pane's descriptor
//! number and program, its screen and recent output, and its program resume
//! state. It is versioned so a new server can refuse a record it does not
//! understand, and the old server falls back to a normal restart.

use std::collections::BTreeMap;
use std::path::PathBuf;

use prismattyc_emulator::EmulatorStateV1;
use serde::{Deserialize, Serialize};

use crate::pane_log::PaneLogFrame;
use crate::program_resume::ResumeRecord;
use crate::remote_size::SizeOwner;

/// The record format this build writes and reads.
pub const HANDOFF_FORMAT: u32 = 1;

/// One pane, as the old server leaves it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PaneHandoff {
    pub pane_id: u64,
    /// The PTY master's descriptor number, kept open across the exec.
    pub fd: i32,
    /// The pane's program. A child of the server process.
    pub pid: Option<u32>,
    /// The program already ended and was reaped: the pane shows its last
    /// screen, and `pid` must never be signalled.
    pub exited: bool,
    pub cols: u16,
    pub rows: u16,
    pub outer_rows: u16,
    pub cell_px: (u32, u32),
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_cell: Option<String>,
    /// The screen at `snapshot_seq`, when the emulator was at a point it can
    /// export (not in the middle of an escape sequence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<EmulatorStateV1>,
    pub snapshot_seq: u64,
    /// The pane's output log. Frames after `snapshot_seq` are replayed onto
    /// the snapshot; all of them stay available to clients that catch up.
    pub tail: Vec<PaneLogFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_owner: Option<SizeOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_record: Option<ResumeRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resume_times: BTreeMap<u32, u64>,
}
