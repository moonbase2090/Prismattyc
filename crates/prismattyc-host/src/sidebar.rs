//! Combined sidebar tree model (issue #113): Spaces → tabs → panes.
//!
//! Data layer behind `layout = "sidebar"`. Rendering, hit-testing, and the
//! collapse toggles follow in later PRs; the default `bars` layout never
//! builds this. Only reads pane-handle fields (#109 owns handle behavior).

use crate::mux::TabInfo;
use prismattyc_mux::SavedSpaceTab;

/// One pane row in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneNode {
    pub title: String,
    pub focused: bool,
    pub active: bool,
    /// Waiting mail count (the mail badge).
    pub mail: u32,
    /// Agent attention is waiting on this pane.
    pub attention: bool,
}

/// One tab row with its panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabNode {
    pub title: String,
    pub fixed_name: Option<String>,
    pub selected: bool,
    pub unseen: bool,
    pub attention: bool,
    pub zoomed: bool,
    pub panes: Vec<PaneNode>,
}

/// One space row with its tabs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceNode {
    pub name: String,
    pub current: bool,
    pub collapsed: bool,
    /// Sessions needing input (the needs-you count).
    pub attention: usize,
    pub tabs: Vec<TabNode>,
}

/// Live tab for the attached space: info plus per-pane mail depths aligned
/// with the layout panes (empty means none waiting).
pub struct LiveTab<'a> {
    pub info: &'a TabInfo,
    /// Attached session display names, separate from local child titles.
    pub session_names: &'a [String],
    pub mail: &'a [u32],
    pub attention: &'a [bool],
}

/// Tabs for one space: live info for the attached space, saved file content
/// for the rest.
pub enum TabsSource<'a> {
    Live(&'a [LiveTab<'a>]),
    Saved {
        tabs: &'a [SavedSpaceTab],
        /// Sessions outside every saved tab; each becomes its own tab.
        extra_sessions: &'a [String],
    },
}

/// One space's input to [`SidebarTree::build`].
pub struct SpaceInput<'a> {
    pub name: &'a str,
    pub current: bool,
    pub collapsed: bool,
    pub attention: usize,
    pub tabs: TabsSource<'a>,
}

/// Spaces → tabs → panes in rail order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidebarTree {
    pub spaces: Vec<SpaceNode>,
}

impl TabNode {
    /// Live tab from the attached space; `mail` and `attention` align with
    /// layout panes (empty means none / false).
    pub fn live(info: &TabInfo, mail: &[u32], attention: &[bool]) -> Self {
        let panes = if info.handles == 0 {
            vec![PaneNode {
                title: info
                    .pane_title
                    .clone()
                    .unwrap_or_else(|| info.title.clone()),
                focused: info.selected,
                active: info.active,
                mail: mail.first().copied().unwrap_or(0),
                attention: attention.first().copied().unwrap_or(false),
            }]
        } else {
            info.handle_titles
                .iter()
                .enumerate()
                .map(|(index, title)| PaneNode {
                    title: title.clone(),
                    focused: info.focused_handle == Some(index),
                    active: info.handle_active.get(index).copied().unwrap_or(false),
                    mail: mail.get(index).copied().unwrap_or(0),
                    attention: attention.get(index).copied().unwrap_or(false),
                })
                .collect()
        };
        TabNode {
            title: info.title.clone(),
            fixed_name: info.fixed_name.clone(),
            selected: info.selected,
            unseen: info.unseen,
            attention: info.attention,
            zoomed: info.zoomed,
            panes,
        }
    }

    /// Saved tab from another space's file: structure only, no live state.
    pub fn saved(title: &str, fixed_name: Option<&str>, sessions: &[String]) -> Self {
        TabNode {
            title: tab_label(fixed_name.unwrap_or(title), sessions),
            fixed_name: fixed_name.map(str::to_string),
            selected: false,
            unseen: false,
            attention: false,
            zoomed: false,
            panes: sessions
                .iter()
                .map(|session| PaneNode {
                    title: session.clone(),
                    focused: false,
                    active: false,
                    mail: 0,
                    attention: false,
                })
                .collect(),
        }
    }
}

/// Keep session identity visible without discarding an explicit window title.
fn tab_label(title: &str, sessions: &[String]) -> String {
    match sessions {
        [session] if !session.is_empty() => {
            if title.trim().is_empty() || title == "main" || title == session {
                session.clone()
            } else {
                format!("{title} · {session}")
            }
        }
        _ => title.to_string(),
    }
}

/// Resolve live names by verified Space ownership. Names that cannot be linked
/// to a live identity remain saved/offline rows, rather than borrowing a name.
pub fn saved_tabs(
    space: &prismattyc_mux::SavedSpace,
    snapshot: Option<&prismattyc_mux::Snapshot>,
) -> (Vec<SavedSpaceTab>, Vec<String>) {
    let owned: Vec<_> = snapshot
        .into_iter()
        .flat_map(|snapshot| &snapshot.sessions)
        .filter(|session| space.id.is_some() && session.space_id == space.id)
        .collect();
    // SavedSpaceTab members are names, including numeric names, not daemon IDs.
    let name = |key: &str| {
        owned
            .iter()
            .find(|session| session.name == key)
            .map_or_else(|| key.to_string(), |session| session.name.clone())
    };
    let mut tabs = space.tabs.clone();
    for tab in &mut tabs {
        tab.sessions = tab.sessions.iter().map(|key| name(key)).collect();
    }
    let mut order: Vec<_> = prismattyc_mux::space_sessions_in_tab_order(space)
        .iter()
        .map(|key| name(key))
        .collect();
    for session in owned {
        if !order.contains(&session.name) {
            order.push(session.name.clone());
        }
    }
    (tabs, order)
}

/// A session row is highlighted only while its space is current and that
/// pane has keyboard focus. The tab row uses the tab's own selected flag,
/// so both can be highlighted at once.
pub fn session_row_selected(current_space: bool, pane_focused: bool) -> bool {
    current_space && pane_focused
}

/// Titles in the same order as the pane rows [`TabNode::live`] builds.
pub fn session_titles(info: &TabInfo) -> Vec<String> {
    TabNode::live(info, &[], &[])
        .panes
        .into_iter()
        .map(|pane| pane.title)
        .collect()
}

/// A session row clicked in a space that is not current. The host opens
/// the space, then focuses the pane whose title matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSession {
    pub space: String,
    pub title: String,
    pub tab: Option<usize>,
}

/// Where to focus once `current_space` is the pending space and a live
/// pane carries `title`. Nothing matches until the space has actually
/// switched, so the click cannot focus a pane in the space being left.
pub fn pending_session_slot(
    pending: &PendingSession,
    current_space: Option<&str>,
    tabs: &[Vec<&str>],
) -> Option<(usize, usize)> {
    if current_space != Some(pending.space.as_str()) || pending.title.is_empty() {
        return None;
    }
    if let Some(tab) = pending.tab {
        if let Some(pane) = tabs
            .get(tab)
            .and_then(|panes| panes.iter().position(|title| *title == pending.title))
        {
            return Some((tab, pane));
        }
    }
    for (tab, panes) in tabs.iter().enumerate() {
        if let Some(pane) = panes.iter().position(|title| *title == pending.title) {
            return Some((tab, pane));
        }
    }
    None
}

impl SpaceNode {
    /// Tabs the tree shows: none while collapsed.
    pub fn visible_tabs(&self) -> &[TabNode] {
        if self.collapsed {
            &[]
        } else {
            &self.tabs
        }
    }
}

/// One flattened tree row for layout and hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Space,
    Tab,
    Pane,
}

/// One flattened row: indices into the tree plus depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub depth: usize,
    pub kind: RowKind,
    pub space: usize,
    pub tab: Option<usize>,
    pub pane: Option<usize>,
}

/// Pane rows appear only when a session splits into multiple panes (#181).
pub fn show_pane_rows(pane_count: usize, fixed_name: Option<&str>) -> bool {
    pane_count > 1 || (pane_count == 1 && fixed_name.is_some())
}

/// Rows in paint order: every space, then tabs and panes of expanded ones.
pub fn visible_rows(tree: &SidebarTree) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for (space_index, space) in tree.spaces.iter().enumerate() {
        rows.push(TreeRow {
            depth: 0,
            kind: RowKind::Space,
            space: space_index,
            tab: None,
            pane: None,
        });
        for (tab_index, tab) in space.visible_tabs().iter().enumerate() {
            rows.push(TreeRow {
                depth: 1,
                kind: RowKind::Tab,
                space: space_index,
                tab: Some(tab_index),
                pane: None,
            });
            if show_pane_rows(tab.panes.len(), tab.fixed_name.as_deref()) {
                for (pane_index, _) in tab.panes.iter().enumerate() {
                    rows.push(TreeRow {
                        depth: 2,
                        kind: RowKind::Pane,
                        space: space_index,
                        tab: Some(tab_index),
                        pane: Some(pane_index),
                    });
                }
            }
        }
    }
    rows
}

/// Icon-strip rows. A collapsed space still lists its tabs and panes: the
/// strip is navigation, and the per-space chevron is a separate control.
pub fn icon_rows(tree: &SidebarTree) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for (space_index, space) in tree.spaces.iter().enumerate() {
        rows.push(TreeRow {
            depth: 0,
            kind: RowKind::Space,
            space: space_index,
            tab: None,
            pane: None,
        });
        for (tab_index, tab) in space.tabs.iter().enumerate() {
            rows.push(TreeRow {
                depth: 1,
                kind: RowKind::Tab,
                space: space_index,
                tab: Some(tab_index),
                pane: None,
            });
            if show_pane_rows(tab.panes.len(), tab.fixed_name.as_deref()) {
                for (pane_index, _) in tab.panes.iter().enumerate() {
                    rows.push(TreeRow {
                        depth: 2,
                        kind: RowKind::Pane,
                        space: space_index,
                        tab: Some(tab_index),
                        pane: Some(pane_index),
                    });
                }
            }
        }
    }
    rows
}

/// Needs-you badge weight for one flattened tree row (0 hides the marker).
pub fn row_needs_you(tree: &SidebarTree, row: &TreeRow) -> usize {
    let space = tree.spaces.get(row.space);
    match (space, row.kind) {
        (Some(space), RowKind::Space) => space.attention,
        (Some(space), RowKind::Tab) => {
            let tab = row.tab.and_then(|index| space.tabs.get(index));
            tab.map(|tab| usize::from(tab.attention || tab.panes.iter().any(|pane| pane.attention)))
                .unwrap_or(0)
        }
        (Some(space), RowKind::Pane) => {
            let tab = row.tab.and_then(|index| space.tabs.get(index));
            let pane = row
                .pane
                .and_then(|index| tab.and_then(|tab| tab.panes.get(index)));
            usize::from(pane.is_some_and(|pane| pane.attention))
        }
        _ => 0,
    }
}

impl SidebarTree {
    /// Build the tree in rail order. Saved spaces with no recorded tabs
    /// show one tab per session.
    pub fn build(spaces: &[SpaceInput<'_>]) -> Self {
        let mut tree = SidebarTree::default();
        for space in spaces {
            let mut tabs: Vec<TabNode> = match &space.tabs {
                TabsSource::Live(tabs) => tabs
                    .iter()
                    .map(|tab| {
                        let mut node = TabNode::live(tab.info, tab.mail, tab.attention);
                        if node.panes.len() == 1 && !tab.session_names.is_empty() {
                            node.title = tab_label(&tab.info.title, tab.session_names);
                        }
                        node
                    })
                    .collect(),
                TabsSource::Saved {
                    tabs,
                    extra_sessions,
                } => {
                    let mut nodes: Vec<TabNode> = tabs
                        .iter()
                        .map(|tab| {
                            TabNode::saved(&tab.title, tab.fixed_name.as_deref(), &tab.sessions)
                        })
                        .collect();
                    let known: Vec<&str> = tabs
                        .iter()
                        .flat_map(|tab| tab.sessions.iter().map(String::as_str))
                        .collect();
                    for session in *extra_sessions {
                        if !known.contains(&session.as_str()) {
                            nodes.push(TabNode::saved(
                                session,
                                None,
                                std::slice::from_ref(session),
                            ));
                        }
                    }
                    nodes
                }
            };
            let titles: Vec<_> = tabs.iter().map(|tab| tab.title.clone()).collect();
            for (index, tab) in tabs.iter_mut().enumerate() {
                if titles.iter().filter(|title| **title == tab.title).count() > 1 {
                    tab.title = format!("{} · window {}", tab.title, index + 1);
                }
            }
            tree.spaces.push(SpaceNode {
                name: space.name.to_string(),
                current: space.current,
                collapsed: space.collapsed,
                attention: space.attention,
                tabs,
            });
        }
        tree
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_tab(
        title: &str,
        selected: bool,
        handles: Vec<(&str, bool)>,
        focused: Option<usize>,
    ) -> TabInfo {
        TabInfo {
            window: None,
            title: title.to_string(),
            selected,
            unseen: false,
            active: true,
            attention: false,
            zoomed: false,
            handles: handles.len(),
            focused_handle: focused,
            handle_titles: handles.iter().map(|(name, _)| name.to_string()).collect(),
            handle_active: handles.iter().map(|(_, active)| *active).collect(),
            pane_title: None,
            git_label: None,
            fixed_name: None,
        }
    }

    #[test]
    fn saved_single_session_row_shows_identity_instead_of_generic_window_title() {
        let tabs = [SavedSpaceTab {
            title: "main".into(),
            sessions: vec!["agent-1".into()],
            layout: None,
            fixed_name: None,
        }];
        let tree = SidebarTree::build(&[SpaceInput {
            name: "space-a",
            current: false,
            collapsed: false,
            attention: 0,
            tabs: TabsSource::Saved {
                tabs: &tabs,
                extra_sessions: &[],
            },
        }]);
        assert_eq!(tree.spaces[0].tabs[0].title, "agent-1");
    }

    #[test]
    fn session_labels_are_invariant_when_selection_switches_spaces() {
        let saved: Vec<_> = [
            ("main", "agent-1"),
            ("logs", "agent-2"),
            ("Review", "agent-3"),
        ]
        .into_iter()
        .map(|(title, session)| {
            vec![SavedSpaceTab {
                title: title.into(),
                sessions: vec![session.into()],
                layout: None,
                fixed_name: None,
            }]
        })
        .collect();
        let names = ["space-a", "space-b", "space-c"];
        let identities: Vec<_> = saved.iter().map(|tabs| tabs[0].sessions.clone()).collect();
        let infos: Vec<_> = saved
            .iter()
            .map(|tabs| {
                let mut info = live_tab(&tabs[0].title, true, vec![], None);
                info.pane_title = Some(tabs[0].sessions[0].clone());
                info
            })
            .collect();
        for selected in 0..3 {
            let live = [LiveTab {
                info: &infos[selected],
                session_names: &identities[selected],
                mail: &[],
                attention: &[],
            }];
            let inputs: Vec<_> = (0..3)
                .map(|index| SpaceInput {
                    name: names[index],
                    current: index == selected,
                    collapsed: false,
                    attention: 0,
                    tabs: if index == selected {
                        TabsSource::Live(&live)
                    } else {
                        TabsSource::Saved {
                            tabs: &saved[index],
                            extra_sessions: &[],
                        }
                    },
                })
                .collect();
            let tree = SidebarTree::build(&inputs);
            let labels: Vec<_> = tree
                .spaces
                .iter()
                .map(|space| space.tabs[0].title.as_str())
                .collect();
            assert_eq!(labels, ["agent-1", "logs · agent-2", "Review · agent-3"]);
            assert_eq!(tree.spaces[1].tabs[0].panes[0].title, "agent-2");
        }
    }

    #[test]
    fn local_rows_keep_their_title_policy_without_a_session_identity() {
        let mut info = live_tab("Local notes", true, vec![], None);
        info.pane_title = Some("shell title".into());
        let live = [LiveTab {
            info: &info,
            session_names: &[],
            mail: &[],
            attention: &[],
        }];
        let tree = SidebarTree::build(&[SpaceInput {
            name: "space-a",
            current: true,
            collapsed: false,
            attention: 0,
            tabs: TabsSource::Live(&live),
        }]);
        assert_eq!(tree.spaces[0].tabs[0].title, "Local notes");
        assert_eq!(tree.spaces[0].tabs[0].panes[0].title, "shell title");
    }

    #[test]
    fn repeated_windows_keep_session_identity_and_custom_titles() {
        let tabs: Vec<_> = ["main", "main", "Review"]
            .into_iter()
            .map(|title| SavedSpaceTab {
                title: title.into(),
                sessions: vec!["agent-1".into()],
                layout: None,
                fixed_name: None,
            })
            .collect();
        let tree = SidebarTree::build(&[SpaceInput {
            name: "space-a",
            current: false,
            collapsed: false,
            attention: 0,
            tabs: TabsSource::Saved {
                tabs: &tabs,
                extra_sessions: &[],
            },
        }]);
        assert_eq!(
            tree.spaces[0]
                .tabs
                .iter()
                .map(|tab| tab.title.as_str())
                .collect::<Vec<_>>(),
            [
                "agent-1 · window 1",
                "agent-1 · window 2",
                "Review · agent-1"
            ]
        );
        let pending = PendingSession {
            space: "space-a".into(),
            title: "agent-1".into(),
            tab: Some(1),
        };
        assert_eq!(
            pending_session_slot(
                &pending,
                Some("space-a"),
                &[vec!["agent-1"], vec!["agent-1"], vec!["agent-1"]]
            ),
            Some((1, 0))
        );
        assert_eq!(
            pending_session_slot(
                &pending,
                Some("space-b"),
                &[vec!["agent-1"], vec!["agent-1"]]
            ),
            None
        );
    }

    #[test]
    fn pending_focus_follows_session_when_tabs_reorder_during_open() {
        let pending = PendingSession {
            space: "space-b".into(),
            title: "agent-2".into(),
            tab: Some(1),
        };
        assert_eq!(
            pending_session_slot(
                &pending,
                Some("space-b"),
                &[vec!["agent-2"], vec!["agent-1"]]
            ),
            Some((0, 0))
        );
        assert_eq!(
            pending_session_slot(
                &pending,
                Some("space-b"),
                &[vec!["agent-1"], vec!["agent-2"]]
            ),
            Some((1, 0))
        );
        assert_eq!(
            pending_session_slot(
                &pending,
                Some("space-b"),
                &[vec!["agent-1"], vec!["agent-3"]]
            ),
            None
        );
    }

    #[test]
    fn saved_identity_uses_owned_live_names_and_keeps_offline_remote_fallbacks() {
        let space: prismattyc_mux::SavedSpace = serde_json::from_value(serde_json::json!({
            "version": 2, "id":"owner-a", "saved_at_unix":0, "sessions":[],
            "tabs":[{"title":"main", "sessions":["agent-old"]}, {"title":"Notes", "sessions":["offline"]},
                {"title":"Remote", "sessions":["remote-session"]}, {"title":"main", "sessions":["8"]}]
        })).unwrap();
        let snapshot = prismattyc_mux::Snapshot {
            sequence: 1,
            sessions: vec![
                prismattyc_mux::SessionSnapshot {
                    id: 7,
                    name: "Agent Renamed".into(),
                    agent_id: None,
                    space_id: Some("owner-a".into()),
                    windows: vec![],
                },
                prismattyc_mux::SessionSnapshot {
                    id: 8,
                    name: "foreign".into(),
                    agent_id: None,
                    space_id: Some("owner-b".into()),
                    windows: vec![],
                },
                prismattyc_mux::SessionSnapshot {
                    id: 9,
                    name: "agent-new".into(),
                    agent_id: None,
                    space_id: Some("owner-a".into()),
                    windows: vec![],
                },
            ],
        };
        let (tabs, order) = saved_tabs(&space, Some(&snapshot));
        assert_eq!(
            tabs.iter()
                .map(|tab| tab.sessions[0].as_str())
                .collect::<Vec<_>>(),
            ["agent-old", "offline", "remote-session", "8"]
        );
        assert_eq!(
            order,
            [
                "agent-old",
                "offline",
                "remote-session",
                "8",
                "Agent Renamed",
                "agent-new"
            ]
        );
        assert_eq!(saved_tabs(&space, None).0[0].sessions, ["agent-old"]);
        let mut legacy = space.clone();
        legacy.id = None;
        assert_eq!(
            saved_tabs(&legacy, Some(&snapshot)).0[0].sessions,
            ["agent-old"]
        );
    }

    #[test]
    fn live_tabs_expand_handles_with_focus_and_activity() {
        let infos = [
            live_tab(
                "grid",
                true,
                vec![("build", true), ("review", false)],
                Some(1),
            ),
            live_tab("notes", false, vec![], None),
        ];
        let mails: Vec<Vec<u32>> = vec![vec![0, 3], vec![]];
        let attentions: Vec<Vec<bool>> = vec![vec![false, true], vec![]];
        let tabs: Vec<LiveTab<'_>> = infos
            .iter()
            .zip(mails.iter())
            .zip(attentions.iter())
            .map(|((info, mail), attention)| LiveTab {
                info,
                session_names: &[],
                mail,
                attention,
            })
            .collect();
        let tree = SidebarTree::build(&[SpaceInput {
            name: "lab",
            current: true,
            collapsed: false,
            attention: 0,
            tabs: TabsSource::Live(&tabs),
        }]);
        assert_eq!(tree.spaces.len(), 1);
        let space = &tree.spaces[0];
        assert!(space.current && !space.collapsed);
        assert_eq!(space.tabs.len(), 2);
        let grid = &space.tabs[0];
        assert!(grid.selected);
        assert_eq!(grid.panes.len(), 2);
        assert!(!grid.panes[0].focused && grid.panes[0].active);
        assert!(grid.panes[1].focused && !grid.panes[1].active);
        assert_eq!(grid.panes[0].mail, 0);
        assert_eq!(grid.panes[1].mail, 3, "waiting mail rides the pane");
        assert!(grid.panes[1].attention);
        assert!(!grid.panes[0].attention);
        // A single-pane tab shows its own title as the one pane.
        let notes = &space.tabs[1];
        assert_eq!(notes.panes.len(), 1);
        assert_eq!(notes.panes[0].title, "notes");
        assert!(!notes.panes[0].focused, "focus sits in the other tab");
    }

    #[test]
    fn visible_rows_flatten_in_paint_order() {
        let tabs = vec![SavedSpaceTab {
            title: "main".to_string(),
            sessions: vec!["shell".to_string()],
            layout: None,
            fixed_name: None,
        }];
        let tree = SidebarTree::build(&[
            SpaceInput {
                name: "lab",
                current: true,
                collapsed: false,
                attention: 0,
                tabs: TabsSource::Saved {
                    tabs: &tabs,
                    extra_sessions: &[],
                },
            },
            SpaceInput {
                name: "mail",
                current: false,
                collapsed: true,
                attention: 0,
                tabs: TabsSource::Saved {
                    tabs: &tabs,
                    extra_sessions: &[],
                },
            },
        ]);
        let rows = visible_rows(&tree);
        assert_eq!(
            rows.iter()
                .map(|row| (row.depth, row.kind, row.space))
                .collect::<Vec<_>>(),
            vec![
                (0, RowKind::Space, 0),
                (1, RowKind::Tab, 0),
                (0, RowKind::Space, 1),
            ],
            "a single-pane session does not get a pane row"
        );
        let icons = icon_rows(&tree);
        assert!(
            icons.len() > rows.len(),
            "a collapsed space still contributes its sessions"
        );
        assert!(icons
            .iter()
            .any(|row| row.space == 1 && row.kind == RowKind::Tab));
        assert!(
            !icons
                .iter()
                .any(|row| row.kind == RowKind::Pane && row.space == 0),
            "solo panes hide under the session row in the icon strip too"
        );
    }

    #[test]
    fn multi_pane_tab_keeps_pane_rows_in_sidebar_and_icon_strip() {
        let tabs = vec![SavedSpaceTab {
            title: "grid".to_string(),
            sessions: vec!["build".to_string(), "review".to_string()],
            layout: None,
            fixed_name: None,
        }];
        let tree = SidebarTree::build(&[SpaceInput {
            name: "lab",
            current: true,
            collapsed: false,
            attention: 0,
            tabs: TabsSource::Saved {
                tabs: &tabs,
                extra_sessions: &[],
            },
        }]);
        let pane_rows: Vec<_> = visible_rows(&tree)
            .into_iter()
            .filter(|row| row.kind == RowKind::Pane)
            .collect();
        assert_eq!(pane_rows.len(), 2);
        assert_eq!(
            icon_rows(&tree)
                .into_iter()
                .filter(|row| row.kind == RowKind::Pane)
                .count(),
            2
        );
    }

    #[test]
    fn pane_row_visibility_tracks_live_split_and_close() {
        let split = live_tab(
            "grid",
            true,
            vec![("build", true), ("review", false)],
            Some(0),
        );
        let solo = live_tab("grid", true, vec![("build", true)], Some(0));
        let build = |info: &TabInfo| {
            SidebarTree::build(&[SpaceInput {
                name: "lab",
                current: true,
                collapsed: false,
                attention: 0,
                tabs: TabsSource::Live(&[LiveTab {
                    info,
                    session_names: &[],
                    mail: &[],
                    attention: &[],
                }]),
            }])
        };
        assert_eq!(
            visible_rows(&build(&split))
                .into_iter()
                .filter(|row| row.kind == RowKind::Pane)
                .count(),
            2,
            "splitting adds pane rows"
        );
        assert!(
            visible_rows(&build(&solo))
                .into_iter()
                .all(|row| row.kind != RowKind::Pane),
            "closing back to one pane removes pane rows"
        );
    }

    #[test]
    fn row_needs_you_marks_space_tab_and_pane_rows() {
        let tree = SidebarTree {
            spaces: vec![SpaceNode {
                name: "lab".to_string(),
                current: true,
                collapsed: false,
                attention: 2,
                tabs: vec![TabNode {
                    title: "grid".to_string(),
                    selected: true,
                    unseen: false,
                    attention: true,
                    zoomed: false,
                    panes: vec![
                        PaneNode {
                            title: "a".into(),
                            focused: false,
                            active: false,
                            mail: 0,
                            attention: false,
                        },
                        PaneNode {
                            title: "b".into(),
                            focused: true,
                            active: false,
                            mail: 0,
                            attention: true,
                        },
                    ],
                    fixed_name: None,
                }],
            }],
        };
        let rows = visible_rows(&tree);
        assert_eq!(row_needs_you(&tree, &rows[0]), 2);
        assert_eq!(row_needs_you(&tree, &rows[1]), 1);
        assert_eq!(row_needs_you(&tree, &rows[2]), 0);
        assert_eq!(row_needs_you(&tree, &rows[3]), 1);
    }

    #[test]
    fn saved_spaces_expand_files_and_hide_collapsed_tabs() {
        let tabs = vec![SavedSpaceTab {
            title: "main".to_string(),
            sessions: vec!["shell".to_string(), "logs".to_string()],
            layout: None,
            fixed_name: None,
        }];
        let extra = vec!["scratch".to_string()];
        let tree = SidebarTree::build(&[
            SpaceInput {
                name: "lab",
                current: true,
                collapsed: false,
                attention: 2,
                tabs: TabsSource::Saved {
                    tabs: &tabs,
                    extra_sessions: &[],
                },
            },
            SpaceInput {
                name: "mail",
                current: false,
                collapsed: true,
                attention: 0,
                tabs: TabsSource::Saved {
                    tabs: &[],
                    extra_sessions: &extra,
                },
            },
        ]);
        let lab = &tree.spaces[0];
        assert_eq!(lab.attention, 2);
        assert_eq!(lab.tabs.len(), 1);
        assert_eq!(lab.tabs[0].panes.len(), 2);
        assert!(lab.tabs[0].panes.iter().all(|pane| !pane.focused));
        let mail = &tree.spaces[1];
        assert!(!mail.current && mail.collapsed);
        // An untabbed session becomes its own tab, hidden while collapsed.
        assert_eq!(mail.tabs.len(), 1);
        assert_eq!(mail.tabs[0].title, "scratch");
        assert!(mail.visible_tabs().is_empty());
        assert_eq!(lab.visible_tabs().len(), 1);
    }

    #[test]
    fn session_highlight_follows_the_focused_pane_in_the_current_space() {
        assert!(session_row_selected(true, true));
        assert!(!session_row_selected(true, false));
        assert!(
            !session_row_selected(false, true),
            "another space does not show a live focus highlight"
        );
    }

    #[test]
    fn pending_session_lands_on_the_matching_title_after_the_space_switches() {
        let mut notes = live_tab("notes", true, vec![], None);
        notes.pane_title = Some("composer-2".to_string());
        assert_eq!(
            session_titles(&notes),
            vec!["composer-2".to_string()],
            "a single pane uses the same title the row paints"
        );
        let pending = PendingSession {
            space: "mail".to_string(),
            title: "composer-2".to_string(),
            tab: None,
        };
        let tabs = [vec!["prismattyc-1", "prismattyc-3"], vec!["composer-2"]];
        assert_eq!(
            pending_session_slot(&pending, Some("lab"), &tabs),
            None,
            "the space being left is not focused"
        );
        assert_eq!(
            pending_session_slot(&pending, Some("mail"), &tabs),
            Some((1, 0))
        );
        assert_eq!(pending_session_slot(&pending, None, &tabs), None);
        let missing = PendingSession {
            space: "mail".to_string(),
            title: String::new(),
            tab: None,
        };
        assert_eq!(pending_session_slot(&missing, Some("mail"), &tabs), None);
    }
}
