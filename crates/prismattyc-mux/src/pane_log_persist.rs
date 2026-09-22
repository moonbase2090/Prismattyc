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
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use prismattyc_emulator::{Emulator, EmulatorStateV1};
use serde::{Deserialize, Serialize};

use crate::mailbox::default_mail_db_path;
use crate::pane_log::{PaneEvent, PaneLogFrame};

/// On-disk format number. A mismatch starts empty with one reset line.
pub(crate) const PERSIST_FORMAT: u32 = 2;
/// How often a dirty log may be written while the daemon runs.
pub(crate) const PERSIST_CADENCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Pane id to log sequence and restore identity at the last capture.
pub(crate) type CaptureMarks = std::collections::HashMap<u64, (u64, String, usize)>;

/// State captured under the control lock. Encoding and disk I/O belong to the
/// worker, never to a request handler. Only one checkpoint may be in flight.
pub(crate) struct PendingPane {
    pub delta: Vec<PaneLogFrame>,
    pub id: u64,
    pub record: Option<PersistPane>,
    pub state: Option<EmulatorStateV1>,
}

pub(crate) struct PersistWorker {
    send: mpsc::SyncSender<(PathBuf, Vec<PendingPane>)>,
    done: mpsc::Receiver<(bool, bool)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PersistWorker {
    pub(crate) fn start() -> io::Result<Self> {
        let (send, jobs) = mpsc::sync_channel::<(PathBuf, Vec<PendingPane>)>(1);
        let (complete, done) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("pmux-checkpoint".into())
            .spawn(move || {
                let mut cached = std::collections::HashMap::<u64, PersistPane>::new();
                let mut journal_bytes = 0usize;
                while let Ok((path, pending)) = jobs.recv() {
                    let result = (|| -> Result<(), PersistError> {
                        let full = pending.iter().all(|p| p.record.is_some());
                        let ids: Vec<_> = pending.iter().map(|p| p.id).collect();
                        cached.retain(|id, _| ids.contains(id));
                        let mut updates = Vec::new();
                        for pane in pending {
                            if let Some(mut record) = pane.record {
                                if let Some(state) = pane.state {
                                    record.snapshot = Some(serde_json::to_vec(&state)?);
                                }
                                updates.push(JournalUpdate::Replace {
                                    pane: record.clone(),
                                });
                                cached.insert(pane.id, record);
                            } else if !pane.delta.is_empty() {
                                let record = cached.get_mut(&pane.id).ok_or_else(|| {
                                    PersistError::State("missing checkpoint cache".into())
                                })?;
                                updates.push(JournalUpdate::Append {
                                    session: record.session.clone(),
                                    pane_index: record.pane_index,
                                    frames: pane.delta.clone(),
                                });
                                record.tail.extend(pane.delta);
                            }
                        }
                        if full {
                            let panes = ids
                                .iter()
                                .map(|id| {
                                    cached.get(id).cloned().ok_or_else(|| {
                                        PersistError::State("missing checkpoint cache".into())
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            write_persist(
                                &path,
                                &PersistFile::from_panes(panes, EmulatorStateCodec.id()),
                            )?;
                            journal_bytes = 0;
                        } else {
                            let active = ids
                                .iter()
                                .map(|id| {
                                    cached
                                        .get(id)
                                        .map(|p| (p.session.clone(), p.pane_index))
                                        .ok_or_else(|| {
                                            PersistError::State("missing checkpoint cache".into())
                                        })
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            let transaction = JournalTransaction { active, updates };
                            let mut bytes = serde_json::to_vec(&transaction)?;
                            bytes.push(b'\n');
                            // A partial append is repaired by a fresh atomic snapshot on retry.
                            fs::OpenOptions::new()
                                .append(true)
                                .open(&path)?
                                .write_all(&bytes)?;
                            journal_bytes = journal_bytes.saturating_add(bytes.len());
                        }
                        Ok(())
                    })();
                    // Request fresh captures after bounded journal growth. An idle daemon
                    // need not compact until another event makes it dirty.
                    if complete
                        .send((result.is_ok(), journal_bytes >= JOURNAL_LIMIT))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            send,
            done,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, path: PathBuf, panes: Vec<PendingPane>) -> bool {
        self.send.try_send((path, panes)).is_ok()
    }

    pub(crate) fn completed(&self) -> Option<(bool, bool)> {
        self.done.try_recv().ok()
    }
}

impl Drop for PersistWorker {
    fn drop(&mut self) {
        // Disconnect before joining. Shutdown waits for the accepted checkpoint.
        let (closed, _) = mpsc::sync_channel(0);
        let sender = std::mem::replace(&mut self.send, closed);
        drop(sender);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
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

// Bound replay work and in-memory tails independently of session lifetime.
const JOURNAL_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct JournalTransaction {
    active: Vec<(String, usize)>,
    updates: Vec<JournalUpdate>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum JournalUpdate {
    Replace {
        pane: PersistPane,
    },
    Append {
        session: String,
        pane_index: usize,
        frames: Vec<PaneLogFrame>,
    },
}

impl JournalTransaction {
    fn apply(self, file: &mut PersistFile) -> Result<(), RestoreFail> {
        for update in self.updates {
            match update {
                JournalUpdate::Replace { pane } => {
                    file.panes.retain(|p| {
                        (p.session.as_str(), p.pane_index)
                            != (pane.session.as_str(), pane.pane_index)
                    });
                    file.panes.push(pane);
                }
                JournalUpdate::Append {
                    session,
                    pane_index,
                    frames,
                } => {
                    let pane = file
                        .panes
                        .iter_mut()
                        .find(|p| p.session == session && p.pane_index == pane_index)
                        .ok_or(RestoreFail::Corrupt)?;
                    pane.tail.extend(frames);
                }
            }
        }
        let mut ordered = Vec::with_capacity(self.active.len());
        for (session, index) in self.active {
            let position = file
                .panes
                .iter()
                .position(|p| p.session == session && p.pane_index == index)
                .ok_or(RestoreFail::Corrupt)?;
            ordered.push(file.panes.remove(position));
        }
        file.panes = ordered;
        Ok(())
    }
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

/// Read a legacy snapshot or a snapshot followed by committed event transactions.
pub(crate) fn parse_persist_bytes(bytes: &[u8]) -> Result<PersistFile, RestoreFail> {
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<PersistFile>();
    let mut file = stream
        .next()
        .ok_or(RestoreFail::Corrupt)?
        .map_err(|_| RestoreFail::Corrupt)?;
    let offset = stream.byte_offset();
    // Version 1 was a single JSON document. Upgrade it in memory only.
    if file.format == 1 {
        if !bytes[offset..].iter().all(u8::is_ascii_whitespace) {
            return Err(RestoreFail::Corrupt);
        }
        file.format = PERSIST_FORMAT;
    } else if file.format == PERSIST_FORMAT {
        for line in bytes[offset..].split_inclusive(|b| *b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            // A killed writer can leave an incomplete final transaction. Never
            // apply it, even if the prefix happens to be valid JSON.
            if line.last() != Some(&b'\n') {
                break;
            }
            let transaction: JournalTransaction =
                serde_json::from_slice(line).map_err(|_| RestoreFail::Corrupt)?;
            transaction.apply(&mut file)?;
        }
    }
    Ok(file)
}

pub(crate) fn write_persist(path: &Path, file: &PersistFile) -> Result<(), PersistError> {
    write_document(path, file)
}

fn write_document(path: &Path, file: &impl Serialize) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec(file)?;
    bytes.push(b'\n');
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
            let cols = usize::from(*cols).max(1);
            let rows = usize::from(*rows).max(1);
            if emulator.screen().columns() != cols || emulator.screen().rows() != rows {
                if *reflow {
                    emulator.resize(cols, rows);
                } else {
                    emulator.resize_legacy(cols, rows);
                }
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

    #[test]
    fn worker_reuses_unchanged_panes_removes_closed_panes_and_drains_on_drop() {
        let dir = std::env::temp_dir().join(format!("checkpoint-worker-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.json");
        let worker = PersistWorker::start().unwrap();
        let wait = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                if let Some((ok, _)) = worker.completed() {
                    assert!(ok);
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let original = fixture_pane();
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                delta: Vec::new(),
                id: 1,
                record: Some(original.clone()),
                state: None,
            }]
        ));
        wait();
        assert_eq!(read_persist(&path).unwrap().panes, vec![original.clone()]);
        let mut renamed = original.clone();
        renamed.session = "second".into();
        assert!(worker.submit(
            path.clone(),
            vec![
                PendingPane {
                    delta: Vec::new(),
                    id: 1,
                    record: None,
                    state: None
                },
                PendingPane {
                    delta: Vec::new(),
                    id: 2,
                    record: Some(renamed.clone()),
                    state: None
                },
            ]
        ));
        wait();
        assert_eq!(
            read_persist(&path).unwrap().panes,
            vec![original, renamed.clone()]
        );
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                delta: Vec::new(),
                id: 2,
                record: None,
                state: None
            }]
        ));
        drop(worker);
        assert_eq!(read_persist(&path).unwrap().panes, vec![renamed]);
        fs::remove_dir_all(dir).unwrap();
    }

    fn wait_worker(worker: &PersistWorker) -> (bool, bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(result) = worker.completed() {
                return result;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn incremental_checkpoint_preserves_base_and_restores_after_torn_append() {
        let dir = std::env::temp_dir().join(format!("checkpoint-delta-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.json");
        let worker = PersistWorker::start().unwrap();
        let original = fixture_pane();
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: Some(original.clone()),
                state: None,
                delta: vec![],
            }]
        ));
        assert_eq!(wait_worker(&worker), (true, false));
        let base = fs::read(&path).unwrap();
        let frame = PaneLogFrame {
            seq: original.tail.last().unwrap().seq + 1,
            event: PaneEvent::Output {
                bytes: b"\r\nAFTER CHECKPOINT".to_vec(),
            },
        };
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: None,
                state: None,
                delta: vec![frame.clone()],
            }]
        ));
        assert_eq!(wait_worker(&worker), (true, false));
        let saved = fs::read(&path).unwrap();
        assert!(
            saved.starts_with(&base),
            "steady output must not rewrite the snapshot"
        );
        assert!(
            saved.len() - base.len() < 1024,
            "one event must not rewrite history"
        );
        let mut expected = restore_emulator(&original, &EmulatorStateCodec).unwrap();
        replay_event(&mut expected, &frame.event);
        let restored = read_persist(&path).unwrap();
        assert_eq!(
            restore_emulator(&restored.panes[0], &EmulatorStateCodec)
                .unwrap()
                .export_state()
                .unwrap(),
            expected.export_state().unwrap()
        );
        // Exercise every possible truncation of the new transaction, including
        // complete JSON without its commit newline.
        for end in base.len()..saved.len() {
            assert_eq!(
                parse_persist_bytes(&saved[..end]).unwrap().panes,
                vec![original.clone()]
            );
        }
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: None,
                state: None,
                delta: vec![PaneLogFrame {
                    seq: frame.seq + 1,
                    event: PaneEvent::Output {
                        bytes: vec![b'x'; JOURNAL_LIMIT / 2]
                    }
                }],
            }]
        ));
        assert_eq!(
            wait_worker(&worker),
            (true, true),
            "large journals must request compaction"
        );
        // A fresh capture atomically replaces all prior journal transactions.
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: Some(original.clone()),
                state: None,
                delta: vec![],
            }]
        ));
        assert_eq!(wait_worker(&worker), (true, false));
        assert_eq!(fs::read(&path).unwrap(), base);
        // Failure must be visible; a retry with fresh captures repairs the file.
        fs::remove_file(&path).unwrap();
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: None,
                state: None,
                delta: vec![frame],
            }]
        ));
        assert_eq!(wait_worker(&worker), (false, false));
        assert!(worker.submit(
            path.clone(),
            vec![PendingPane {
                id: 1,
                record: Some(original.clone()),
                state: None,
                delta: vec![],
            }]
        ));
        assert_eq!(wait_worker(&worker), (true, false));
        assert_eq!(read_persist(&path).unwrap().panes, vec![original]);
        drop(worker);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_checkpoint_is_readable_and_complete_corrupt_journal_is_rejected() {
        let mut file = PersistFile::from_panes(vec![fixture_pane()], EmulatorStateCodec.id());
        file.format = 1;
        let bytes = serde_json::to_vec(&file).unwrap();
        let upgraded = parse_persist_bytes(&bytes).unwrap();
        upgraded
            .validate(PERSIST_FORMAT, EmulatorStateCodec.id())
            .unwrap();
        assert_eq!(upgraded.panes, file.panes);
        let mut bytes = serde_json::to_vec(&upgraded).unwrap();
        bytes.extend_from_slice(b"\n{bad journal}\n");
        assert_eq!(parse_persist_bytes(&bytes), Err(RestoreFail::Corrupt));
    }

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
    fn pixel_resize_tail_recovery_preserves_graphics() {
        let image = b"\x1b_Ga=T,t=d,f=100,i=7;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC\x1b\\";
        for reflow in [false, true] {
            let mut live = fresh_emulator(80, 24);
            live.feed(image);
            let graphics = live.export_state().unwrap().graphics;
            assert_eq!(graphics.images.len(), 1);
            live.set_cell_pixels(13, 27);
            live.feed(b"\x1b[");
            let snapshot = EmulatorStateCodec.export(&live).unwrap();
            assert!(snapshot.is_empty());
            let rec = PersistPane {
                session: "default".into(),
                pane_index: 0,
                snapshot_seq: 0,
                snapshot: Some(snapshot),
                cols: 80,
                rows: 24,
                cell_px: (13, 27),
                tail: vec![
                    PaneLogFrame {
                        seq: 1,
                        event: PaneEvent::Output {
                            bytes: image.to_vec(),
                        },
                    },
                    PaneLogFrame {
                        seq: 2,
                        event: PaneEvent::Resize {
                            cols: 80,
                            rows: 24,
                            cell_px: (13, 27),
                            size_owner: None,
                            reflow,
                        },
                    },
                    PaneLogFrame {
                        seq: 3,
                        event: PaneEvent::Output {
                            bytes: b"\x1b[".to_vec(),
                        },
                    },
                ],
            };
            let mut restored = restore_emulator(&rec, &EmulatorStateCodec).unwrap();
            restored.feed(b"0m");
            live.feed(b"0m");
            let state = restored.export_state().unwrap();
            assert_eq!(state.graphics, graphics);
            assert_eq!((state.cell_width_px, state.cell_height_px), (13, 27));
            assert_eq!(state, live.export_state().unwrap());
        }
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
