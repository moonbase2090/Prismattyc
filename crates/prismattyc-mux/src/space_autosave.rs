//! Daemon-side autosave of Space window layouts (PT-56).
//!
//! The desktop host saves the Space it shows. Nothing saved a layout change
//! made without a host window (pane titles, splits, closed panes), so a
//! daemon restart reopened the older arrangement. This worker rewrites the
//! per-session `windows` of a Space file when the live shape differs.
//! Membership, tabs and focus are owned by `pmux space` and the host and are
//! never touched here.
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

/// A changed shape must hold for two ticks before it is written.
const TICK: Duration = Duration::from_secs(1);

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

/// The Space with its sessions' windows replaced by the live ones, and a
/// fingerprint of the change. `None` when nothing differs, or when the live
/// daemon does not match the file one to one: a session that failed to
/// restore, or a member the file does not list yet, must not be saved over.
pub(crate) fn updated_space(
    saved: &SavedSpace,
    snapshot: &Snapshot,
) -> Option<(SavedSpace, String)> {
    let id = saved.id.as_deref()?;
    let owned: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id.as_deref() == Some(id))
        .collect();
    if owned.len() != saved.sessions.len() {
        return None;
    }
    let mut next = saved.clone();
    let mut fingerprint = String::new();
    for entry in &mut next.sessions {
        let live = owned.iter().find(|session| session.name == entry.name)?;
        if live.windows.is_empty() {
            return None;
        }
        let windows = from_sessions(&[*live]).sessions.pop()?.windows;
        let live_shape = window_shapes(&windows);
        if live_shape != window_shapes(&entry.windows) {
            fingerprint.push_str(&format!("{}:{live_shape};", entry.name));
            entry.windows = windows;
        }
    }
    (!fingerprint.is_empty()).then_some((next, fingerprint))
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

/// One pass. `pending` holds the fingerprint seen on the previous pass for
/// each Space; a Space is written when the same fingerprint is seen twice.
fn sync(
    dir: &Path,
    config: &Path,
    pending: &mut HashMap<String, String>,
    snapshot: impl Fn() -> Result<Snapshot>,
) -> Result<()> {
    if !autosave_enabled(config) || !dir.is_dir() || transfer_pending(dir) {
        pending.clear();
        return Ok(());
    }
    let live = snapshot()?;
    let mut due = Vec::new();
    let mut seen = HashSet::new();
    for entry in list_spaces(dir)? {
        let Ok(saved) = load_space(dir, &entry.name) else {
            continue;
        };
        let Some((_, fingerprint)) = updated_space(&saved, &live) else {
            continue;
        };
        seen.insert(entry.name.clone());
        if pending.get(&entry.name) == Some(&fingerprint) {
            due.push((entry.name, fingerprint));
        } else {
            pending.insert(entry.name, fingerprint);
        }
    }
    pending.retain(|name, _| seen.contains(name));
    if due.is_empty() {
        return Ok(());
    }
    // Same lock as every `pmux space` edit. Take it before the second
    // snapshot so a transfer in flight cannot interleave with this write.
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
    let live = snapshot()?;
    for (name, fingerprint) in due {
        let Ok(saved) = load_space(dir, &name) else {
            continue;
        };
        match updated_space(&saved, &live) {
            Some((next, again)) if again == fingerprint => {
                save_space(dir, &name, &next)?;
                pending.remove(&name);
            }
            _ => {}
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
                let mut pending = HashMap::new();
                while matches!(
                    receiver.recv_timeout(TICK),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let result = sync(&dir, &config, &mut pending, || {
                        plane
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .snapshot()
                            .map_err(|error| anyhow::anyhow!("{error:?}"))
                    });
                    if let Err(error) = result {
                        eprintln!("pmuxd: Space autosave failed: {error:#}");
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

    #[test]
    fn a_new_pane_title_or_split_is_a_change_but_a_cwd_move_is_not() {
        let before = [session("a", Some(SPACE), &[("", "/one")])];
        let file = saved(&before);

        let cd = snapshot(vec![session("a", Some(SPACE), &[("", "/two")])]);
        assert!(updated_space(&file, &cd).is_none(), "cwd is not layout");

        let titled = snapshot(vec![session("a", Some(SPACE), &[("build", "/one")])]);
        let (next, _) = updated_space(&file, &titled).expect("title change");
        assert!(matches!(
            &next.sessions[0].windows[0].root,
            SavedNode::Leaf { title: Some(t), .. } if t == "build"
        ));

        let split = snapshot(vec![session(
            "a",
            Some(SPACE),
            &[("", "/one"), ("", "/one")],
        )]);
        let (next, _) = updated_space(&file, &split).expect("split");
        assert!(matches!(
            &next.sessions[0].windows[0].root,
            SavedNode::Split { ratio, .. } if (*ratio - 0.4).abs() < 1e-9
        ));
        assert_eq!(next.tabs, file.tabs, "tabs stay with the host");
    }

    #[test]
    fn a_space_that_does_not_match_the_daemon_is_never_saved_over() {
        let before = [
            session("a", Some(SPACE), &[("", "/x")]),
            session("b", Some(SPACE), &[("", "/x")]),
        ];
        let file = saved(&before);
        let changed = |name: &str, space: Option<&str>| session(name, space, &[("t", "/x")]);

        let missing = snapshot(vec![changed("a", Some(SPACE))]);
        assert!(
            updated_space(&file, &missing).is_none(),
            "b did not restore"
        );

        let foreign = snapshot(vec![changed("a", Some(SPACE)), changed("b", Some("other"))]);
        assert!(updated_space(&file, &foreign).is_none(), "b left the Space");

        let extra = snapshot(vec![
            changed("a", Some(SPACE)),
            changed("b", Some(SPACE)),
            changed("c", Some(SPACE)),
        ]);
        assert!(updated_space(&file, &extra).is_none(), "c is not listed");

        let both = snapshot(vec![changed("a", Some(SPACE)), changed("b", Some(SPACE))]);
        assert!(updated_space(&file, &both).is_some());
    }

    #[test]
    fn the_file_is_rewritten_after_the_change_holds_one_tick_and_not_before() {
        let dir = scratch("debounce");
        let before = [session("a", Some(SPACE), &[("", "/x")])];
        save_space(&dir, "desk", &saved(&before)).unwrap();
        let config = dir.join("missing.toml");
        let live = snapshot(vec![session("a", Some(SPACE), &[("build", "/x")])]);
        let mut pending = HashMap::new();

        sync(&dir, &config, &mut pending, || Ok(live.clone())).unwrap();
        let first = load_space(&dir, "desk").unwrap();
        assert!(matches!(
            &first.sessions[0].windows[0].root,
            SavedNode::Leaf { title: None, .. }
        ));

        sync(&dir, &config, &mut pending, || Ok(live.clone())).unwrap();
        let second = load_space(&dir, "desk").unwrap();
        assert!(matches!(
            &second.sessions[0].windows[0].root,
            SavedNode::Leaf { title: Some(t), .. } if t == "build"
        ));
        assert!(pending.is_empty());

        let reopened = load_space(&dir, "desk").unwrap();
        assert_eq!(reopened.id.as_deref(), Some(SPACE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flapping_change_is_not_written() {
        let dir = scratch("flap");
        save_space(
            &dir,
            "desk",
            &saved(&[session("a", Some(SPACE), &[("", "/x")])]),
        )
        .unwrap();
        let config = dir.join("missing.toml");
        let mut pending = HashMap::new();
        for title in ["one", "two", "three"] {
            let live = snapshot(vec![session("a", Some(SPACE), &[(title, "/x")])]);
            sync(&dir, &config, &mut pending, || Ok(live.clone())).unwrap();
        }
        let saved = load_space(&dir, "desk").unwrap();
        assert!(matches!(
            &saved.sessions[0].windows[0].root,
            SavedNode::Leaf { title: None, .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opt_out_and_a_pending_transfer_block_the_write() {
        let dir = scratch("blocked");
        save_space(
            &dir,
            "desk",
            &saved(&[session("a", Some(SPACE), &[("", "/x")])]),
        )
        .unwrap();
        let live = snapshot(vec![session("a", Some(SPACE), &[("build", "/x")])]);
        let untouched = |dir: &Path| {
            matches!(
                &load_space(dir, "desk").unwrap().sessions[0].windows[0].root,
                SavedNode::Leaf { title: None, .. }
            )
        };

        let off = dir.join("off.toml");
        std::fs::write(&off, "[spaces]\nautosave = false\n").unwrap();
        let mut pending = HashMap::new();
        for _ in 0..3 {
            sync(&dir, &off, &mut pending, || Ok(live.clone())).unwrap();
        }
        assert!(untouched(&dir), "spaces.autosave = false is honored");

        let legacy = dir.join("legacy.toml");
        std::fs::write(&legacy, "space_autosave = false\n").unwrap();
        for _ in 0..3 {
            sync(&dir, &legacy, &mut pending, || Ok(live.clone())).unwrap();
        }
        assert!(untouched(&dir), "legacy space_autosave = false is honored");

        std::fs::write(dir.join(".ownership-transaction"), "{}").unwrap();
        let on = dir.join("on.toml");
        for _ in 0..3 {
            sync(&dir, &on, &mut pending, || Ok(live.clone())).unwrap();
        }
        assert!(untouched(&dir), "a pending transfer is never raced");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
