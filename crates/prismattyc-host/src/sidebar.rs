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
    /// Live tab from the attached space; `mail` holds per-pane waiting
    /// counts aligned with the layout panes (empty means none).
    pub fn live(info: &TabInfo, mail: &[u32]) -> Self {
        let panes = if info.handles == 0 {
            vec![PaneNode {
                title: info
                    .pane_title
                    .clone()
                    .unwrap_or_else(|| info.title.clone()),
                focused: info.selected,
                active: info.active,
                mail: mail.first().copied().unwrap_or(0),
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
                })
                .collect(),
        }
    }
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
    rows
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
                    .map(|tab| TabNode::live(tab.info, tab.mail))
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
        let infos = vec![
            live_tab(
                "grid",
                true,
                vec![("build", true), ("review", false)],
                Some(1),
            ),
            live_tab("notes", false, vec![], None),
        ];
        let mails: Vec<Vec<u32>> = vec![vec![0, 3], vec![]];
        let tabs: Vec<LiveTab<'_>> = infos
            .iter()
            .zip(mails.iter())
            .map(|(info, mail)| LiveTab { info, mail })
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
                (2, RowKind::Pane, 0),
                (0, RowKind::Space, 1),
            ]
        );
    }

    #[test]
    fn saved_spaces_expand_files_and_hide_collapsed_tabs() {
        let tabs = vec![SavedSpaceTab {
            title: "main".to_string(),
            sessions: vec!["shell".to_string(), "logs".to_string()],
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
}
