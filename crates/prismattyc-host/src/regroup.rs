//! Apply an attach-tabs file to a live host (PT-65, PT-213).
//!
//! Sessions already attached move (identity preserved). Sessions in the
//! file but not attached get a new pane via `pmux attach`. In `add` mode
//! (default), attached sessions absent from the file keep their tab. In
//! `switch` mode those host panes detach; the mux sessions stay in pmuxd.
//! Local (non-attach) panes are kept.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use prismattyc_mux::{PaneId, WindowId};

use crate::attach_tabs::{regroup_diff, AttachTabRecord, AttachTabsFile, AttachTabsMode};
use crate::mux::MuxRuntime;

/// Apply `file` to the live tabs. Returns whether any move or attach ran.
///
/// `names` maps a live session id to its stable name. The file references
/// sessions by their current (ephemeral) id; a live pane remembers the id
/// it attached under, which drifts when a session is respawned or the
/// daemon rebinds. Matching on the stable name, not the id, stops regroup
/// from re-attaching a session that is already live under a different id
/// (the open-side mirror of PT-108).
pub fn apply(
    mux: &mut MuxRuntime,
    pane_sessions: &mut HashMap<PaneId, String>,
    file: &AttachTabsFile,
    mux_bin: &str,
    names: &HashMap<String, String>,
) -> Result<bool> {
    apply_with_placeholders(mux, pane_sessions, file, mux_bin, names, &HashSet::new())
}

/// Missing saved seats are explicit placeholders, never failed PTY children.
pub fn apply_with_placeholders(
    mux: &mut MuxRuntime,
    pane_sessions: &mut HashMap<PaneId, String>,
    file: &AttachTabsFile,
    mux_bin: &str,
    names: &HashMap<String, String>,
    stopped: &HashSet<String>,
) -> Result<bool> {
    mux.unzoom()?;
    // Regroup identifies a session by its stable name, not the ephemeral id
    // a pane attached under. `file` names sessions by their current id;
    // resolve those ids to names, and read each live pane's name from its
    // mark, so a session already live under a drifted id is recognised as
    // present instead of re-attached (PT-108 open-side mirror).
    let mut id_of_name: HashMap<String, String> = HashMap::new();
    let file = named_file(file, names, &mut id_of_name);
    let live = live_session_tabs(mux, pane_sessions, names);
    let diff = regroup_diff(&live, &file);
    let mut changed = !diff.is_noop();

    let attach_id = |name: &str| {
        id_of_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    };

    let mut dummies: HashSet<PaneId> = HashSet::new();
    let mut move_dummies: HashSet<PaneId> = HashSet::new();
    // A reused window whose only pane is local (not an attach) is the launch
    // placeholder of a bare host. Once the regroup fills that window with
    // sessions the placeholder is closed like a dummy; it survives when the
    // window stays single-pane (PT-170). Collect before switch-detach so a
    // leftover local shell is not treated as that placeholder.
    for (_, panes) in mux.tab_panes() {
        if let [only] = panes.as_slice() {
            if !pane_sessions.contains_key(only) && !mux.is_retained_local_terminal(*only) {
                dummies.insert(*only);
            }
        }
    }
    if file.mode == AttachTabsMode::Switch && !diff.keep.is_empty() {
        for dummy in detach_sessions(mux, pane_sessions, names, &diff.keep)? {
            dummies.insert(dummy);
        }
        changed = true;
    }

    let mut dest: Vec<WindowId> = mux.window_ids();
    while dest.len() < file.tabs.len() {
        let index = dest.len();
        let attach_here = diff
            .attaches
            .iter()
            .find(|(_, dest_index)| *dest_index == index)
            .map(|(name, _)| name.clone());
        let window = if let Some(name) = attach_here {
            let id = attach_id(&name);
            let program = if stopped.contains(&name) {
                crate::mux::EMPTY_SPACE_PROGRAM
            } else {
                mux_bin
            };
            let window = mux.new_tab(program, &attach_args(&id))?;
            let pane = mux.focused_id();
            if stopped.contains(&name) {
                mux.saved_session_placeholder(pane, &name);
            }
            mux.mark_attach_session(pane, id.clone(), name);
            pane_sessions.insert(pane, id);
            window
        } else {
            let window = mux.new_tab("/bin/sh", &[])?;
            dummies.insert(mux.focused_id());
            window
        };
        dest.push(window);
    }

    let mut attached: HashSet<String> = HashSet::new();
    for (pane, id) in pane_sessions.iter() {
        attached.insert(session_name(mux, *pane, id, names));
    }

    for (name, dest_index) in &diff.attaches {
        if attached.contains(name) {
            continue;
        }
        let Some(&window) = dest.get(*dest_index) else {
            continue;
        };
        let id = attach_id(name);
        select_window(mux, window)?;
        let program = if stopped.contains(name) {
            crate::mux::EMPTY_SPACE_PROGRAM
        } else {
            mux_bin
        };
        let pane = mux.split_focused(
            program,
            &attach_args(&id),
            prismattyc_mux::Axis::Horizontal,
            0.5,
        )?;
        if stopped.contains(name) {
            mux.saved_session_placeholder(pane, name);
        }
        mux.mark_attach_session(pane, id.clone(), name.clone());
        pane_sessions.insert(pane, id);
        attached.insert(name.clone());
    }

    for (name, dest_index) in &diff.moves {
        let Some(&want) = dest.get(*dest_index) else {
            continue;
        };
        let Some(pane) = pane_sessions
            .iter()
            .find_map(|(pane, id)| (session_name(mux, *pane, id, names) == *name).then_some(*pane))
        else {
            continue;
        };
        let Some(src) = mux.pane_window(pane) else {
            continue;
        };
        if src == want {
            continue;
        }
        if mux.domain_pane_count(src).is_some_and(|count| count == 1) {
            select_window(mux, src)?;
            let dummy = mux.split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)?;
            dummies.insert(dummy);
            move_dummies.insert(dummy);
        }
        mux.move_pane_to_window(pane, want)?;
    }

    for (index, tab) in file.tabs.iter().enumerate() {
        if let Some(&window) = dest.get(index) {
            let _ = mux.rename_window(window, &tab.title);
        }
    }

    for dummy in dummies {
        if pane_sessions.contains_key(&dummy) {
            continue;
        }
        let Some(window) = mux.pane_window(dummy) else {
            continue;
        };
        // A move can leave its source tab holding only the temporary pane.
        // Close that tab after all moves finish. Other lone local panes and
        // the final tab keep the existing closer protection.
        if mux.domain_pane_count(window).is_some_and(|count| count > 1)
            || (move_dummies.contains(&dummy) && mux.tab_count() > 1)
        {
            select_window(mux, window)?;
            if mux.focus(dummy) {
                let _ = mux.close_focused();
            }
        }
    }

    seed_focus(mux, pane_sessions, &file, names)?;
    Ok(changed)
}

/// Live tab membership keyed by stable session name (not the id a pane
/// attached under).
fn live_session_tabs(
    mux: &MuxRuntime,
    pane_sessions: &HashMap<PaneId, String>,
    names: &HashMap<String, String>,
) -> Vec<(String, Vec<String>)> {
    mux.tab_panes()
        .into_iter()
        .map(|(title, panes)| {
            let sessions: Vec<String> = panes
                .into_iter()
                .filter_map(|pane| {
                    let id = pane_sessions.get(&pane)?;
                    Some(session_name(mux, pane, id, names))
                })
                .collect();
            (title, sessions)
        })
        .collect()
}

/// The stable name of a live attach pane: its recorded name if the mark
/// carries one, else the id resolved through `names`, else the raw id.
fn session_name(
    mux: &MuxRuntime,
    pane: PaneId,
    id: &str,
    names: &HashMap<String, String>,
) -> String {
    if names.is_empty() {
        // The snapshot failed: `named_file` kept the file's ids, so the live
        // side must use ids too. Preferring the recorded name here would
        // match nothing and re-attach every live session.
        return id.to_string();
    }
    mux.attach_name_of(pane)
        .map(str::to_string)
        .unwrap_or_else(|| resolve_name(names, id))
}

/// Map a session id to its stable name, falling back to the id itself when
/// the snapshot does not list it (an empty `names` keeps id matching).
fn resolve_name(names: &HashMap<String, String>, id: &str) -> String {
    names.get(id).cloned().unwrap_or_else(|| id.to_string())
}

/// A copy of `file` with every session id replaced by its stable name.
/// `id_of_name` collects the reverse map so an attach still spawns
/// `pmux attach --session-id <id>` for the session's current id.
fn named_file(
    file: &AttachTabsFile,
    names: &HashMap<String, String>,
    id_of_name: &mut HashMap<String, String>,
) -> AttachTabsFile {
    let tabs = file
        .tabs
        .iter()
        .map(|tab| {
            let sessions = tab
                .sessions
                .iter()
                .map(|id| {
                    let name = resolve_name(names, id);
                    id_of_name.entry(name.clone()).or_insert_with(|| id.clone());
                    name
                })
                .collect();
            AttachTabRecord {
                title: tab.title.clone(),
                sessions,
            }
        })
        .collect();
    AttachTabsFile {
        tabs,
        active_tab: file.active_tab,
        focused_session: file
            .focused_session
            .as_ref()
            .map(|id| resolve_name(names, id)),
        space: file.space.clone(),
        mode: file.mode,
        ..file.clone()
    }
}

fn attach_args(session: &str) -> Vec<String> {
    vec!["attach".into(), "--session-id".into(), session.to_string()]
}

/// Close host attach panes for sessions the switch file does not name.
/// Mux sessions stay alive. Local shells are not in `pane_sessions`.
fn detach_sessions(
    mux: &mut MuxRuntime,
    pane_sessions: &mut HashMap<PaneId, String>,
    names: &HashMap<String, String>,
    keep: &[String],
) -> Result<Vec<PaneId>> {
    let want: HashSet<&str> = keep.iter().map(String::as_str).collect();
    let targets: Vec<PaneId> = pane_sessions
        .iter()
        .filter(|(pane, id)| want.contains(session_name(mux, **pane, id, names).as_str()))
        .map(|(pane, _)| *pane)
        .collect();
    let mut created = Vec::new();
    for pane in targets {
        if let Some(dummy) = detach_attach_pane(mux, pane_sessions, pane)? {
            created.push(dummy);
        }
    }
    Ok(created)
}

fn detach_attach_pane(
    mux: &mut MuxRuntime,
    pane_sessions: &mut HashMap<PaneId, String>,
    pane: PaneId,
) -> Result<Option<PaneId>> {
    let Some(window) = mux.pane_window(pane) else {
        pane_sessions.remove(&pane);
        return Ok(None);
    };
    let last_tab = mux.tab_count() == 1;
    let last_in_window = mux
        .domain_pane_count(window)
        .is_some_and(|count| count == 1);
    let dummy = if last_tab && last_in_window {
        mux.empty_space_view(pane)?;
        pane_sessions.remove(&pane);
        return Ok(Some(pane));
    } else {
        None
    };
    select_window(mux, window)?;
    if mux.focus(pane) {
        let _ = mux.close_focused()?;
    }
    pane_sessions.remove(&pane);
    Ok(dummy)
}

fn select_window(mux: &mut MuxRuntime, window: WindowId) -> Result<()> {
    let ids = mux.window_ids();
    if let Some(index) = ids.iter().position(|id| *id == window) {
        let _ = mux.select_tab(index)?;
    }
    Ok(())
}

/// Focus the saved session. `file` is name-keyed (see [`named_file`]), so
/// the wanted session and the live panes are matched by stable name.
fn seed_focus(
    mux: &mut MuxRuntime,
    pane_sessions: &HashMap<PaneId, String>,
    file: &AttachTabsFile,
    names: &HashMap<String, String>,
) -> Result<()> {
    let tab = file.active_tab.min(file.tabs.len().saturating_sub(1));
    let _ = mux.select_tab(tab)?;
    let want = file.focused_session.clone().or_else(|| {
        file.tabs
            .get(tab)
            .and_then(|tab| tab.sessions.first().cloned())
    });
    if let Some(want) = want {
        if let Some(pane) = pane_sessions
            .iter()
            .find_map(|(pane, id)| (session_name(mux, *pane, id, names) == want).then_some(*pane))
        {
            if let Some(window) = mux.pane_window(pane) {
                select_window(mux, window)?;
                let _ = mux.focus(pane);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach_tabs::AttachTabRecord;

    /// A fake `pmux` that ignores `attach --session-id S` and stays alive:
    /// a dead child would collapse its slot on the next drain and hide the
    /// layout regroup built. Regroup only needs the pane and the session mark.
    fn fake_mux_bin() -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "pt152-fake-mux-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pmux");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn runtime() -> MuxRuntime {
        MuxRuntime::spawn("/bin/sleep", &["30".to_string()], 80, 24).unwrap()
    }

    fn file(tabs: &[(&str, &[&str])], active_tab: usize, focused: Option<&str>) -> AttachTabsFile {
        AttachTabsFile {
            tabs: tabs
                .iter()
                .map(|(title, sessions)| AttachTabRecord {
                    title: (*title).into(),
                    sessions: sessions.iter().map(|s| (*s).to_string()).collect(),
                })
                .collect(),
            active_tab,
            focused_session: focused.map(str::to_string),
            space: None,
            mode: AttachTabsMode::Add,
            ..Default::default()
        }
    }

    /// Session names per tab, in pane order, from the live mapping.
    fn sessions_per_tab(mux: &MuxRuntime, panes: &HashMap<PaneId, String>) -> Vec<Vec<String>> {
        mux.tab_panes()
            .into_iter()
            .map(|(_, ids)| {
                ids.iter()
                    .filter_map(|pane| panes.get(pane).cloned())
                    .collect()
            })
            .collect()
    }

    fn total_panes(mux: &MuxRuntime) -> usize {
        mux.tab_panes().iter().map(|(_, panes)| panes.len()).sum()
    }

    fn titles(mux: &MuxRuntime) -> Vec<String> {
        mux.tab_infos().into_iter().map(|tab| tab.title).collect()
    }

    #[test]
    fn apply_attaches_saved_sessions_into_their_tabs_and_titles_them() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        let saved = file(
            &[("agents", &["s1"]), ("logs", &["s2", "s3"])],
            1,
            Some("s3"),
        );
        let changed = apply(&mut mux, &mut panes, &saved, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(changed);
        assert_eq!(
            sessions_per_tab(&mux, &panes),
            vec![
                vec!["s1".to_string()],
                vec!["s2".to_string(), "s3".to_string()]
            ]
        );
        assert_eq!(titles(&mux), vec!["agents".to_string(), "logs".to_string()]);
        assert_eq!(panes.len(), 3, "one attach pane per saved session");
        for (pane, session) in &panes {
            assert_eq!(mux.attach_session_of(*pane), Some(session.as_str()));
        }
        assert_eq!(mux.selected_tab_index(), 1, "active_tab is honoured");
        assert_eq!(
            panes.get(&mux.focused_id()).map(String::as_str),
            Some("s3"),
            "focused_session is honoured"
        );
        // The bare host's launch shell is closed once tab 0 holds a session;
        // no dummy placeholder survives (PT-170).
        assert_eq!(total_panes(&mux), 3);
    }

    #[test]
    fn apply_moves_live_sessions_without_leftover_dummy_tabs() {
        for mode in [AttachTabsMode::Add, AttachTabsMode::Switch] {
            let mut mux = runtime();
            let mut panes = HashMap::new();
            let fake = fake_mux_bin();
            let first = file(&[("a", &["s1"]), ("b", &["s2"]), ("c", &["s3"])], 0, None);
            apply(&mut mux, &mut panes, &first, &fake, &HashMap::new()).unwrap();
            let original_panes = panes.clone();
            assert_eq!(mux.tab_count(), 3);

            // Both source tabs need a temporary pane while their sessions move.
            // Once the moves finish, neither temporary tab belongs in the view.
            let mut regrouped = file(&[("both", &["s1", "s2", "s3"])], 0, Some("s3"));
            regrouped.mode = mode;
            assert!(apply(&mut mux, &mut panes, &regrouped, &fake, &HashMap::new()).unwrap());
            assert_eq!(
                sessions_per_tab(&mux, &panes),
                vec![vec!["s1".to_string(), "s2".to_string(), "s3".to_string()]],
                "the source tabs must close after their sessions move"
            );
            assert_eq!(titles(&mux), vec!["both"]);
            assert_eq!(panes, original_panes, "live pane identities are preserved");
            assert_eq!(total_panes(&mux), 3, "only the three session panes remain");
            assert_eq!(mux.selected_tab_index(), 0);
            assert_eq!(panes.get(&mux.focused_id()).map(String::as_str), Some("s3"));
            assert!(panes.keys().all(|pane| mux.pane_window(*pane).is_some()));
            assert!(
                !apply(&mut mux, &mut panes, &regrouped, &fake, &HashMap::new()).unwrap(),
                "applying the same arrangement again must be a no-op"
            );
        }
    }

    #[test]
    fn apply_move_cleanup_preserves_an_unrelated_local_tab() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        let first = file(&[("a", &["s1"]), ("b", &["s2"])], 0, None);
        apply(&mut mux, &mut panes, &first, FAKE_MUX, &HashMap::new()).unwrap();
        assert_eq!(mux.tab_panes().len(), 2);

        let local_window = mux.new_tab("/bin/sleep", &["30".to_string()]).unwrap();
        let local_pane = mux.focused_id();
        mux.rename_window(local_window, "local").unwrap();
        let mut regrouped = file(&[("both", &["s1", "s2"])], 0, Some("s2"));
        regrouped.mode = AttachTabsMode::Switch;
        let changed = apply(&mut mux, &mut panes, &regrouped, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(changed);
        assert_eq!(
            sessions_per_tab(&mux, &panes),
            vec![vec!["s1".to_string(), "s2".to_string()], vec![]],
            "both sessions share tab 0; the unrelated local tab survives"
        );
        assert_eq!(titles(&mux), vec!["both", "local"]);
        assert_eq!(mux.pane_window(local_pane), Some(local_window));
        assert_eq!(panes.len(), 2, "no new attach for a live session");
        assert_eq!(
            total_panes(&mux),
            3,
            "two attach panes and the intentional local pane"
        );
        assert_eq!(panes.get(&mux.focused_id()).map(String::as_str), Some("s2"));
        assert!(mux.close_tab_at(0).unwrap());
        assert_eq!(mux.focused_id(), local_pane);
        assert!(
            !mux.close_focused().unwrap(),
            "the final local pane stays protected"
        );
    }

    #[test]
    fn apply_is_a_noop_when_the_live_tabs_already_match() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        let saved = file(&[("a", &["s1"]), ("b", &["s2"])], 0, None);
        apply(&mut mux, &mut panes, &saved, FAKE_MUX, &HashMap::new()).unwrap();
        let before = (mux.tab_panes(), titles(&mux), total_panes(&mux));
        let changed = apply(&mut mux, &mut panes, &saved, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(!changed, "second apply of the same file is a no-op");
        assert_eq!((mux.tab_panes(), titles(&mux), total_panes(&mux)), before);
    }

    #[test]
    fn apply_keeps_attached_sessions_absent_from_the_file() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        let saved = file(&[("a", &["s1"]), ("b", &["s2"])], 0, None);
        apply(&mut mux, &mut panes, &saved, FAKE_MUX, &HashMap::new()).unwrap();
        // A newer file forgets s2: regroup never closes a session pane.
        let partial = file(&[("a", &["s1"])], 0, None);
        apply(&mut mux, &mut panes, &partial, FAKE_MUX, &HashMap::new()).unwrap();
        let live = sessions_per_tab(&mux, &panes);
        assert!(
            live.iter().flatten().any(|s| s == "s2"),
            "s2 must survive an apply that omits it: {live:?}"
        );
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn apply_places_a_new_session_beside_the_saved_neighbour() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        // s2 is new and saved next to s1: it is attached into tab 0, not a
        // fresh tab.
        let changed = apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1", "s2"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        assert!(changed);
        assert_eq!(mux.tab_panes().len(), 1);
        assert_eq!(
            sessions_per_tab(&mux, &panes),
            vec![vec!["s1".to_string(), "s2".to_string()]]
        );
    }

    /// A session that is already live must not be re-attached as a duplicate
    /// when a fresh cache names it under a new id (PT-108 open-side mirror).
    /// The pane attached under one id; the session was respawned and the
    /// daemon rebound it to another. Matching on the stable name, not the id,
    /// recognises the session as present.
    #[test]
    fn apply_does_not_duplicate_a_live_session_after_its_id_changes() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;

        // First open: the session named "seat" is live under id "3".
        let first = file(&[("work", &["3"])], 0, None);
        let names_first = HashMap::from([("3".to_string(), "seat".to_string())]);
        apply(&mut mux, &mut panes, &first, FAKE_MUX, &names_first).unwrap();
        assert_eq!(panes.len(), 1, "one attach pane for the live session");

        // The session is respawned; the daemon rebinds it to id "7". A fresh
        // cache references the same session under the new id.
        let second = file(&[("work", &["7"])], 0, None);
        let names_second = HashMap::from([("7".to_string(), "seat".to_string())]);
        let changed = apply(&mut mux, &mut panes, &second, FAKE_MUX, &names_second).unwrap();

        assert!(
            !changed,
            "the session is already live by name; nothing to do"
        );
        assert_eq!(
            panes.len(),
            1,
            "a session already live under a different id must not be re-attached"
        );
    }
    /// A failed daemon snapshot (empty `names`) must fall back to id
    /// matching on both sides. Live panes carry recorded names from an
    /// earlier successful regroup; comparing those against the file's ids
    /// matched nothing and re-attached every live session.
    #[test]
    fn empty_names_after_a_named_apply_does_not_duplicate() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        let first = file(&[("work", &["3"])], 0, None);
        let names = HashMap::from([("3".to_string(), "seat".to_string())]);
        apply(&mut mux, &mut panes, &first, FAKE_MUX, &names).unwrap();
        assert_eq!(panes.len(), 1);
        // Snapshot failed on the next chip click: empty map, same file.
        let changed = apply(&mut mux, &mut panes, &first, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(!changed, "same file + empty names must be a no-op");
        assert_eq!(
            panes.len(),
            1,
            "empty names must not re-attach the live session"
        );
    }
    /// Opening a space into a bare host must not keep the launch shell as
    /// pane 1 of the first tab (owner macOS dogfood, PT-170).
    #[test]
    fn apply_closes_the_bare_host_launch_shell_once_the_tab_holds_a_session() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        assert_eq!(total_panes(&mux), 1, "bare host: one local pane");
        let launch_shell = mux.focused_id();
        let saved = file(&[("work", &["s1"])], 0, Some("s1"));
        let changed = apply(&mut mux, &mut panes, &saved, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(changed);
        assert_eq!(sessions_per_tab(&mux, &panes), vec![vec!["s1".to_string()]]);
        assert_eq!(total_panes(&mux), 1, "the attach pane is the only pane");
        assert_ne!(mux.focused_id(), launch_shell, "the launch shell is gone");
        // A lone local pane in a tab the file does not fill stays put.
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let empty = file(&[], 0, None);
        apply(&mut mux, &mut panes, &empty, FAKE_MUX, &HashMap::new()).unwrap();
        assert_eq!(total_panes(&mux), 1, "no attaches: the local pane survives");
    }

    #[test]
    fn explicit_blank_terminal_survives_reused_tab_and_space_switch() {
        let mut mux = runtime();
        let local = mux.focused_id();
        mux.retain_local_terminal(local);
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        let first = file(&[("work", &["s1"])], 0, Some("s1"));
        apply(&mut mux, &mut panes, &first, &fake, &HashMap::new()).unwrap();
        assert_eq!(total_panes(&mux), 2);
        assert!(mux.is_retained_local_terminal(local));
        let mut next = file(&[("other", &["s2"])], 0, Some("s2"));
        next.mode = AttachTabsMode::Switch;
        apply(&mut mux, &mut panes, &next, &fake, &HashMap::new()).unwrap();
        assert!(mux.pane_window(local).is_some());
        assert!(mux.is_retained_local_terminal(local));
        assert!(!panes.contains_key(&local));
    }

    #[test]
    fn apply_switch_detaches_sessions_absent_from_the_file() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1"]), ("b", &["s2"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(panes.len(), 2);
        let mut next = file(&[("a", &["s1"])], 0, None);
        next.mode = AttachTabsMode::Switch;
        let changed = apply(&mut mux, &mut panes, &next, FAKE_MUX, &HashMap::new()).unwrap();
        assert!(changed);
        let live: Vec<String> = sessions_per_tab(&mux, &panes)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(live, vec!["s1".to_string()]);
        assert_eq!(
            panes.len(),
            1,
            "s2 host pane detaches; session is not killed"
        );
        assert!(
            live.iter().all(|s| s != "s2"),
            "s2 must not remain attached: {live:?}"
        );
        assert_eq!(
            total_panes(&mux),
            panes.len(),
            "no leftover placeholder beside the kept session"
        );
    }

    #[test]
    fn apply_switch_from_one_tab_space_leaves_no_placeholder() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1"]), ("b", &["s2"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        let mut next = file(&[("c", &["s3"])], 0, Some("s3"));
        next.mode = AttachTabsMode::Switch;
        apply(&mut mux, &mut panes, &next, FAKE_MUX, &HashMap::new()).unwrap();
        assert_eq!(sessions_per_tab(&mux, &panes), vec![vec!["s3".to_string()]]);
        assert_eq!(panes.len(), 1, "only the new space's session is attached");
        assert_eq!(
            total_panes(&mux),
            1,
            "switch must not leave a placeholder /bin/sh: panes={}",
            total_panes(&mux)
        );
    }

    #[test]
    fn apply_switch_keeps_a_local_shell() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        mux.split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        let before_panes = total_panes(&mux);
        assert_eq!(before_panes, 2, "attach + local shell");
        let mut next = file(&[("b", &["s2"])], 0, None);
        next.mode = AttachTabsMode::Switch;
        apply(&mut mux, &mut panes, &next, FAKE_MUX, &HashMap::new()).unwrap();
        assert_eq!(panes.len(), 1, "only s2 is attached");
        assert!(sessions_per_tab(&mux, &panes)
            .iter()
            .flatten()
            .any(|s| s == "s2"));
        let local = total_panes(&mux).saturating_sub(panes.len());
        assert_eq!(
            local,
            1,
            "the local shell stays: panes={}",
            total_panes(&mux)
        );
    }

    #[test]
    fn apply_add_still_keeps_sessions_absent_from_the_file() {
        let mut mux = runtime();
        let mut panes = HashMap::new();
        let fake = fake_mux_bin();
        #[allow(non_snake_case)]
        let FAKE_MUX: &str = &fake;
        apply(
            &mut mux,
            &mut panes,
            &file(&[("a", &["s1"]), ("b", &["s2"])], 0, None),
            FAKE_MUX,
            &HashMap::new(),
        )
        .unwrap();
        let mut next = file(&[("a", &["s1"])], 0, None);
        next.mode = AttachTabsMode::Add;
        apply(&mut mux, &mut panes, &next, FAKE_MUX, &HashMap::new()).unwrap();
        let live = sessions_per_tab(&mux, &panes);
        assert!(
            live.iter().flatten().any(|s| s == "s2"),
            "add mode must keep s2: {live:?}"
        );
        assert_eq!(panes.len(), 2);
    }
}
