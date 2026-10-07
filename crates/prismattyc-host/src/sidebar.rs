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
            selected: info.selected,
            unseen: info.unseen,
            attention: info.attention,
            zoomed: info.zoomed,
            panes,
        }
    }

    /// Saved tab from another space's file: structure only, no live state.
    pub fn saved(title: &str, sessions: &[String]) -> Self {
        TabNode {
            title: title.to_string(),
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
pub fn show_pane_rows(pane_count: usize) -> bool {
    pane_count > 1
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
            if show_pane_rows(tab.panes.len()) {
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
            if show_pane_rows(tab.panes.len()) {
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
            let tabs = match &space.tabs {
                TabsSource::Live(tabs) => tabs
                    .iter()
                    .map(|tab| TabNode::live(tab.info, tab.mail, tab.attention))
                    .collect(),
                TabsSource::Saved {
                    tabs,
                    extra_sessions,
                } => {
                    let mut nodes: Vec<TabNode> = tabs
                        .iter()
                        .map(|tab| TabNode::saved(&tab.title, &tab.sessions))
                        .collect();
                    let known: Vec<&str> = tabs
                        .iter()
                        .flat_map(|tab| tab.sessions.iter().map(String::as_str))
                        .collect();
                    for session in *extra_sessions {
                        if !known.contains(&session.as_str()) {
                            nodes.push(TabNode::saved(session, std::slice::from_ref(session)));
                        }
                    }
                    nodes
                }
            };
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
        }
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
        };
        assert_eq!(pending_session_slot(&missing, Some("mail"), &tabs), None);
    }
}
