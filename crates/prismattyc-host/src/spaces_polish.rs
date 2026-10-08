//! Space preferences, visible save state, and guarded undo of membership edits.
use super::*;

#[derive(Default)]
pub(super) struct State {
    pub failed: bool,
    last_poll: Option<Instant>,
    pub changed: Option<(String, Instant)>,
    /// The rail has shown "Saving…" for this fingerprint. The next poll writes.
    pub(super) armed: bool,
    /// Autosave command is on the worker. A second write waits.
    pub(super) save_in_flight: bool,
    pub undo: Option<PathBuf>,
}

/// True once, when a decided write may start. A later call stays false
/// until [`note_save_finished`].
pub(super) fn claim_save(state: &mut State) -> bool {
    if state.save_in_flight {
        return false;
    }
    state.save_in_flight = true;
    true
}

pub(super) fn note_save_finished(state: &mut State, ok: bool) {
    state.save_in_flight = false;
    state.armed = false;
    if ok {
        state.failed = false;
        state.changed = None;
    } else {
        state.failed = true;
    }
}

/// Minimum gap between arrangement checks.
const POLL_GAP: Duration = Duration::from_millis(250);
/// Idle time after the last structural change before the save is armed.
const AUTOSAVE_AFTER: Duration = Duration::from_millis(750);

pub(super) fn undo_label(host: &HostState) -> &'static str {
    if host.space_polish.undo.is_some() {
        "Restore the previous membership; keep processes running"
    } else {
        "No removal or move to undo in this window"
    }
}

pub(super) fn undo_path(host: &HostState) -> PathBuf {
    let base = host
        .attach_layout_path
        .clone()
        .unwrap_or_else(|| config::config_path().with_file_name("window"));
    base.with_extension(format!("undo-{}.json", std::process::id()))
}

pub(super) fn undo(host: &mut HostState) {
    let Some(path) = host.space_polish.undo.clone() else {
        rail_error_toast(host, " Nothing to undo ");
        return;
    };
    match run_pmux_space(&[
        "space".into(),
        "undo".into(),
        path.to_string_lossy().into_owned(),
    ]) {
        Ok(()) => {
            host.space_polish.undo = None;
            host.last_space_refresh = None;
            host.observed_space_sessions.clear();
            refresh_rail(host);
            rail_toast(host, " Membership restored; sessions kept running ");
        }
        Err(error) => rail_error_toast(host, &format!(" Could not undo: {error} ")),
    }
}

/// One host tab the way a space file stores it: session names, and the
/// split tree with those names on the leaves.
#[derive(Debug, Clone, PartialEq)]
struct TabSnap {
    title: String,
    sessions: Vec<String>,
    layout: Option<prismattyc_mux::attach_tabs::TabLayoutNode>,
}

fn name_of(file: &attach_tabs::AttachTabsFile, id: &str) -> String {
    file.session_names
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

/// Live attach cache as space-file snaps. Leaf ids become session names so
/// a saved file compares equal to the tree that produced it.
fn snaps_from_attach(file: &attach_tabs::AttachTabsFile) -> Vec<TabSnap> {
    file.tabs
        .iter()
        .map(|tab| {
            let sessions: Vec<String> = tab.sessions.iter().map(|id| name_of(file, id)).collect();
            let layout = tab.layout.as_ref().and_then(|node| {
                prismattyc_mux::attach_tabs::remap_layout(node, |id| Some(name_of(file, id)))
            });
            let layout = prismattyc_mux::attach_tabs::layout_for_sessions(layout, &sessions);
            TabSnap {
                title: tab.title.clone(),
                sessions,
                layout,
            }
        })
        .collect()
}

fn live_snaps(host: &HostState) -> Vec<TabSnap> {
    snaps_from_attach(&attach_records_from_live(host))
}

fn saved_snaps(space: &prismattyc_mux::SavedSpace) -> Vec<TabSnap> {
    if space.tabs.is_empty() {
        space
            .sessions
            .iter()
            .map(|session| TabSnap {
                title: session
                    .windows
                    .first()
                    .map(|window| window.title.clone())
                    .unwrap_or_else(|| session.name.clone()),
                sessions: vec![session.name.clone()],
                layout: None,
            })
            .collect()
    } else {
        space
            .tabs
            .iter()
            .map(|tab| TabSnap {
                title: tab.title.clone(),
                sessions: tab.sessions.clone(),
                layout: tab.layout.clone(),
            })
            .collect()
    }
}

fn node_shape(node: &prismattyc_mux::SavedNode) -> serde_json::Value {
    use prismattyc_mux::SavedNode;
    match node {
        SavedNode::Leaf { title, .. } => serde_json::json!({"title":title}),
        SavedNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            serde_json::json!({"axis":axis,"ratio":ratio,"first":node_shape(first),"second":node_shape(second)})
        }
    }
}

pub(crate) fn saved_shapes(
    space: &prismattyc_mux::SavedSpace,
    snapshot: Option<&prismattyc_mux::Snapshot>,
) -> (bool, String) {
    // No snapshot matches today's `live_snapshot() == None` result: skip the
    // write. A stopped daemon must not look like a layout change.
    let Some(snapshot) = snapshot else {
        return (true, String::new());
    };
    let mut fingerprint = String::new();
    let mut matches = true;
    for saved in &space.sessions {
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|s| s.name == saved.name && s.space_id == space.id)
        else {
            continue;
        };
        let layout = prismattyc_mux::from_snapshot(live);
        let live_shape = layout
            .windows
            .iter()
            .map(|w| (&w.title, node_shape(&w.root)))
            .collect::<Vec<_>>();
        fingerprint.push_str(&format!("{}:{live_shape:?}", live.id));
        matches &= saved
            .windows
            .iter()
            .map(|w| (&w.title, node_shape(&w.root)))
            .collect::<Vec<_>>()
            == live_shape;
    }
    (matches, fingerprint)
}

struct Decide {
    status: &'static str,
    write: bool,
}

/// `false` when this tick must not inspect or write the arrangement.
/// A blocked restore clears an armed write so a partial layout is not saved.
fn begin_poll(state: &mut State, now: Instant, blocked: bool) -> bool {
    if state
        .last_poll
        .is_some_and(|earlier| now.saturating_duration_since(earlier) < POLL_GAP)
    {
        return false;
    }
    state.last_poll = Some(now);
    if blocked {
        state.armed = false;
        return false;
    }
    true
}

fn decide(
    state: &mut State,
    now: Instant,
    autosave: bool,
    matches: bool,
    fingerprint: &str,
    paused: bool,
) -> Decide {
    if matches {
        state.changed = None;
        state.armed = false;
        return Decide {
            status: if state.failed { "Save failed" } else { "Saved" },
            write: false,
        };
    }
    if state
        .changed
        .as_ref()
        .is_none_or(|(old, _)| old != fingerprint)
    {
        state.changed = Some((fingerprint.to_string(), now));
        state.armed = false;
    }
    if state.failed {
        return Decide {
            status: "Save failed",
            write: false,
        };
    }
    if !autosave {
        state.armed = false;
        return Decide {
            status: "Unsaved changes",
            write: false,
        };
    }
    if paused {
        state.armed = false;
        return Decide {
            status: "Saving…",
            write: false,
        };
    }
    if state.armed {
        state.armed = false;
        return Decide {
            status: "Saving…",
            write: true,
        };
    }
    let idle = state
        .changed
        .as_ref()
        .map(|(_, started)| now.saturating_duration_since(*started))
        .unwrap_or_default();
    if idle >= AUTOSAVE_AFTER {
        state.armed = true;
    }
    Decide {
        status: "Saving…",
        write: false,
    }
}

/// True when a live member of this Space is neither attached nor a session
/// this window has already applied. Autosave must not release that member.
fn view_omits_unobserved_member(host: &HostState) -> bool {
    let Some(owner) = host.mux.space_id.as_deref() else {
        return false;
    };
    let Some(snapshot) = snapshot_client::snapshot_for_periodic(host.snapshot_client.as_deref())
    else {
        return false;
    };
    snapshot.sessions.iter().any(|session| {
        if session.space_id.as_deref() != Some(owner) {
            return false;
        }
        let id = session.id.to_string();
        let attached = host.attach_pane_sessions.values().any(|live| live == &id);
        !attached && !host.observed_space_sessions.contains(&id)
    })
}

pub(super) fn poll(host: &mut HostState) {
    let now = Instant::now();
    let blocked = host.restore_prompt.is_some() || host.space_opens.blocks_persist();
    if !begin_poll(&mut host.space_polish, now, blocked) {
        return;
    }
    let Some(name) = host.space_rail.current.clone() else {
        return;
    };
    let autosave = host.space_autosave_enabled;
    let paused = host.context_menu.is_some() || host.session_prompt.is_some();
    let loaded = host
        .space_poll_loaded
        .take()
        .filter(|(loaded_name, _)| loaded_name == &name);
    let Some((_, space)) = loaded else {
        let _ = host.space_client.submit_poll(spaces_dir(), name);
        return;
    };
    let step = match space {
        Some(space) if space.id == host.mux.space_id => {
            let live = live_snaps(host);
            let snap = snapshot_client::snapshot_for_periodic(host.snapshot_client.as_deref());
            let (shapes_match, shapes) = saved_shapes(&space, snap.as_ref());
            let fingerprint = format!("{name}:{live:?}:{shapes}");
            decide(
                &mut host.space_polish,
                now,
                autosave,
                live == saved_snaps(&space) && shapes_match,
                &fingerprint,
                paused,
            )
        }
        _ => Decide {
            status: "Save unavailable",
            write: false,
        },
    };
    let mut status = step.status;
    // A pane move assigns the session to this Space before the window
    // attaches it. The daemon id is not in `observed_space_sessions` yet.
    // Saving the current view would release that session.
    if step.write && !view_omits_unobserved_member(host) && claim_save(&mut host.space_polish) {
        persist_attach_layout_from_live(host);
        let view_path = host.attach_layout_path.clone();
        let queued = host.space_client.submit_save(space_client::SaveJob {
            pmux: pmux_bin(),
            name: name.clone(),
            view_path,
        });
        if !queued {
            host.space_polish.save_in_flight = false;
        }
        status = "Saving…";
    }
    if host.space_rail.save_status != status {
        host.space_rail.save_status = status.to_string();
        host.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn a_second_autosave_is_not_claimed_while_one_is_in_flight() {
        let mut state = State {
            changed: Some(("layout".into(), Instant::now())),
            ..State::default()
        };
        assert!(claim_save(&mut state));
        assert!(!claim_save(&mut state));
        note_save_finished(&mut state, false);
        assert!(state.failed);
        assert!(!state.save_in_flight);
        assert!(state.changed.is_some());
        assert!(claim_save(&mut state));
        note_save_finished(&mut state, true);
        assert!(!state.failed);
        assert!(!state.save_in_flight);
        assert!(state.changed.is_none());
    }

    use prismattyc_mux::{PaneId, SavedSpace, SavedSpaceTab, SAVED_SPACE_VERSION};

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn autosave_arms_after_the_idle_then_writes_on_the_next_poll() {
        let mut state = State::default();
        let t0 = Instant::now();
        assert!(begin_poll(&mut state, t0, false));
        let first = decide(&mut state, t0, true, false, "split-a", false);
        assert!(!first.write);
        assert_eq!(first.status, "Saving…");
        assert!(!state.armed);

        assert!(
            !begin_poll(&mut state, at(t0, 249), false),
            "poll gap holds"
        );
        assert!(begin_poll(&mut state, at(t0, 250), false));
        assert!(!decide(&mut state, at(t0, 250), true, false, "split-a", false).write);
        assert!(begin_poll(&mut state, at(t0, 500), false));
        assert!(!decide(&mut state, at(t0, 500), true, false, "split-a", false).write);

        let early = decide(&mut state, at(t0, 749), true, false, "split-a", false);
        assert!(!early.write);
        assert!(!state.armed, "749ms is still inside the debounce");

        assert!(begin_poll(&mut state, at(t0, 999), false));
        let arm = decide(&mut state, at(t0, 999), true, false, "split-a", false);
        assert!(!arm.write);
        assert!(state.armed, "the idle paints Saving… before the write");
        assert_eq!(arm.status, "Saving…");

        assert!(begin_poll(&mut state, at(t0, 1249), false));
        let write = decide(&mut state, at(t0, 1249), true, false, "split-a", false);
        assert!(write.write);
        assert!(!state.armed);
        assert_eq!(write.status, "Saving…");
    }

    #[test]
    fn a_new_arrangement_restarts_the_debounce() {
        let mut state = State::default();
        let t0 = Instant::now();
        assert!(begin_poll(&mut state, t0, false));
        decide(&mut state, t0, true, false, "ratio-0.50", false);
        assert!(begin_poll(&mut state, at(t0, 700), false));
        decide(&mut state, at(t0, 700), true, false, "ratio-0.25", false);
        assert!(!state.armed);
        assert!(begin_poll(&mut state, at(t0, 1000), false));
        let still = decide(&mut state, at(t0, 1000), true, false, "ratio-0.25", false);
        assert!(
            !still.write,
            "the burst must not write on the first fingerprint's clock"
        );
        assert!(begin_poll(&mut state, at(t0, 1450), false));
        let arm = decide(&mut state, at(t0, 1450), true, false, "ratio-0.25", false);
        assert!(!arm.write && state.armed);
        assert!(begin_poll(&mut state, at(t0, 1700), false));
        assert!(decide(&mut state, at(t0, 1700), true, false, "ratio-0.25", false).write);
    }

    #[test]
    fn restore_and_a_paused_menu_do_not_write() {
        let mut state = State::default();
        let t0 = Instant::now();
        assert!(begin_poll(&mut state, t0, false));
        decide(&mut state, t0, true, false, "tree", false);
        assert!(begin_poll(&mut state, at(t0, 800), false));
        decide(&mut state, at(t0, 800), true, false, "tree", false);
        assert!(state.armed);

        assert!(
            !begin_poll(&mut state, at(t0, 1100), true),
            "a restore in progress must not reach the write"
        );
        assert!(
            !state.armed,
            "the armed write is dropped while restore blocks"
        );

        assert!(begin_poll(&mut state, at(t0, 1400), false));
        let rearmed = decide(&mut state, at(t0, 1400), true, false, "tree", false);
        assert!(
            !rearmed.write,
            "restore must not be followed by an immediate overwrite"
        );
        assert!(state.armed);

        assert!(begin_poll(&mut state, at(t0, 1700), false));
        let paused = decide(&mut state, at(t0, 1700), true, false, "tree", true);
        assert!(!paused.write);
        assert!(!state.armed);
        assert_eq!(paused.status, "Saving…");
    }

    #[test]
    fn autosave_off_stays_unsaved_and_a_match_clears_the_timer() {
        let mut state = State::default();
        let t0 = Instant::now();
        assert!(begin_poll(&mut state, t0, false));
        let off = decide(&mut state, t0, false, false, "tree", false);
        assert_eq!(off.status, "Unsaved changes");
        assert!(!off.write);
        assert!(begin_poll(&mut state, at(t0, 2000), false));
        assert!(!decide(&mut state, at(t0, 2000), false, false, "tree", false).write);

        state.failed = true;
        let failed = decide(&mut state, at(t0, 2000), true, false, "tree", false);
        assert_eq!(failed.status, "Save failed");
        assert!(!failed.write);

        let saved = decide(&mut state, at(t0, 2000), true, true, "tree", false);
        assert_eq!(saved.status, "Save failed");
        assert!(state.changed.is_none());
        state.failed = false;
        let clean = decide(&mut state, at(t0, 2000), true, true, "tree", false);
        assert_eq!(clean.status, "Saved");
        assert!(!clean.write);
    }

    #[test]
    fn a_split_ratio_changes_the_arrangement_fingerprint() {
        let leaf = |name: &str| prismattyc_mux::attach_tabs::TabLayoutNode::Leaf {
            session: name.into(),
        };
        let split = |ratio: f64| TabSnap {
            title: "agents".into(),
            sessions: vec!["s1".into(), "s2".into()],
            layout: Some(prismattyc_mux::attach_tabs::TabLayoutNode::Split {
                axis: prismattyc_mux::AxisWire::Horizontal,
                ratio,
                first: Box::new(leaf("s1")),
                second: Box::new(leaf("s2")),
            }),
        };
        let wide = split(0.25);
        let even = split(0.5);
        assert_ne!(wide, even);
        assert_ne!(format!("{wide:?}"), format!("{even:?}"));
        let reordered = TabSnap {
            sessions: vec!["s2".into(), "s1".into()],
            ..wide.clone()
        };
        assert_ne!(wide, reordered);
    }

    fn runtime() -> crate::mux::MuxRuntime {
        crate::mux::MuxRuntime::spawn("/bin/sleep", &["30".to_string()], 80, 24).unwrap()
    }

    fn fake_mux_bin() -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "pt161-fake-mux-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(prismattyc_mux::platform::executable_name("pmux"));
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn space_from_snaps(snaps: &[TabSnap]) -> SavedSpace {
        let mut sessions = Vec::new();
        for snap in snaps {
            for name in &snap.sessions {
                if sessions
                    .iter()
                    .any(|session: &prismattyc_mux::SavedSpaceSession| session.name == *name)
                {
                    continue;
                }
                sessions.push(prismattyc_mux::stub_space_session(name.clone()));
            }
        }
        SavedSpace {
            version: SAVED_SPACE_VERSION,
            id: Some("desk".into()),
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions,
            tabs: snaps
                .iter()
                .map(|snap| SavedSpaceTab {
                    title: snap.title.clone(),
                    sessions: snap.sessions.clone(),
                    layout: snap.layout.clone(),
                })
                .collect(),
            active_tab: 0,
            focused_session: None,
        }
    }

    /// Advance the synthetic clock the way [`poll`] does, and write only when
    /// the gate says the debounce has elapsed.
    fn save_when_due(
        state: &mut State,
        now: &mut Instant,
        fingerprint: &str,
        snaps: &[TabSnap],
        dir: &std::path::Path,
        name: &str,
    ) {
        let path = dir.join(format!("{name}.json"));
        let before = std::fs::read(&path).ok();
        for _ in 0..12 {
            if !begin_poll(state, *now, false) {
                *now += POLL_GAP;
                continue;
            }
            let step = decide(state, *now, true, false, fingerprint, false);
            if !step.write {
                assert_eq!(
                    std::fs::read(&path).ok(),
                    before,
                    "the space file must stay untouched until the debounce elapses"
                );
                *now += POLL_GAP;
                continue;
            }
            prismattyc_mux::save_space(dir, name, &space_from_snaps(snaps)).unwrap();
            state.changed = None;
            state.armed = false;
            return;
        }
        panic!("autosave did not write {fingerprint}");
    }

    fn reopen(saved: &SavedSpace, fake: &str) -> Vec<TabSnap> {
        use prismattyc_mux::attach_tabs::{AttachTabRecord, AttachTabsFile};
        let file = AttachTabsFile {
            tabs: saved
                .tabs
                .iter()
                .map(|tab| AttachTabRecord {
                    title: tab.title.clone(),
                    sessions: tab.sessions.clone(),
                    layout: tab.layout.clone(),
                })
                .collect(),
            ..AttachTabsFile::default()
        };
        let mut fresh = runtime();
        let mut panes = HashMap::new();
        crate::regroup::apply(&mut fresh, &mut panes, &file, fake, &HashMap::new()).unwrap();
        snaps_from_attach(&crate::attach_tabs::records_from_runtime(&fresh, &panes))
    }

    #[test]
    fn pane_add_split_and_close_autosave_the_live_tree() {
        let fake = fake_mux_bin();
        let dir = std::env::temp_dir().join(format!(
            "pt161-autosave-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut mux = runtime();
        let first = mux.focused_id();
        mux.mark_attach_session(first, "s1".into(), "s1".into());
        let mut sessions: HashMap<PaneId, String> = HashMap::new();
        sessions.insert(first, "s1".into());

        let mut gate = State::default();
        let mut now = Instant::now();

        let live = crate::attach_tabs::records_from_runtime(&mux, &sessions);
        let added = snaps_from_attach(&live);
        assert_eq!(added.len(), 1);
        assert!(
            added[0].layout.is_none(),
            "a single pane stores no split tree"
        );
        assert_eq!(added[0].sessions, ["s1"]);
        save_when_due(&mut gate, &mut now, "add", &added, &dir, "desk");
        let saved = prismattyc_mux::load_space(&dir, "desk").unwrap();
        assert_eq!(saved_snaps(&saved), added);
        assert_eq!(
            reopen(&saved, &fake),
            added,
            "reopen restores the added pane"
        );

        let second = mux
            .split_focused(
                "/bin/sleep",
                &["30".to_string()],
                prismattyc_mux::Axis::Horizontal,
                0.25,
            )
            .unwrap();
        mux.mark_attach_session(second, "s2".into(), "s2".into());
        sessions.insert(second, "s2".into());
        let split = snaps_from_attach(&crate::attach_tabs::records_from_runtime(&mux, &sessions));
        match split[0].layout.as_ref() {
            Some(prismattyc_mux::attach_tabs::TabLayoutNode::Split { axis, ratio, .. }) => {
                assert_eq!(*axis, prismattyc_mux::AxisWire::Horizontal);
                assert!((*ratio - 0.25).abs() < 1e-9);
            }
            other => panic!("split must record a tree, got {other:?}"),
        }
        save_when_due(&mut gate, &mut now, "split", &split, &dir, "desk");
        let saved = prismattyc_mux::load_space(&dir, "desk").unwrap();
        assert_eq!(saved_snaps(&saved), split);
        let raw = std::fs::read_to_string(dir.join("desk.json")).unwrap();
        assert!(
            raw.contains("\"horizontal\"") && raw.contains("0.25"),
            "saved arrangement fixture:\n{raw}"
        );
        println!("AUTOSAVE_FIXTURE\n{raw}");
        assert_eq!(reopen(&saved, &fake), split, "reopen restores the split");

        let closing = mux.focused_id();
        assert!(mux.close_focused().unwrap());
        sessions.remove(&closing);
        let closed = snaps_from_attach(&crate::attach_tabs::records_from_runtime(&mux, &sessions));
        assert!(
            closed[0].layout.is_none(),
            "closing the split pane drops the tree"
        );
        assert_eq!(closed[0].sessions, ["s1"]);
        save_when_due(&mut gate, &mut now, "close", &closed, &dir, "desk");
        let saved = prismattyc_mux::load_space(&dir, "desk").unwrap();
        assert_eq!(saved_snaps(&saved), closed);
        let raw = std::fs::read_to_string(dir.join("desk.json")).unwrap();
        assert!(
            !raw.contains("\"kind\": \"split\""),
            "close must replace the split, not leave it behind:\n{raw}"
        );
        assert_eq!(
            reopen(&saved, &fake),
            closed,
            "reopen restores the closed tree"
        );

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "atomic save left {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
