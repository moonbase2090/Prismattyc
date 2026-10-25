// SPDX-License-Identifier: MIT
//! Daemon-side autosave of Space window layouts (PT-56).
//!
//! The desktop host saves the Space it shows. Nothing saved a layout change
//! made without a host window (pane titles, splits, closed panes), so a
//! daemon restart reopened the older arrangement. This worker rewrites the
//! per-session `windows` of a Space file when the live shape differs, and
//! drops a session that no longer exists. A session that moved to another
//! Space, a member the file does not list, and a Space whose last `space
//! open` did not finish are left alone. Tab order and focus belong to the
//! host.
use crate::attach_tabs::{layout_for_sessions, remap_layout};
use crate::control::{ControlPlane, Snapshot};
use crate::layout_file::{
    from_sessions, list_spaces, load_space, save_space, spaces_dir_for_socket, SavedNode,
    SavedSpace, SavedWindow,
};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// A changed shape must hold for two ticks (one second) before it is written.
const TICK: Duration = Duration::from_millis(500);

fn shape(node: &SavedNode) -> serde_json::Value {
    match node {
        SavedNode::Leaf { title, .. } => serde_json::json!({ "title": title }),
        SavedNode::Split {
            axis,
            ratio,
            first,
            second,
        } => serde_json::json!({
            "axis": axis,
            "ratio": ratio,
            "first": shape(first),
            "second": shape(second),
        }),
    }
}

/// Window titles and split trees. Working directories and foreground
/// commands change constantly and do not count as a layout change.
fn window_shapes(windows: &[SavedWindow]) -> String {
    let shapes: Vec<_> = windows
        .iter()
        .map(|window| serde_json::json!([window.title, shape(&window.root)]))
        .collect();
    serde_json::Value::Array(shapes).to_string()
}

/// How a session that is in the file but gone from the daemon is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closes {
    /// Drop it, but never write a Space with no sessions.
    Drop,
    /// Drop it, even the last one: the daemon saw this Space in use.
    DropAll,
}

/// What the live daemon says about one Space file.
#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    /// The live sessions are not the file's sessions: one moved to another
    /// Space, a member is not listed in the file, or nothing is left and
    /// the daemon never saw the Space in use.
    Mismatch,
    /// Same members, same window shapes.
    InSync,
    /// The file with the live windows and without sessions that no longer
    /// exist, and a fingerprint of the change.
    Changed(Box<SavedSpace>, String),
}

pub(crate) fn plan(saved: &SavedSpace, snapshot: &Snapshot, closes: Closes) -> Plan {
    let Some(id) = saved.id.as_deref() else {
        return Plan::Mismatch;
    };
    let mut next = saved.clone();
    next.sessions.clear();
    let mut closed: Vec<&str> = Vec::new();
    let mut fingerprint = String::new();
    for entry in &saved.sessions {
        let Some(live) = snapshot.sessions.iter().find(|s| s.name == entry.name) else {
            closed.push(&entry.name);
            fingerprint.push_str(&format!("closed:{};", entry.name));
            continue;
        };
        if live.space_id.as_deref() != Some(id) || live.windows.is_empty() {
            return Plan::Mismatch;
        }
        let Some(windows) = from_sessions(&[live]).sessions.pop().map(|s| s.windows) else {
            return Plan::Mismatch;
        };
        let mut kept = entry.clone();
        let live_shape = window_shapes(&windows);
        if live_shape != window_shapes(&entry.windows) {
            fingerprint.push_str(&format!("{}:{live_shape};", entry.name));
            kept.windows = windows;
        }
        next.sessions.push(kept);
    }
    let unlisted = snapshot.sessions.iter().any(|live| {
        live.space_id.as_deref() == Some(id) && !saved.sessions.iter().any(|e| e.name == live.name)
    });
    if unlisted || (next.sessions.is_empty() && closes != Closes::DropAll) {
        return Plan::Mismatch;
    }
    if fingerprint.is_empty() {
        return Plan::InSync;
    }
    if !closed.is_empty() {
        for tab in &mut next.tabs {
            tab.sessions.retain(|name| !closed.contains(&name.as_str()));
            let layout = tab.layout.take().and_then(|node| {
                remap_layout(&node, |name| {
                    (!closed.contains(&name)).then(|| name.to_string())
                })
            });
            tab.layout = layout_for_sessions(layout, &tab.sessions);
        }
        next.tabs.retain(|tab| !tab.sessions.is_empty());
        if next.active_tab >= next.tabs.len() {
            next.active_tab = 0;
        }
        if next
            .focused_session
            .as_deref()
            .is_some_and(|name| closed.contains(&name))
        {
            next.focused_session = None;
        }
    }
    Plan::Changed(Box::new(next), fingerprint)
}

/// True when the live daemon holds exactly the file's sessions and window
/// shapes. `pmux space open` uses this to tell a finished retry from one that
/// found a half-built session and skipped it.
pub fn restore_matches(saved: &SavedSpace, snapshot: &Snapshot) -> bool {
    plan(saved, snapshot, Closes::Drop) == Plan::InSync
}

/// `[spaces] autosave`, else the legacy `space_autosave`, else on. The host
/// reads the same keys.
fn autosave_enabled(config: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(config) else {
        return true;
    };
    let Ok(table) = raw.parse::<toml::Table>() else {
        return true;
    };
    table
        .get("spaces")
        .and_then(|spaces| spaces.get("autosave"))
        .and_then(toml::Value::as_bool)
        .or_else(|| table.get("space_autosave").and_then(toml::Value::as_bool))
        .unwrap_or(true)
}

fn transfer_pending(dir: &Path) -> bool {
    dir.join(".ownership-transaction").exists()
}

/// `pmux space open` writes this before it builds a Space and removes it
/// only after every session and title is in place, so a restore that failed
/// or was killed keeps it. `pmux space save` removes it too.
pub fn restoring_marker(dir: &Path, space_id: &str) -> PathBuf {
    dir.join(format!(".restoring-{space_id}"))
}

fn restoring(dir: &Path, saved: &SavedSpace) -> bool {
    saved
        .id
        .as_deref()
        .is_some_and(|id| restoring_marker(dir, id).exists())
}

/// What the worker remembers between passes.
#[derive(Default)]
struct State {
    /// Fingerprint seen on the previous pass for each Space name. A change is
    /// written when the same fingerprint is seen twice.
    pending: HashMap<String, String>,
}

/// The daemon's view for one pass.
struct Live {
    snapshot: Snapshot,
    /// Space ids that have owned a session since the daemon started. The
    /// plane records this when a session is moved into a Space, so a Space
    /// opened and closed between two ticks is still known.
    owned: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Debounced: a change is written when it holds for two ticks.
    Tick,
    /// The daemon is shutting down. The worker is stopped before the server
    /// and its sessions are torn down, so the live view is still the user's.
    /// Write what has changed now, without the debounce.
    Final,
}

fn closes(live: &Live, saved: &SavedSpace) -> Closes {
    match saved.id.as_ref() {
        Some(id) if live.owned.contains(id) => Closes::DropAll,
        _ => Closes::Drop,
    }
}

fn sync(
    dir: &Path,
    config: &Path,
    state: &mut State,
    pass: Pass,
    live: impl Fn() -> Result<Live>,
) -> Result<()> {
    if !autosave_enabled(config) || !dir.is_dir() || transfer_pending(dir) {
        state.pending.clear();
        return Ok(());
    }
    let now = live()?;
    let mut due = Vec::new();
    let mut seen = HashSet::new();
    for entry in list_spaces(dir)? {
        let Ok(saved) = load_space(dir, &entry.name) else {
            continue;
        };
        if restoring(dir, &saved) {
            continue;
        }
        let Plan::Changed(_, fingerprint) = plan(&saved, &now.snapshot, closes(&now, &saved))
        else {
            continue;
        };
        seen.insert(entry.name.clone());
        if pass == Pass::Final || state.pending.get(&entry.name) == Some(&fingerprint) {
            due.push((entry.name, fingerprint));
        } else {
            state.pending.insert(entry.name, fingerprint);
        }
    }
    state.pending.retain(|name, _| seen.contains(name));
    if due.is_empty() {
        return Ok(());
    }
    // Same lock as every `pmux space` edit, and `space open` holds it for the
    // whole restore. Take it before the second snapshot so a transfer or a
    // restore in flight cannot interleave with this write.
    let lock = crate::platform::private_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".ownership.lock"))
        .context("open Space ownership lock")?;
    crate::platform::lock_exclusive(&lock)?;
    if transfer_pending(dir) {
        return Ok(());
    }
    let now = live()?;
    for (name, fingerprint) in due {
        let Ok(saved) = load_space(dir, &name) else {
            continue;
        };
        if restoring(dir, &saved) {
            continue;
        }
        if let Plan::Changed(next, again) = plan(&saved, &now.snapshot, closes(&now, &saved)) {
            if again == fingerprint {
                save_space(dir, &name, &next)?;
                state.pending.remove(&name);
            }
        }
    }
    Ok(())
}

pub struct Worker {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn start(plane: Arc<Mutex<ControlPlane>>, socket: PathBuf) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pmux-space-autosave".into())
            .spawn(move || {
                let dir = spaces_dir_for_socket(&socket);
                let config = crate::prism_config_path();
                let mut state = State::default();
                let live = || -> Result<Live> {
                    let plane = plane
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    Ok(Live {
                        snapshot: plane
                            .snapshot()
                            .map_err(|error| anyhow::anyhow!("{error:?}"))?,
                        owned: plane.spaces_owned(),
                    })
                };
                let mut pass = Pass::Tick;
                loop {
                    if let Err(error) = sync(&dir, &config, &mut state, pass, live) {
                        eprintln!("pmuxd: Space autosave failed: {error:#}");
                    }
                    if pass == Pass::Final {
                        break;
                    }
                    if !matches!(
                        receiver.recv_timeout(TICK),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        pass = Pass::Final;
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        AxisWire, LayoutSnapshot, PaneGeometry, PaneInputLedger, PaneSnapshot, SessionSnapshot,
        SpawnSpec, WindowBounds, WindowSnapshot,
    };
    use std::collections::BTreeMap;

    const SPACE: &str = "0123456789abcdef0123456789abcdef";

    fn pane(id: u64, title: &str, cwd: &str) -> PaneSnapshot {
        PaneSnapshot {
            pane_write: None,
            id,
            title: title.into(),
            title_pinned: !title.is_empty(),
            controller_id: None,
            geometry: PaneGeometry {
                pane_id: id,
                col: 0,
                row: 0,
                cols: 40,
                rows: 24,
            },
            spawn: Some(SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec![],
                cwd: Some(cwd.into()),
                env: BTreeMap::new(),
            }),
            child_pid: None,
            mail: None,
            status: None,
            attention: None,
            mail_inject: None,
            ledger: PaneInputLedger {
                last_output_at_ms: None,
                focused: false,
                controller_id: None,
                last_controller_write_at_ms: None,
                last_write_ended_with_cr: false,
                dirty_input: false,
                last_input_at_ms: None,
            },
            size_owner: None,
        }
    }

    /// One window. A second pane is a 0.4 horizontal split.
    fn session(name: &str, space: Option<&str>, panes: &[(&str, &str)]) -> SessionSnapshot {
        let leaf = |index: usize| LayoutSnapshot::Leaf {
            pane_id: index as u64 + 1,
        };
        let layout = match panes.len() {
            1 => leaf(0),
            _ => LayoutSnapshot::Split {
                axis: AxisWire::Horizontal,
                ratio: 0.4,
                first: Box::new(leaf(0)),
                second: Box::new(leaf(1)),
            },
        };
        SessionSnapshot {
            space_id: space.map(str::to_string),
            id: 1,
            name: name.into(),
            agent_id: None,
            windows: vec![WindowSnapshot {
                id: 1,
                title: "main".into(),
                bounds: WindowBounds {
                    window_id: 1,
                    cols: 80,
                    rows: 24,
                },
                sync_input: false,
                layout,
                panes: panes
                    .iter()
                    .enumerate()
                    .map(|(index, (title, cwd))| pane(index as u64 + 1, title, cwd))
                    .collect(),
            }],
        }
    }

    fn snapshot(sessions: Vec<SessionSnapshot>) -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions,
        }
    }

    fn saved(sessions: &[SessionSnapshot]) -> SavedSpace {
        let refs: Vec<_> = sessions.iter().collect();
        let mut space = from_sessions(&refs);
        space.id = Some(SPACE.into());
        space.version = crate::layout_file::OWNED_SPACE_VERSION;
        space
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pmux-autosave-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn live_with(snapshot: &Snapshot, owned: &[&str]) -> Live {
        Live {
            snapshot: snapshot.clone(),
            owned: owned.iter().map(|id| id.to_string()).collect(),
        }
    }

    fn live_of(snapshot: &Snapshot) -> Live {
        let owned: Vec<_> = snapshot
            .sessions
            .iter()
            .filter_map(|s| s.space_id.as_deref())
            .collect();
        live_with(snapshot, &owned)
    }

    fn changed(plan: Plan) -> SavedSpace {
        match plan {
            Plan::Changed(space, _) => *space,
            other => panic!("expected a change, got {other:?}"),
        }
    }

    fn leaf_title(space: &SavedSpace) -> Option<String> {
        match &space.sessions[0].windows[0].root {
            SavedNode::Leaf { title, .. } => title.clone(),
            other => panic!("expected a leaf, got {other:?}"),
        }
    }

    #[test]
    fn a_new_pane_title_or_split_is_a_change_but_a_cwd_move_is_not() {
        let before = [session("a", Some(SPACE), &[("", "/one")])];
        let file = saved(&before);

        let cd = snapshot(vec![session("a", Some(SPACE), &[("", "/two")])]);
        assert_eq!(
            plan(&file, &cd, Closes::Drop),
            Plan::InSync,
            "cwd is not layout"
        );

        let titled = snapshot(vec![session("a", Some(SPACE), &[("build", "/one")])]);
        assert_eq!(
            leaf_title(&changed(plan(&file, &titled, Closes::Drop))).as_deref(),
            Some("build")
        );

        let split = snapshot(vec![session(
            "a",
            Some(SPACE),
            &[("", "/one"), ("", "/one")],
        )]);
        let next = changed(plan(&file, &split, Closes::Drop));
        assert!(matches!(
            &next.sessions[0].windows[0].root,
            SavedNode::Split { ratio, .. } if (*ratio - 0.4).abs() < 1e-9
        ));
        assert_eq!(next.tabs, file.tabs, "tabs stay with the host");
    }

    #[test]
    fn a_closed_session_is_dropped_but_a_moved_or_unlisted_one_blocks_the_save() {
        let before = [
            session("a", Some(SPACE), &[("", "/x")]),
            session("b", Some(SPACE), &[("", "/x")]),
        ];
        let mut file = saved(&before);
        file.tabs = vec![
            crate::layout_file::SavedSpaceTab {
                title: "a".into(),
                sessions: vec!["a".into()],
                layout: None,
            },
            crate::layout_file::SavedSpaceTab {
                title: "b".into(),
                sessions: vec!["b".into()],
                layout: None,
            },
        ];
        file.active_tab = 1;
        file.focused_session = Some("b".into());
        let kept = |name: &str, space: Option<&str>| session(name, space, &[("", "/x")]);

        let closed = snapshot(vec![kept("a", Some(SPACE))]);
        let next = changed(plan(&file, &closed, Closes::Drop));
        assert_eq!(next.sessions.len(), 1);
        assert_eq!(next.sessions[0].name, "a");
        assert_eq!(next.tabs.len(), 1);
        assert_eq!(next.active_tab, 0);
        assert_eq!(next.focused_session, None);

        let moved = snapshot(vec![kept("a", Some(SPACE)), kept("b", Some("other"))]);
        assert_eq!(
            plan(&file, &moved, Closes::Drop),
            Plan::Mismatch,
            "b is in another Space"
        );

        let unlisted = snapshot(vec![
            kept("a", Some(SPACE)),
            kept("b", Some(SPACE)),
            kept("c", Some(SPACE)),
        ]);
        assert_eq!(
            plan(&file, &unlisted, Closes::Drop),
            Plan::Mismatch,
            "c is not listed"
        );

        let renamed = snapshot(vec![kept("a", Some(SPACE)), kept("b2", Some(SPACE))]);
        assert_eq!(
            plan(&file, &renamed, Closes::Drop),
            Plan::Mismatch,
            "rename not yet saved"
        );

        let none = snapshot(vec![]);
        assert_eq!(
            plan(&file, &none, Closes::Drop),
            Plan::Mismatch,
            "never save an empty Space"
        );
    }

    #[test]
    fn the_file_is_rewritten_after_the_change_holds_one_tick_and_not_before() {
        let dir = scratch("debounce");
        let before = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        save_space(&dir, "desk", &saved(&before.sessions)).unwrap();
        let config = dir.join("missing.toml");
        let mut state = State::default();
        let live = snapshot(vec![session("a", Some(SPACE), &[("build", "/x")])]);

        sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        assert_eq!(leaf_title(&load_space(&dir, "desk").unwrap()), None);

        sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        let written = load_space(&dir, "desk").unwrap();
        assert_eq!(leaf_title(&written).as_deref(), Some("build"));
        assert_eq!(written.id.as_deref(), Some(SPACE));
        assert!(state.pending.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flapping_change_is_not_written() {
        let dir = scratch("flap");
        let before = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        save_space(&dir, "desk", &saved(&before.sessions)).unwrap();
        let config = dir.join("missing.toml");
        let mut state = State::default();
        for title in ["one", "two", "three"] {
            let live = snapshot(vec![session("a", Some(SPACE), &[(title, "/x")])]);
            sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        }
        assert_eq!(leaf_title(&load_space(&dir, "desk").unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_space_whose_restore_did_not_finish_is_not_overwritten() {
        // Two panes saved; the restore produced one. The marker `space open`
        // leaves behind until it finishes is what tells this from a close.
        let dir = scratch("restoring");
        let full = [session("a", Some(SPACE), &[("", "/x"), ("", "/x")])];
        save_space(&dir, "desk", &saved(&full)).unwrap();
        let config = dir.join("missing.toml");
        let partial = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        let marker = restoring_marker(&dir, SPACE);
        std::fs::write(&marker, "").unwrap();
        let mut state = State::default();
        for _ in 0..4 {
            sync(&dir, &config, &mut state, Pass::Tick, || {
                Ok(live_of(&partial))
            })
            .unwrap();
        }
        assert!(matches!(
            &load_space(&dir, "desk").unwrap().sessions[0].windows[0].root,
            SavedNode::Split { .. }
        ));

        // The next open finished and removed the marker; a later close saves.
        std::fs::remove_file(&marker).unwrap();
        for _ in 0..2 {
            sync(&dir, &config, &mut state, Pass::Tick, || {
                Ok(live_of(&partial))
            })
            .unwrap();
        }
        assert!(matches!(
            &load_space(&dir, "desk").unwrap().sessions[0].windows[0].root,
            SavedNode::Leaf { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_closed_session_leaves_the_file_after_one_tick_of_debounce() {
        let dir = scratch("closed");
        let both = [
            session("a", Some(SPACE), &[("", "/x")]),
            session("b", Some(SPACE), &[("", "/x")]),
        ];
        save_space(&dir, "desk", &saved(&both)).unwrap();
        let config = dir.join("missing.toml");
        let live = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        let mut state = State::default();
        sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        assert_eq!(load_space(&dir, "desk").unwrap().sessions.len(), 2);
        sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        assert_eq!(load_space(&dir, "desk").unwrap().sessions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_the_last_session_is_saved_but_an_unopened_space_is_not_emptied() {
        let dir = scratch("last");
        let one = [session("a", Some(SPACE), &[("", "/x")])];
        save_space(&dir, "desk", &saved(&one)).unwrap();
        let config = dir.join("missing.toml");
        let none = snapshot(vec![]);

        // A daemon that just started and never saw the Space: leave the file.
        let mut fresh = State::default();
        for _ in 0..4 {
            sync(&dir, &config, &mut fresh, Pass::Tick, || {
                Ok(live_with(&none, &[]))
            })
            .unwrap();
        }
        assert_eq!(load_space(&dir, "desk").unwrap().sessions.len(), 1);

        // The Space was seen in use, then its only session went away.
        let mut state = State::default();
        let live = snapshot(one.to_vec());
        sync(&dir, &config, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        for _ in 0..2 {
            sync(&dir, &config, &mut state, Pass::Tick, || {
                Ok(live_with(&none, &[SPACE]))
            })
            .unwrap();
        }
        let emptied = load_space(&dir, "desk").unwrap();
        assert!(emptied.sessions.is_empty() && emptied.tabs.is_empty());
        assert_eq!(emptied.id.as_deref(), Some(SPACE), "identity is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_half_built_session_does_not_count_as_a_finished_restore() {
        let full = [session("a", Some(SPACE), &[("", "/x"), ("", "/x")])];
        let file = saved(&full);
        let partial = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        assert!(!restore_matches(&file, &partial));
        assert!(restore_matches(&file, &snapshot(full.to_vec())));
    }

    #[test]
    fn a_space_opened_and_closed_between_ticks_is_still_a_close() {
        // The plane recorded the ownership, so the worker does not need to
        // have polled while the session was alive.
        let dir = scratch("quick");
        let one = [session("a", Some(SPACE), &[("", "/x")])];
        save_space(&dir, "desk", &saved(&one)).unwrap();
        let config = dir.join("missing.toml");
        let none = snapshot(vec![]);
        let mut state = State::default();
        for _ in 0..2 {
            sync(&dir, &config, &mut state, Pass::Tick, || {
                Ok(live_with(&none, &[SPACE]))
            })
            .unwrap();
        }
        assert!(load_space(&dir, "desk").unwrap().sessions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_writes_pending_changes_and_closes_at_once() {
        let dir = scratch("final");
        let both = [
            session("a", Some(SPACE), &[("", "/x")]),
            session("b", Some(SPACE), &[("", "/x")]),
        ];
        save_space(&dir, "desk", &saved(&both)).unwrap();
        let config = dir.join("missing.toml");
        let mut state = State::default();

        // A rename made less than one tick before shutdown is flushed.
        let renamed = snapshot(vec![
            session("a", Some(SPACE), &[("build", "/x")]),
            session("b", Some(SPACE), &[("", "/x")]),
        ]);
        sync(&dir, &config, &mut state, Pass::Final, || {
            Ok(live_of(&renamed))
        })
        .unwrap();
        let written = load_space(&dir, "desk").unwrap();
        assert!(matches!(
            &written.sessions[0].windows[0].root,
            SavedNode::Leaf { title: Some(t), .. } if t == "build"
        ));

        // A session stopped on purpose just before shutdown is a close.
        let stopped = snapshot(vec![session("a", Some(SPACE), &[("build", "/x")])]);
        sync(&dir, &config, &mut state, Pass::Final, || {
            Ok(live_with(&stopped, &[SPACE]))
        })
        .unwrap();
        assert_eq!(load_space(&dir, "desk").unwrap().sessions.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opt_out_and_a_pending_transfer_block_the_write() {
        let dir = scratch("blocked");
        let before = snapshot(vec![session("a", Some(SPACE), &[("", "/x")])]);
        save_space(&dir, "desk", &saved(&before.sessions)).unwrap();
        let on = dir.join("on.toml");
        let mut state = State::default();
        let live = snapshot(vec![session("a", Some(SPACE), &[("build", "/x")])]);
        let untouched = |dir: &Path| leaf_title(&load_space(dir, "desk").unwrap()).is_none();

        let off = dir.join("off.toml");
        std::fs::write(&off, "[spaces]\nautosave = false\n").unwrap();
        for _ in 0..3 {
            sync(&dir, &off, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        }
        assert!(untouched(&dir), "spaces.autosave = false is honored");

        let legacy = dir.join("legacy.toml");
        std::fs::write(&legacy, "space_autosave = false\n").unwrap();
        for _ in 0..3 {
            sync(&dir, &legacy, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        }
        assert!(untouched(&dir), "legacy space_autosave = false is honored");

        std::fs::write(dir.join(".ownership-transaction"), "{}").unwrap();
        for _ in 0..3 {
            sync(&dir, &on, &mut state, Pass::Tick, || Ok(live_of(&live))).unwrap();
        }
        assert!(untouched(&dir), "a pending transfer is never raced");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
