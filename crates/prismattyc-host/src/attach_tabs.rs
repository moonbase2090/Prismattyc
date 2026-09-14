//! Attach-tabs cache helpers. Contract: `docs/mux-cli.md` (Where things live).

use std::collections::HashMap;

use crate::AttachTarget;

/// A private view file for a window that does not own the default CLI target.
pub fn window_layout_path(socket: &std::path::Path, pid: u32, window: u64) -> std::path::PathBuf {
    let mut path = layout_path_from_socket(socket).into_os_string();
    path.push(format!(".view-{pid}-{window}.json"));
    path.into()
}

pub use prismattyc_mux::attach_tabs::{
    layout_path_from_socket, load, persist_if_changed, regroup_diff, remap_tabs, AttachTabRecord,
    AttachTabsFile, AttachTabsMode,
};

/// Build the cache from live tabs in strip order.
///
/// Each pane is a mux session id or `None` for a local-shell pane.
/// Local-shell panes are skipped. A tab with no remaining sessions is
/// omitted. A PT-68 placeholder is not a collapse: keep its session.
pub fn records_from_live_tabs<I>(
    tabs: I,
    selected: usize,
    focused_session: Option<String>,
) -> AttachTabsFile
where
    I: IntoIterator<Item = (String, Vec<Option<String>>)>,
{
    let (kept, active_tab) = remap_tabs(tabs, selected, |(title, panes)| {
        let sessions: Vec<String> = panes.into_iter().flatten().collect();
        (!sessions.is_empty()).then_some((title, sessions))
    });
    let focused_session = focused_session.filter(|id| {
        kept.iter()
            .any(|(_, sessions)| sessions.iter().any(|session| session == id))
    });
    AttachTabsFile {
        tabs: kept
            .into_iter()
            .map(|(title, sessions)| AttachTabRecord { title, sessions })
            .collect(),
        active_tab,
        focused_session,
        space: None,
        mode: AttachTabsMode::Add,
        ..Default::default()
    }
}

/// Update only selection on an existing cache (child-exit; PT-68).
pub fn overlay_selection(
    file: &mut AttachTabsFile,
    live_sessions: &[String],
    focused: Option<String>,
) {
    if let Some(index) = file
        .tabs
        .iter()
        .position(|tab| tab.sessions.iter().any(|id| live_sessions.contains(id)))
    {
        file.active_tab = index;
    }
    if let Some(id) = focused.filter(|id| file.tabs.iter().any(|tab| tab.sessions.contains(id))) {
        file.focused_session = Some(id);
    } else if let Some(tab) = file.tabs.get(file.active_tab) {
        file.focused_session = tab.sessions.first().cloned();
    }
}

/// Live cache file from a mux runtime and attach pane → session map.
pub fn records_from_runtime(
    runtime: &crate::mux::MuxRuntime,
    sessions: &HashMap<prismattyc_mux::PaneId, String>,
) -> AttachTabsFile {
    records_from_live_tabs(
        runtime.tab_panes().into_iter().map(|(title, panes)| {
            (
                title,
                panes
                    .into_iter()
                    .map(|pane| {
                        runtime
                            .attach_session_of(pane)
                            .map(str::to_string)
                            .or_else(|| sessions.get(&pane).cloned())
                    })
                    .collect(),
            )
        }),
        runtime.selected_tab_index(),
        runtime
            .attach_session_of(runtime.focused_id())
            .map(str::to_string)
            .or_else(|| sessions.get(&runtime.focused_id()).cloned()),
    )
}

/// Tab title for a restored group with no saved title: the session title
/// when the tab holds one session, else the titles joined with " + ".
pub fn attach_group_title(members: &[AttachTarget]) -> String {
    if members.len() == 1 {
        return members[0].title.clone();
    }
    members
        .iter()
        .map(|member| member.title.as_str())
        .collect::<Vec<_>>()
        .join(" + ")
}

/// One tab per session, in attach order. Used for sessions with no saved
/// tab state.
fn one_tab_per_session(sessions: &[AttachTarget]) -> Vec<(String, Vec<AttachTarget>)> {
    sessions
        .iter()
        .map(|session| (session.title.clone(), vec![session.clone()]))
        .collect()
}

/// Restored tab groups plus the remapped active tab / focused session.
pub struct AttachGroups {
    pub groups: Vec<(String, Vec<AttachTarget>)>,
    pub active_tab: usize,
    pub focused_session: Option<String>,
}

/// Restore saved tab titles and membership, then append new sessions one
/// tab each. `active_tab` is an index into `groups` after dropped tabs.
pub fn group_attach_targets(
    sessions: &[AttachTarget],
    layout: Option<&AttachTabsFile>,
) -> AttachGroups {
    let Some(layout) = layout else {
        return AttachGroups {
            groups: one_tab_per_session(sessions),
            active_tab: 0,
            focused_session: None,
        };
    };
    let mut remaining: HashMap<String, AttachTarget> = sessions
        .iter()
        .map(|session| (session.session.clone(), session.clone()))
        .collect();
    let (kept, active_tab) = remap_tabs(layout.tabs.iter(), layout.active_tab, |tab| {
        let members: Vec<AttachTarget> = tab
            .sessions
            .iter()
            .filter_map(|id| remaining.remove(id))
            .collect();
        if members.is_empty() {
            return None;
        }
        let title = if tab.title.trim().is_empty() {
            attach_group_title(&members)
        } else {
            tab.title.clone()
        };
        Some((title, members))
    });
    let focused_session = layout.focused_session.clone().filter(|id| {
        kept.iter()
            .any(|(_, members)| members.iter().any(|member| member.session == *id))
    });
    let mut groups = kept;
    if !remaining.is_empty() {
        let leftover: Vec<AttachTarget> = sessions
            .iter()
            .filter(|session| remaining.contains_key(&session.session))
            .cloned()
            .collect();
        groups.extend(one_tab_per_session(&leftover));
    }
    AttachGroups {
        groups,
        active_tab,
        focused_session,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use prismattyc_mux::attach_tabs::save;

    use super::*;

    fn target(session: &str, title: &str) -> AttachTarget {
        AttachTarget {
            session: session.into(),
            title: title.into(),
        }
    }

    #[test]
    fn no_layout_gives_one_tab_per_session() {
        let sessions = [
            target("2", "alpha-pm"),
            target("3", "beta-pm"),
            target("10", "alpha-sb"),
        ];
        let grouped = group_attach_targets(&sessions, None);
        let groups = grouped.groups;
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, "alpha-pm");
        assert_eq!(groups[1].0, "beta-pm");
        assert_eq!(groups[2].0, "alpha-sb");
        assert!(groups.iter().all(|(_, members)| members.len() == 1));
        assert_eq!(grouped.active_tab, 0);
    }

    #[test]
    fn layout_restores_custom_titles_and_drops_missing_sessions() {
        let sessions = [
            target("2", "alpha-pm"),
            target("10", "alpha-sb"),
            target("99", "new-xt"),
        ];
        let layout = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "WORK".into(),
                    sessions: vec!["2".into(), "gone".into()],
                },
                AttachTabRecord {
                    title: "Agents".into(),
                    sessions: vec!["10".into()],
                },
            ],
            ..Default::default()
        };
        let grouped = group_attach_targets(&sessions, Some(&layout));
        let groups = grouped.groups;
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, "WORK");
        assert_eq!(groups[0].1[0].session, "2");
        assert_eq!(groups[1].0, "Agents");
        assert_eq!(groups[1].1[0].session, "10");
        assert_eq!(groups[2].0, "new-xt");
        assert_eq!(groups[2].1[0].session, "99");
    }

    #[test]
    fn group_attach_targets_remaps_active_tab_when_a_file_tab_is_dropped() {
        let sessions = [target("2", "B"), target("3", "C")];
        let layout = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "A".into(),
                    sessions: vec!["1".into()],
                },
                AttachTabRecord {
                    title: "B".into(),
                    sessions: vec!["2".into()],
                },
                AttachTabRecord {
                    title: "C".into(),
                    sessions: vec!["3".into()],
                },
            ],
            active_tab: 2,
            focused_session: Some("3".into()),
            space: None,
            mode: AttachTabsMode::Add,
            ..Default::default()
        };
        let grouped = group_attach_targets(&sessions, Some(&layout));
        assert_eq!(grouped.groups.len(), 2);
        assert_eq!(grouped.groups[0].0, "B");
        assert_eq!(grouped.groups[1].0, "C");
        assert_eq!(grouped.active_tab, 1, "file index 2 becomes live index 1");
        assert_eq!(grouped.focused_session.as_deref(), Some("3"));
    }

    #[test]
    fn records_from_live_tabs_keeps_nearest_when_selected_tab_is_local_shell() {
        let file = records_from_live_tabs(
            vec![
                ("a".into(), vec![Some("2".into())]),
                ("scratch".into(), vec![None]),
                ("b".into(), vec![Some("3".into())]),
            ],
            1,
            None,
        );
        assert_eq!(file.tabs.len(), 2);
        assert_eq!(file.tabs[0].title, "a");
        assert_eq!(file.tabs[1].title, "b");
        assert_eq!(file.active_tab, 0);
    }

    #[test]
    fn overlay_selection_keeps_tab_records() {
        let mut file = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "a".into(),
                    sessions: vec!["1".into(), "2".into()],
                },
                AttachTabRecord {
                    title: "b".into(),
                    sessions: vec!["3".into()],
                },
            ],
            active_tab: 1,
            focused_session: Some("3".into()),
            space: None,
            mode: AttachTabsMode::Add,
            ..Default::default()
        };
        overlay_selection(&mut file, &["2".into()], Some("2".into()));
        assert_eq!(file.tabs.len(), 2);
        assert_eq!(file.active_tab, 0);
        assert_eq!(file.focused_session.as_deref(), Some("2"));
    }

    #[test]
    fn layout_path_sits_beside_the_socket() {
        let path = layout_path_from_socket(Path::new("/run/user/1000/prismattyc/pmux.sock"));
        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/prismattyc/pmux.attach-tabs.json")
        );
    }

    fn cache_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "prism-attach-cache-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("pmux.attach-tabs.json");
        (dir, path)
    }

    fn live_file(
        runtime: &crate::mux::MuxRuntime,
        sessions: &std::collections::HashMap<prismattyc_mux::PaneId, String>,
    ) -> AttachTabsFile {
        records_from_runtime(runtime, sessions)
    }

    fn persist_live(
        path: &Path,
        last: &mut Option<AttachTabsFile>,
        runtime: &crate::mux::MuxRuntime,
        sessions: &std::collections::HashMap<prismattyc_mux::PaneId, String>,
    ) -> bool {
        persist_if_changed(path, last, live_file(runtime, sessions)).unwrap()
    }

    #[test]
    fn runtime_marked_session_persists_without_hashmap() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        runtime.mark_attach_session(pane, "6".into(), "codex-mb".into());
        let empty = std::collections::HashMap::new();
        let file = records_from_runtime(&runtime, &empty);
        assert_eq!(file.tabs.len(), 1);
        assert_eq!(file.tabs[0].sessions, vec!["6".to_string()]);
        assert_eq!(file.focused_session.as_deref(), Some("6"));
    }

    #[test]
    fn two_tabs_keep_runtime_pane_missing_from_map() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime
            .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        let second = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let third = runtime.focused_id();
        runtime
            .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        let fourth = runtime.focused_id();
        runtime.mark_attach_session(first, "2".into(), "a".into());
        runtime.mark_attach_session(second, "4".into(), "b".into());
        runtime.mark_attach_session(third, "5".into(), "c".into());
        runtime.mark_attach_session(fourth, "6".into(), "d".into());
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(first, "2".into());
        sessions.insert(second, "4".into());
        sessions.insert(third, "5".into());
        let file = records_from_runtime(&runtime, &sessions);
        assert_eq!(file.tabs.len(), 2);
        assert_eq!(
            file.tabs[0].sessions,
            vec!["2".to_string(), "4".to_string()]
        );
        assert_eq!(
            file.tabs[1].sessions,
            vec!["5".to_string(), "6".to_string()]
        );
    }

    #[test]
    fn even_columns_registers_new_attach_panes() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.mark_attach_session(first, "2".into(), "a".into());
        let before = runtime.active_pane_ids();
        runtime.ensure_even_columns("/bin/sh", &[], 2).unwrap();
        runtime.register_new_active_attaches(&before, "6");
        let empty = std::collections::HashMap::new();
        let file = records_from_runtime(&runtime, &empty);
        assert_eq!(file.tabs.len(), 1);
        let sessions = &file.tabs[0].sessions;
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        assert!(sessions.contains(&"2".to_string()), "{sessions:?}");
        assert!(sessions.contains(&"6".to_string()), "{sessions:?}");
    }

    #[test]
    fn mux_close_tab_rewrites_cache() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let second = runtime.focused_id();
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(first, "2".into());
        sessions.insert(second, "3".into());
        let (dir, path) = cache_path("close-tab");
        let mut last = None;
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        assert_eq!(load(&path).unwrap().tabs.len(), 2);
        assert!(runtime.close_tab().unwrap());
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        let file = load(&path).unwrap();
        assert_eq!(file.tabs.len(), 1);
        assert_eq!(file.tabs[0].sessions, vec!["2".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mux_rename_tab_rewrites_title() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let pane = runtime.focused_id();
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(pane, "2".into());
        let (dir, path) = cache_path("rename");
        let mut last = None;
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        runtime
            .rename_window(runtime.active_window_id(), "WORK")
            .unwrap();
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        assert_eq!(load(&path).unwrap().tabs[0].title, "WORK");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mux_move_pane_rewrites_grouping() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        let second = runtime
            .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(first, "2".into());
        sessions.insert(second, "3".into());
        runtime.select_tab(0).unwrap();
        runtime.focus(second);
        let (dir, path) = cache_path("move");
        let mut last = None;
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        assert_eq!(
            load(&path).unwrap().tabs[0].sessions,
            vec!["2".to_string(), "3".to_string()]
        );
        runtime.move_focused_to_tab(1).unwrap();
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        let file = load(&path).unwrap();
        assert_eq!(file.tabs[0].sessions, vec!["2".to_string()]);
        assert_eq!(file.tabs[1].sessions, vec!["3".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mux_select_rewrites_active_tab_and_split_drops_focus() {
        let mut runtime = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 80, 24).unwrap();
        let first = runtime.focused_id();
        runtime.new_tab("/bin/sh", &[]).unwrap();
        let second = runtime.focused_id();
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(first, "2".into());
        sessions.insert(second, "3".into());
        let (dir, path) = cache_path("split-select");
        let mut last = None;
        assert!(persist_live(&path, &mut last, &runtime, &sessions));
        let on_second = load(&path).unwrap();
        assert_eq!(on_second.active_tab, 1);
        assert_eq!(on_second.focused_session.as_deref(), Some("3"));
        runtime.select_tab(0).unwrap();
        assert!(
            persist_live(&path, &mut last, &runtime, &sessions),
            "select must rewrite active_tab"
        );
        let on_first = load(&path).unwrap();
        assert_eq!(on_first.active_tab, 0);
        assert_eq!(on_first.focused_session.as_deref(), Some("2"));
        runtime
            .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        assert!(
            persist_live(&path, &mut last, &runtime, &sessions),
            "split focuses a local pane so focused_session drops"
        );
        let after_split = load(&path).unwrap();
        assert_eq!(after_split.tabs, on_first.tabs);
        assert_eq!(after_split.focused_session, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("prism-attach-tabs-{}-rt", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("pmux.attach-tabs.json");
        let file = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "WORK".into(),
                sessions: vec!["2".into()],
            }],
            ..Default::default()
        };
        save(&path, &file).unwrap();
        assert_eq!(load(&path).as_ref(), Some(&file));
        let _ = fs::remove_dir_all(&dir);
    }
}
