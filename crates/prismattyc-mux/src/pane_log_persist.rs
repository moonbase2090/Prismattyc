//! Persist pane-log snapshot + tail next to `mail.db` (PT-115).
//!
//! The file is instance-keyed (`pane-log-<instance>.json`). Restore matches
//! session name and pane index, not pane slot order.
//!
//! The on-disk format is versioned. The emulator blob is behind
//! [`ScreenCodec`]. Production uses [`EmulatorStateCodec`] (`emulator-state-v1`):
//! `serde_json` of [`prismattyc_emulator::EmulatorStateV1`]. A `replay-v0`
//! file is a codec mismatch and starts empty. Export omits the blob when
//! the parser is mid-sequence or graphics are pending; the tail stays.
//! A snapshot that fails import is dropped and the tail is replayed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use prismattyc_emulator::{Emulator, EmulatorStateV1};
use serde::{Deserialize, Serialize};

use crate::mailbox::default_mail_db_path;
use crate::pane_log::{PaneEvent, PaneLogFrame};

/// On-disk format number. A mismatch starts empty with one reset line.
pub(crate) const PERSIST_FORMAT: u32 = 1;
/// How often a dirty log may be written while the daemon runs.
pub(crate) const PERSIST_CADENCE: std::time::Duration = std::time::Duration::from_secs(2);
/// Visible line when the persist file is unusable.
pub(crate) const RESET_LINE: &str = "pane log reset";
const SCROLLBACK: usize = 10_000;

/// Resolve where this daemon may persist, if anywhere.
///
/// `PMUX_PANE_LOG=off` disables persist. Any other non-empty value is an
/// explicit path (tests and ad-hoc daemons). Otherwise only sockets in the
/// instance socket dir (`$XDG_RUNTIME_DIR/prismattyc/pmux.sock` or
/// `pmux-<name>.sock`) write `<data>/prismattyc/pane-log-<instance>.json`.
/// An explicit `--socket` outside that dir does not touch the user's data dir.
#[must_use]
pub fn resolve_pane_log_path(socket: &Path) -> Option<PathBuf> {
    resolve_pane_log_path_from(
        socket,
        std::env::var("PMUX_PANE_LOG").ok().as_deref(),
        default_mail_db_path().parent().map(Path::to_path_buf),
        &instance_socket_dirs(),
    )
}

pub(crate) fn instance_socket_dirs() -> Vec<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    let mut dirs = Vec::new();
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            dirs.push(runtime.join("prismattyc"));
        }
    }
    dirs.push(PathBuf::from(format!("/tmp/prismattyc-{uid}")));
    dirs
}

/// `pmux.sock` → `default`; `pmux-NAME.sock` → `NAME`.
pub(crate) fn instance_from_socket_name(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".sock")?;
    if stem == "pmux" {
        return Some("default");
    }
    stem.strip_prefix("pmux-").filter(|rest| !rest.is_empty())
}

pub(crate) fn resolve_pane_log_path_from(
    socket: &Path,
    pane_log_env: Option<&str>,
    data_dir: Option<PathBuf>,
    instance_dirs: &[PathBuf],
) -> Option<PathBuf> {
    match pane_log_env.map(str::trim) {
        Some("off") | Some("OFF") => return None,
        Some(path) if !path.is_empty() => return Some(PathBuf::from(path)),
        _ => {}
    }
    let name = socket.file_name()?.to_str()?;
    let instance = instance_from_socket_name(name)?;
    let parent = socket.parent()?;
    if !instance_dirs.iter().any(|dir| parent == dir) {
        return None;
    }
    let data = data_dir?;
    Some(data.join(format!("pane-log-{instance}.json")))
}

/// Why a persist file cannot be restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreFail {
    Corrupt,
    VersionMismatch { found: u32 },
    CodecMismatch { found: String },
}

impl RestoreFail {
    pub(crate) fn log_reason(&self) -> &'static str {
        match self {
            Self::Corrupt => "corrupt persist",
            Self::VersionMismatch { .. } => "version mismatch",
            Self::CodecMismatch { .. } => "codec mismatch",
        }
    }
}

/// Export/import for the optional compacted snapshot blob.
pub(crate) trait ScreenCodec: Send + Sync {
    fn id(&self) -> &'static str;
    fn export(&self, emulator: &Emulator) -> Result<Vec<u8>, PersistError>;
    fn import(&self, bytes: &[u8], cols: usize, rows: usize) -> Result<Emulator, PersistError>;
}

/// Tail-replay codec. Snapshot bytes are empty; restore rebuilds from events.
/// Kept for persist-format tests. Production writes [`EmulatorStateCodec`].
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReplayCodec;

#[cfg(test)]
impl ScreenCodec for ReplayCodec {
    fn id(&self) -> &'static str {
        "replay-v0"
    }

    fn export(&self, _emulator: &Emulator) -> Result<Vec<u8>, PersistError> {
        Ok(Vec::new())
    }

    fn import(&self, _bytes: &[u8], cols: usize, rows: usize) -> Result<Emulator, PersistError> {
        Ok(fresh_emulator(cols, rows))
    }
}

/// Compact emulator snapshot. Codec id `emulator-state-v1`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EmulatorStateCodec;

impl ScreenCodec for EmulatorStateCodec {
    fn id(&self) -> &'static str {
        "emulator-state-v1"
    }

    fn export(&self, emulator: &Emulator) -> Result<Vec<u8>, PersistError> {
        let Ok(state) = emulator.export_state() else {
            return Ok(Vec::new());
        };
        Ok(serde_json::to_vec(&state)?)
    }

    fn import(&self, bytes: &[u8], cols: usize, rows: usize) -> Result<Emulator, PersistError> {
        if bytes.is_empty() {
            return Ok(fresh_emulator(cols, rows));
        }
        let state: EmulatorStateV1 = serde_json::from_slice(bytes)?;
        let state_cols = usize::try_from(state.screen.columns)
            .map_err(|_| PersistError::State("snapshot columns overflow".into()))?;
        let state_rows = usize::try_from(state.screen.rows)
            .map_err(|_| PersistError::State("snapshot rows overflow".into()))?;
        if state_cols != cols || state_rows != rows {
            return Err(PersistError::State(format!(
                "snapshot size {state_cols}x{state_rows} does not match pane {cols}x{rows}"
            )));
        }
        Emulator::import_state(state).map_err(|error| PersistError::State(error.to_string()))
    }
}

#[derive(Debug)]
pub(crate) enum PersistError {
    Io(io::Error),
    Json(serde_json::Error),
    State(String),
}

impl From<io::Error> for PersistError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for PersistError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::State(message) => write!(f, "{message}"),
        }
    }
}

/// One persisted pane: snapshot at `snapshot_seq`, then `tail`.
/// Restore matches `session` + `pane_index` only. A new session name gets nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistPane {
    pub session: String,
    pub pane_index: usize,
    pub snapshot_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Vec<u8>>,
    pub cols: u16,
    pub rows: u16,
    pub cell_px: (u32, u32),
    pub tail: Vec<PaneLogFrame>,
}

/// Root persist document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistFile {
    pub format: u32,
    pub codec: String,
    pub panes: Vec<PersistPane>,
}

impl PersistFile {
    pub(crate) fn from_panes(panes: Vec<PersistPane>, codec: &str) -> Self {
        Self {
            format: PERSIST_FORMAT,
            codec: codec.to_string(),
            panes,
        }
    }

    pub(crate) fn validate(&self, format: u32, codec: &str) -> Result<(), RestoreFail> {
        if self.format != format {
            return Err(RestoreFail::VersionMismatch { found: self.format });
        }
        if self.codec != codec {
            return Err(RestoreFail::CodecMismatch {
                found: self.codec.clone(),
            });
        }
        Ok(())
    }
}

/// Parse bytes into a persist file. Invalid JSON is corrupt.
pub(crate) fn parse_persist_bytes(bytes: &[u8]) -> Result<PersistFile, RestoreFail> {
    serde_json::from_slice(bytes).map_err(|_| RestoreFail::Corrupt)
}

pub(crate) fn write_persist(path: &Path, file: &PersistFile) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(file)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn read_persist(path: &Path) -> Result<PersistFile, RestoreFail> {
    let bytes = fs::read(path).map_err(|_| RestoreFail::Corrupt)?;
    parse_persist_bytes(&bytes)
}

/// Apply Output/Resize to a replica. Do not send DSR/CPR/DA to a child.
pub(crate) fn replay_event(emulator: &mut Emulator, event: &PaneEvent) {
    match event {
        PaneEvent::Output { bytes } => {
            let _ = emulator.feed(bytes);
            let _ = emulator.take_pending_replies();
        }
        PaneEvent::Resize {
            cols,
            rows,
            cell_px,
            size_owner: _,
            reflow,
        } => {
            emulator.set_cell_pixels(cell_px.0.max(1), cell_px.1.max(1));
            if *reflow {
                emulator.resize((*cols as usize).max(1), (*rows as usize).max(1));
            } else {
                emulator.resize_legacy((*cols as usize).max(1), (*rows as usize).max(1));
            }
        }
        _ => {}
    }
}

/// Rebuild an emulator from a persist pane (snapshot import + tail).
///
/// A snapshot that fails import is dropped. Restore then replays every
/// retained tail frame (the snapshot sequence is cleared).
pub(crate) fn restore_emulator(
    rec: &PersistPane,
    codec: &dyn ScreenCodec,
) -> Result<Emulator, PersistError> {
    let cols = rec.cols.max(1) as usize;
    let rows = rec.rows.max(1) as usize;
    let (mut emulator, replay_from) = match rec.snapshot.as_deref() {
        Some(bytes) if !bytes.is_empty() => match codec.import(bytes, cols, rows) {
            Ok(emulator) => (emulator, rec.snapshot_seq),
            Err(_) => (codec.import(&[], cols, rows)?, 0),
        },
        _ => (codec.import(&[], cols, rows)?, rec.snapshot_seq),
    };
    emulator.set_cell_pixels(rec.cell_px.0.max(1), rec.cell_px.1.max(1));
    for frame in rec.tail.iter().filter(|frame| frame.seq > replay_from) {
        replay_event(&mut emulator, &frame.event);
    }
    Ok(emulator)
}

pub(crate) fn reset_output_event(reason: &str) -> PaneEvent {
    PaneEvent::Output {
        bytes: format!("{RESET_LINE}: {reason}\r\n").into_bytes(),
    }
}

pub(crate) fn fresh_emulator(cols: usize, rows: usize) -> Emulator {
    let mut emulator = Emulator::new(cols.max(1), rows.max(1), SCROLLBACK);
    emulator.set_retain_alt_history(true);
    emulator
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_log::{PaneEvent, PaneLog, PaneLogFrame};

    const FIXTURE: &[u8] =
        include_bytes!("../../prismattyc-emulator/tests/fixtures/pt72-session.bin");

    fn fixture_pane() -> PersistPane {
        let mut log = PaneLog::new(4 * 1024 * 1024);
        log.append(PaneEvent::Output {
            bytes: FIXTURE.to_vec(),
        });
        PersistPane {
            session: "default".into(),
            pane_index: 0,
            snapshot_seq: 0,
            snapshot: None,
            cols: 80,
            rows: 24,
            cell_px: (8, 16),
            tail: log.iter().cloned().collect(),
        }
    }

    #[test]
    fn legacy_resize_replay_clips_and_new_resize_reflows() {
        let legacy: PaneEvent =
            serde_json::from_str(r#"{"kind":"resize","cols":4,"rows":3,"cell_px":[8,16]}"#)
                .unwrap();
        let mut old = Emulator::new(8, 3, 10);
        old.feed(b"abcdefgh");
        replay_event(&mut old, &legacy);
        assert_eq!(old.screen().history_line_text(0), "abcd");
        let mut new = Emulator::new(8, 3, 10);
        new.feed(b"abcdefgh");
        replay_event(
            &mut new,
            &PaneEvent::Resize {
                cols: 4,
                rows: 3,
                cell_px: (8, 16),
                size_owner: None,
                reflow: true,
            },
        );
        assert_eq!(new.screen().history_line_text(1), "efgh");
    }

    #[test]
    fn encode_decode_round_trip() {
        let file = PersistFile::from_panes(vec![fixture_pane()], ReplayCodec.id());
        let bytes = serde_json::to_vec(&file).unwrap();
        let parsed = parse_persist_bytes(&bytes).unwrap();
        assert_eq!(parsed, file);
        parsed.validate(PERSIST_FORMAT, ReplayCodec.id()).unwrap();
    }

    #[test]
    fn fixture_tail_restores_equal_screen() {
        let rec = fixture_pane();
        let mut live = fresh_emulator(80, 24);
        live.set_cell_pixels(8, 16);
        let _ = live.feed(FIXTURE);
        let _ = live.take_pending_replies();
        let restored = restore_emulator(&rec, &ReplayCodec).unwrap();
        assert_eq!(restored.screen(), live.screen());
    }

    #[test]
    fn corrupt_bytes_are_corrupt() {
        assert_eq!(parse_persist_bytes(b"not-json{"), Err(RestoreFail::Corrupt));
        assert_eq!(parse_persist_bytes(b"{}"), Err(RestoreFail::Corrupt));
    }

    #[test]
    fn version_mismatch_is_a_reset() {
        let file = PersistFile {
            format: 99,
            codec: ReplayCodec.id().into(),
            panes: Vec::new(),
        };
        let bytes = serde_json::to_vec(&file).unwrap();
        let parsed = parse_persist_bytes(&bytes).unwrap();
        assert_eq!(
            parsed.validate(PERSIST_FORMAT, ReplayCodec.id()),
            Err(RestoreFail::VersionMismatch { found: 99 })
        );
    }

    #[test]
    fn codec_mismatch_is_a_reset() {
        let file = PersistFile {
            format: PERSIST_FORMAT,
            codec: "pt-114-v1".into(),
            panes: Vec::new(),
        };
        assert_eq!(
            file.validate(PERSIST_FORMAT, ReplayCodec.id()),
            Err(RestoreFail::CodecMismatch {
                found: "pt-114-v1".into()
            })
        );
        let replay = PersistFile::from_panes(Vec::new(), ReplayCodec.id());
        assert_eq!(
            replay.validate(PERSIST_FORMAT, EmulatorStateCodec.id()),
            Err(RestoreFail::CodecMismatch {
                found: ReplayCodec.id().into()
            })
        );
    }

    #[test]
    fn emulator_state_codec_omits_blob_off_boundary() {
        let mut live = fresh_emulator(80, 24);
        live.feed(b"HI\x1b[");
        let bytes = EmulatorStateCodec.export(&live).unwrap();
        assert!(bytes.is_empty(), "incomplete CSI must omit the snapshot");
        live.feed(b"0mOK");
        let bytes = EmulatorStateCodec.export(&live).unwrap();
        assert!(!bytes.is_empty(), "ground state must export a snapshot");
    }

    #[test]
    fn emulator_state_codec_omits_blob_while_graphics_pending() {
        let mut live = fresh_emulator(80, 24);
        live.feed(b"\x1b_Ga=T,m=1;AAAA");
        let bytes = EmulatorStateCodec.export(&live).unwrap();
        assert!(
            bytes.is_empty(),
            "pending graphics upload must omit the snapshot"
        );
    }

    #[test]
    fn emulator_state_snapshot_restores_without_tail() {
        let mut live = fresh_emulator(80, 24);
        live.set_cell_pixels(8, 16);
        live.feed(b"EARLY\r\nLATE\r\n");
        let _ = live.take_pending_replies();
        let snapshot = EmulatorStateCodec.export(&live).unwrap();
        assert!(!snapshot.is_empty());
        let rec = PersistPane {
            session: "default".into(),
            pane_index: 0,
            snapshot_seq: 99,
            snapshot: Some(snapshot),
            cols: 80,
            rows: 24,
            cell_px: (8, 16),
            tail: Vec::new(),
        };
        let restored = restore_emulator(&rec, &EmulatorStateCodec).unwrap();
        assert_eq!(restored.screen(), live.screen());
    }

    #[test]
    fn emulator_state_bad_snapshot_replays_tail() {
        let mut expected = fresh_emulator(80, 24);
        expected.set_cell_pixels(8, 16);
        expected.feed(b"TAIL_OK\r\n");
        let _ = expected.take_pending_replies();
        let rec = PersistPane {
            session: "default".into(),
            pane_index: 0,
            snapshot_seq: 99,
            snapshot: Some(b"not-emulator-state".to_vec()),
            cols: 80,
            rows: 24,
            cell_px: (8, 16),
            tail: vec![PaneLogFrame {
                seq: 1,
                event: PaneEvent::Output {
                    bytes: b"TAIL_OK\r\n".to_vec(),
                },
            }],
        };
        let restored = restore_emulator(&rec, &EmulatorStateCodec).unwrap();
        assert_eq!(restored.screen(), expected.screen());
    }

    #[test]
    fn emulator_state_size_mismatch_replays_tail() {
        let mut live = fresh_emulator(80, 24);
        live.feed(b"X");
        let snapshot = EmulatorStateCodec.export(&live).unwrap();
        let mut expected = fresh_emulator(40, 24);
        expected.set_cell_pixels(8, 16);
        expected.feed(b"TAIL_OK\r\n");
        let _ = expected.take_pending_replies();
        let rec = PersistPane {
            session: "default".into(),
            pane_index: 0,
            snapshot_seq: 99,
            snapshot: Some(snapshot),
            cols: 40,
            rows: 24,
            cell_px: (8, 16),
            tail: vec![PaneLogFrame {
                seq: 1,
                event: PaneEvent::Output {
                    bytes: b"TAIL_OK\r\n".to_vec(),
                },
            }],
        };
        let restored = restore_emulator(&rec, &EmulatorStateCodec).unwrap();
        assert_eq!(restored.screen(), expected.screen());
    }

    #[test]
    fn instance_socket_stem_maps_default_and_named() {
        assert_eq!(instance_from_socket_name("pmux.sock"), Some("default"));
        assert_eq!(instance_from_socket_name("pmux-work.sock"), Some("work"));
        assert_eq!(instance_from_socket_name("other.sock"), None);
    }

    #[test]
    fn persist_path_is_instance_keyed_only_in_the_socket_dir() {
        let data = PathBuf::from("/data/prismattyc");
        let dirs = [PathBuf::from("/run/user/1000/prismattyc")];
        assert_eq!(
            resolve_pane_log_path_from(
                Path::new("/run/user/1000/prismattyc/pmux.sock"),
                None,
                Some(data.clone()),
                &dirs,
            ),
            Some(PathBuf::from("/data/prismattyc/pane-log-default.json"))
        );
        assert_eq!(
            resolve_pane_log_path_from(
                Path::new("/run/user/1000/prismattyc/pmux-work.sock"),
                None,
                Some(data.clone()),
                &dirs,
            ),
            Some(PathBuf::from("/data/prismattyc/pane-log-work.json"))
        );
        assert_eq!(
            resolve_pane_log_path_from(
                Path::new("/tmp/ad-hoc.sock"),
                None,
                Some(data.clone()),
                &dirs,
            ),
            None
        );
        assert_eq!(
            resolve_pane_log_path_from(
                Path::new("/run/user/1000/prismattyc/pmux.sock"),
                Some("off"),
                Some(data.clone()),
                &dirs,
            ),
            None
        );
        assert_eq!(
            resolve_pane_log_path_from(
                Path::new("/tmp/ad-hoc.sock"),
                Some("/tmp/explicit.json"),
                Some(data),
                &dirs,
            ),
            Some(PathBuf::from("/tmp/explicit.json"))
        );
    }

    #[test]
    fn reset_event_is_one_output_line() {
        match reset_output_event("corrupt persist") {
            PaneEvent::Output { bytes } => {
                let text = String::from_utf8(bytes).unwrap();
                assert!(text.starts_with(RESET_LINE));
                assert!(text.contains("corrupt persist"));
                assert!(text.ends_with("\r\n"));
            }
            other => panic!("{other:?}"),
        }
    }
}
