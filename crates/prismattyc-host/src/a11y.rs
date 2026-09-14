//! AccessKit chrome tree for `prismattyc-host` (PT-173 / PT-174 / PT-175, ADR-0016).
//!
//! Tree shape is a pure function of [`ChromeSnapshot`]. Window, mux, and
//! AccessKit side effects stay in the caller. The focused viewport is one
//! document ([`viewport_document`]); there is no per-cell node.

use accesskit::{
    Action, Live, Node, NodeId, Role, TextPosition, TextSelection, Tree, TreeId, TreeUpdate,
};

pub(crate) const WINDOW_ID: u64 = 1;
pub(crate) const TAB_LIST_ID: u64 = 2;
pub(crate) const SCROLLBAR_ID: u64 = 3;
pub(crate) const RAIL_ID: u64 = 4;
pub(crate) const OVERLAY_ID: u64 = 5;
pub(crate) const DOCUMENT_ID: u64 = 6;
pub(crate) const TEXT_RUN_ID: u64 = 7;
pub(crate) const LIVE_ID: u64 = 8;
pub(crate) const CAPTION_ID: u64 = 9;

/// Minimum gap between cursor-line announces (ADR-0016 D-A5 coalesce).
pub(crate) const GRID_ANNOUNCE_GAP_MS: u64 = 400;
/// Trailing mark so two identical utterances still change the live value.
const LIVE_REPEAT_MARK: char = '\u{200B}';
pub(crate) const TAB_BASE: u64 = 100;
pub(crate) const PANE_BASE: u64 = 200;
pub(crate) const OVERLAY_ROW_BASE: u64 = 400;
pub(crate) const RAIL_CHIP_BASE: u64 = 700;

const MAX_TABS: u64 = 64;
const MAX_PANES: u64 = 64;
const MAX_OVERLAY_ROWS: u64 = 64;
const MAX_RAIL_CHIPS: u64 = 64;

/// One tab chip and the panes it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabSnap {
    pub title: String,
    pub selected: bool,
    pub description: String,
    pub panes: Vec<PaneSnap>,
}

/// A pane name inside a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneSnap {
    pub name: String,
    pub focused: bool,
}

/// Title column for a pane in the chrome tree (PT-191).
pub(crate) fn pane_working_name(title: &str, working: bool) -> String {
    if working {
        format!("{title}, working")
    } else {
        title.to_string()
    }
}

/// Exclusive host overlay. `None` means the tab strip has focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OverlayKind {
    None,
    SessionPrompt {
        name: String,
        renaming: bool,
        allow_blank: bool,
        selected: usize,
    },
    RestorePrompt {
        rows: Vec<String>,
        selected: usize,
    },
    Splash {
        rows: Vec<String>,
        selected: usize,
    },
    Palette {
        rows: Vec<String>,
        selected: usize,
    },
    Find {
        query: String,
    },
    Theme {
        rows: Vec<String>,
        selected: usize,
    },
    Choices {
        title: String,
        rows: Vec<String>,
        selected: usize,
    },
}

impl OverlayKind {
    fn title(&self) -> &str {
        match self {
            OverlayKind::None => "",
            OverlayKind::SessionPrompt { renaming, .. } => {
                if *renaming {
                    "Rename session"
                } else {
                    "New session"
                }
            }
            OverlayKind::RestorePrompt { .. } => "Restore last space?",
            OverlayKind::Splash { .. } => "Prismattyc — splash",
            OverlayKind::Palette { .. } => "Prismattyc — command palette",
            OverlayKind::Find { .. } => "Prismattyc — find",
            OverlayKind::Theme { .. } => "Prismattyc — theme settings",
            OverlayKind::Choices { title, .. } => title,
        }
    }

    fn is_some(&self) -> bool {
        !matches!(self, OverlayKind::None)
    }
}

/// Focused-pane viewport as one document (ADR-0016 D-A4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentSnap {
    pub text: String,
    pub caret: usize,
    pub sel_anchor: Option<usize>,
    pub sel_focus: Option<usize>,
}

/// Plain chrome state. Do not pass `HostState` here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChromeSnapshot {
    pub window_title: String,
    pub tabs: Vec<TabSnap>,
    pub overlay: OverlayKind,
    pub scroll: Option<String>,
    pub rail: Vec<String>,
    pub rail_current: Option<usize>,
    pub document: Option<DocumentSnap>,
    pub live: Option<LiveSnap>,
    pub caption: Option<String>,
}

/// One live-region utterance (ADR-0016 D-A5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSnap {
    pub text: String,
    pub assertive: bool,
}

/// Last cursor/pane we announced, for coalesce and live-value repeats.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AnnounceMemory {
    pub cursor_row: Option<usize>,
    pub pane: Option<u64>,
    pub last_grid_ms: Option<u64>,
    /// Last spoken text without [`LIVE_REPEAT_MARK`].
    pub last_spoken: Option<String>,
    /// Whether the last published live value carried [`LIVE_REPEAT_MARK`].
    pub zwsp: bool,
}

/// Plain facts for one announce decision. No `HostState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnounceFacts {
    pub enabled: bool,
    pub now_ms: u64,
    pub mail: Option<String>,
    pub attention: Option<String>,
    pub notice: Option<String>,
    pub selection: Option<String>,
    pub cursor_row: Option<usize>,
    pub cursor_line: String,
    pub pane: u64,
}

/// Pick at most one live-region string. Mail, attention, and pane-title
/// notices win and may join.
/// Cursor-line announces coalesce to [`GRID_ANNOUNCE_GAP_MS`].
pub(crate) fn announce_decision(
    facts: &AnnounceFacts,
    memory: &AnnounceMemory,
) -> (Option<LiveSnap>, AnnounceMemory) {
    let pane_changed = memory.pane != Some(facts.pane);
    let row_changed = facts.cursor_row != memory.cursor_row;
    let mut next = AnnounceMemory {
        cursor_row: facts.cursor_row.or(memory.cursor_row),
        pane: Some(facts.pane),
        last_grid_ms: memory.last_grid_ms,
        last_spoken: memory.last_spoken.clone(),
        zwsp: memory.zwsp,
    };
    if !facts.enabled {
        return (None, next);
    }
    let mut parts = Vec::new();
    if let Some(mail) = facts.mail.as_deref() {
        if !mail.is_empty() {
            parts.push(mail.to_string());
        }
    }
    if let Some(attention) = facts.attention.as_deref() {
        if !attention.is_empty() {
            parts.push(attention.to_string());
        }
    }
    if let Some(notice) = facts.notice.as_deref() {
        if !notice.is_empty() {
            parts.push(notice.to_string());
        }
    }
    if !parts.is_empty() {
        let text = decorate_live_value(&parts.join(". "), memory, &mut next);
        return (
            Some(LiveSnap {
                text,
                assertive: true,
            }),
            next,
        );
    }
    if let Some(selection) = facts.selection.as_deref() {
        if !selection.is_empty() {
            let text = decorate_live_value(selection, memory, &mut next);
            return (
                Some(LiveSnap {
                    text,
                    assertive: false,
                }),
                next,
            );
        }
    }
    let grid_due = pane_changed
        || row_changed
            && memory
                .last_grid_ms
                .is_none_or(|at| facts.now_ms.saturating_sub(at) >= GRID_ANNOUNCE_GAP_MS);
    if grid_due && (pane_changed || row_changed) && !facts.cursor_line.is_empty() {
        next.last_grid_ms = Some(facts.now_ms);
        let text = decorate_live_value(&facts.cursor_line, memory, &mut next);
        return (
            Some(LiveSnap {
                text,
                assertive: false,
            }),
            next,
        );
    }
    (None, next)
}

/// Publish `text`, toggling a trailing ZWSP when it matches the last utterance.
fn decorate_live_value(text: &str, memory: &AnnounceMemory, next: &mut AnnounceMemory) -> String {
    let same = memory.last_spoken.as_deref() == Some(text);
    next.zwsp = if same { !memory.zwsp } else { false };
    next.last_spoken = Some(text.to_string());
    if next.zwsp {
        format!("{text}{LIVE_REPEAT_MARK}")
    } else {
        text.to_string()
    }
}

/// Test-facing node. Roles are AccessKit names as strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeNode {
    pub id: u64,
    pub role: &'static str,
    pub name: String,
    pub description: String,
    pub selected: bool,
    pub children: Vec<u64>,
    pub value: String,
    pub caret: Option<usize>,
    pub sel_anchor: Option<usize>,
    pub sel_focus: Option<usize>,
}

fn chrome_node(
    id: u64,
    role: &'static str,
    name: impl Into<String>,
    selected: bool,
    children: Vec<u64>,
) -> TreeNode {
    TreeNode {
        id,
        role,
        name: name.into(),
        description: String::new(),
        selected,
        children,
        value: String::new(),
        caret: None,
        sel_anchor: None,
        sel_focus: None,
    }
}

/// Chars written by cells `[0, col)` on one row.
///
/// `cell_chars[i]` is how many Unicode scalars cell `i` contributed (0 for
/// a wide-character continuation). Use this before [`viewport_document`] so
/// a CJK/emoji pair does not push the caret one past the glyph.
pub(crate) fn chars_before_cell(cell_chars: &[usize], col: usize) -> usize {
    cell_chars.iter().take(col).sum()
}

/// Viewport lines plus cursor/selection → one document value.
///
/// `lines` are cell graphemes per visible row (not trimmed). Trailing spaces
/// are stripped, matching `Screen::extract_text`. `cursor` and `selection`
/// use viewport row and a **character** index on that row (see
/// [`chars_before_cell`]). A missing cursor (scrolled off the live grid)
/// places the caret at 0.
pub(crate) fn viewport_document(
    lines: &[String],
    cursor: Option<(usize, usize)>,
    selection: Option<((usize, usize), (usize, usize))>,
) -> DocumentSnap {
    let trimmed: Vec<String> = lines
        .iter()
        .map(|line| line.trim_end_matches(' ').to_string())
        .collect();
    let text = trimmed.join("\n");
    let caret = match cursor {
        Some((row, col)) => cell_offset(&trimmed, row, col),
        None => 0,
    };
    let (sel_anchor, sel_focus) = match selection {
        Some((anchor, focus)) => (
            Some(cell_offset(&trimmed, anchor.0, anchor.1)),
            Some(cell_offset(&trimmed, focus.0, focus.1)),
        ),
        None => (None, None),
    };
    DocumentSnap {
        text,
        caret,
        sel_anchor,
        sel_focus,
    }
}

fn cell_offset(lines: &[String], row: usize, col: usize) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let row = row.min(lines.len() - 1);
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        if index == row {
            return offset + col.min(line.chars().count());
        }
        offset += line.chars().count() + 1;
    }
    offset
}

/// Which host action a Click on `id` should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChromeAction {
    SelectTab(usize),
    OverlayActivate(usize),
    OpenSpace(usize),
}

/// Join badge words for a tab description.
pub(crate) fn tab_description(mail: u32, unseen: bool, attention: bool) -> String {
    let mut parts = Vec::new();
    if mail > 0 {
        parts.push(format!("{mail} mail"));
    }
    if unseen {
        parts.push("unseen".into());
    }
    if attention {
        parts.push("attention".into());
    }
    parts.join(", ")
}

/// Build the chrome tree. Focus is the overlay when one is open, else the
/// focused-pane document, else the selected tab, else the window.
pub(crate) fn build_chrome_tree(snap: &ChromeSnapshot) -> (u64, Vec<TreeNode>) {
    let mut nodes = Vec::new();
    let mut window_children = vec![TAB_LIST_ID];

    let tab_ids: Vec<u64> = (0..snap.tabs.len()).map(|i| TAB_BASE + i as u64).collect();
    nodes.push(chrome_node(
        TAB_LIST_ID,
        "TabList",
        "tabs",
        false,
        tab_ids.clone(),
    ));

    let mut focus = WINDOW_ID;
    for (index, tab) in snap.tabs.iter().enumerate() {
        let id = TAB_BASE + index as u64;
        let pane_ids: Vec<u64> = (0..tab.panes.len()).map(|p| pane_id(index, p)).collect();
        if tab.selected && !snap.overlay.is_some() {
            focus = id;
        }
        let mut tab_node = chrome_node(id, "Tab", tab.title.clone(), tab.selected, pane_ids);
        tab_node.description = tab.description.clone();
        nodes.push(tab_node);
        for (p, pane) in tab.panes.iter().enumerate() {
            let pid = pane_id(index, p);
            if pane.focused && tab.selected && !snap.overlay.is_some() {
                focus = pid;
            }
            nodes.push(chrome_node(
                pid,
                "Pane",
                pane.name.clone(),
                pane.focused,
                Vec::new(),
            ));
        }
    }

    if let Some(label) = snap.scroll.as_deref() {
        window_children.push(SCROLLBAR_ID);
        nodes.push(chrome_node(
            SCROLLBAR_ID,
            "ScrollBar",
            label,
            false,
            Vec::new(),
        ));
    }

    if !snap.rail.is_empty() {
        window_children.push(RAIL_ID);
        let chips: Vec<u64> = (0..snap.rail.len())
            .map(|i| RAIL_CHIP_BASE + i as u64)
            .collect();
        nodes.push(chrome_node(
            RAIL_ID,
            "Complementary",
            "spaces",
            false,
            chips,
        ));
        for (i, name) in snap.rail.iter().enumerate() {
            nodes.push(chrome_node(
                RAIL_CHIP_BASE + i as u64,
                "Button",
                name.clone(),
                snap.rail_current == Some(i),
                Vec::new(),
            ));
        }
    }

    if let Some(doc) = snap.document.as_ref() {
        window_children.push(DOCUMENT_ID);
        if !snap.overlay.is_some() {
            focus = DOCUMENT_ID;
        }
        let mut document = chrome_node(
            DOCUMENT_ID,
            "Document",
            "terminal",
            false,
            vec![TEXT_RUN_ID],
        );
        document.value = doc.text.clone();
        document.caret = Some(doc.caret);
        document.sel_anchor = doc.sel_anchor;
        document.sel_focus = doc.sel_focus;
        nodes.push(document);
        let mut run = chrome_node(TEXT_RUN_ID, "TextRun", "", false, Vec::new());
        run.value = doc.text.clone();
        nodes.push(run);
    }

    window_children.push(LIVE_ID);
    let (role, value) = match snap.live.as_ref() {
        Some(live) if !live.text.is_empty() => (
            if live.assertive { "Alert" } else { "Status" },
            live.text.clone(),
        ),
        _ => ("Status", String::new()),
    };
    let mut live_node = chrome_node(LIVE_ID, role, "announce", false, Vec::new());
    live_node.value = value;
    nodes.push(live_node);

    if let Some(caption) = snap.caption.as_deref().filter(|text| !text.is_empty()) {
        window_children.push(CAPTION_ID);
        let mut caption_node = chrome_node(CAPTION_ID, "Label", "caption", false, Vec::new());
        caption_node.value = caption.to_string();
        nodes.push(caption_node);
    }

    if snap.overlay.is_some() {
        window_children.push(OVERLAY_ID);
        let (rows, selected, extra) = overlay_rows(&snap.overlay);
        let row_ids: Vec<u64> = (0..rows.len())
            .map(|i| OVERLAY_ROW_BASE + i as u64)
            .collect();
        focus = selected
            .and_then(|i| row_ids.get(i).copied())
            .unwrap_or(OVERLAY_ID);
        let mut dialog = chrome_node(OVERLAY_ID, "Dialog", snap.overlay.title(), false, row_ids);
        dialog.description = extra;
        nodes.push(dialog);
        for (i, row) in rows.iter().enumerate() {
            nodes.push(chrome_node(
                OVERLAY_ROW_BASE + i as u64,
                "Button",
                row.clone(),
                selected == Some(i),
                Vec::new(),
            ));
        }
    }

    nodes.insert(
        0,
        chrome_node(
            WINDOW_ID,
            "Window",
            snap.window_title.clone(),
            false,
            window_children,
        ),
    );
    (focus, nodes)
}

fn pane_id(tab: usize, pane: usize) -> u64 {
    PANE_BASE + (tab as u64) * MAX_PANES + pane as u64
}

fn overlay_rows(kind: &OverlayKind) -> (Vec<String>, Option<usize>, String) {
    match kind {
        OverlayKind::None => (Vec::new(), None, String::new()),
        OverlayKind::SessionPrompt {
            name,
            renaming,
            allow_blank,
            selected,
        } => {
            let mut rows = vec![if *renaming {
                "Rename".into()
            } else {
                "Create".into()
            }];
            if *allow_blank {
                rows.push("Blank terminal".into());
            }
            rows.push("Cancel".into());
            (rows, Some(*selected), name.clone())
        }
        OverlayKind::RestorePrompt { rows, selected }
        | OverlayKind::Splash { rows, selected }
        | OverlayKind::Palette { rows, selected }
        | OverlayKind::Theme { rows, selected }
        | OverlayKind::Choices { rows, selected, .. } => {
            let index = (*selected).min(rows.len().saturating_sub(1));
            (rows.clone(), Some(index), String::new())
        }
        OverlayKind::Find { query } => (Vec::new(), None, query.clone()),
    }
}

/// Click target for a node id, if that node is activatable chrome.
pub(crate) fn action_for(id: u64, snap: &ChromeSnapshot) -> Option<ChromeAction> {
    if (TAB_BASE..TAB_BASE + MAX_TABS).contains(&id) {
        let index = (id - TAB_BASE) as usize;
        if index < snap.tabs.len() {
            return Some(ChromeAction::SelectTab(index));
        }
    }
    if (OVERLAY_ROW_BASE..OVERLAY_ROW_BASE + MAX_OVERLAY_ROWS).contains(&id) {
        let index = (id - OVERLAY_ROW_BASE) as usize;
        let (rows, _, _) = overlay_rows(&snap.overlay);
        if index < rows.len() {
            return Some(ChromeAction::OverlayActivate(index));
        }
    }
    if (RAIL_CHIP_BASE..RAIL_CHIP_BASE + MAX_RAIL_CHIPS).contains(&id) {
        let index = (id - RAIL_CHIP_BASE) as usize;
        if index < snap.rail.len() {
            return Some(ChromeAction::OpenSpace(index));
        }
    }
    None
}

/// AccessKit update from a chrome snapshot. Always a full tree.
pub(crate) fn tree_update(snap: &ChromeSnapshot) -> TreeUpdate {
    let (focus, nodes) = build_chrome_tree(snap);
    let mut tree = Tree::new(NodeId(WINDOW_ID));
    tree.toolkit_name = Some("prismattyc-host".into());
    TreeUpdate {
        nodes: nodes.into_iter().map(to_accesskit).collect(),
        tree: Some(tree),
        tree_id: TreeId::ROOT,
        focus: NodeId(focus),
    }
}

fn to_accesskit(node: TreeNode) -> (NodeId, Node) {
    let role = match node.role {
        "Window" => Role::Window,
        "TabList" => Role::TabList,
        "Tab" => Role::Tab,
        "Pane" => Role::Pane,
        "ScrollBar" => Role::ScrollBar,
        "Complementary" => Role::Complementary,
        "Dialog" => Role::Dialog,
        "Button" => Role::Button,
        "Document" => Role::Document,
        "TextRun" => Role::TextRun,
        "Status" => Role::Status,
        "Alert" => Role::Alert,
        "Label" => Role::Label,
        _ => Role::Unknown,
    };
    let mut ak = Node::new(role);
    if !node.name.is_empty() {
        ak.set_label(node.name);
    }
    if !node.description.is_empty() {
        ak.set_description(node.description);
    }
    if !node.value.is_empty() || matches!(role, Role::Status | Role::Alert) {
        ak.set_value(node.value.clone());
    }
    if node.selected {
        ak.set_selected(true);
    }
    if !node.children.is_empty() {
        ak.set_children(node.children.into_iter().map(NodeId).collect::<Vec<_>>());
    }
    if role == Role::Status {
        ak.set_live(Live::Polite);
    }
    if role == Role::Alert {
        ak.set_live(Live::Assertive);
    }
    if role == Role::TextRun {
        let lengths: Vec<u8> = node.value.chars().map(|c| c.len_utf8() as u8).collect();
        ak.set_character_lengths(lengths);
    }
    if role == Role::Document {
        if let Some(caret) = node.caret {
            let run = NodeId(TEXT_RUN_ID);
            let anchor = TextPosition {
                node: run,
                character_index: node.sel_anchor.unwrap_or(caret),
            };
            let focus = TextPosition {
                node: run,
                character_index: node.sel_focus.unwrap_or(caret),
            };
            ak.set_text_selection(TextSelection { anchor, focus });
        }
    }
    if matches!(role, Role::Tab | Role::Button) {
        ak.add_action(Action::Click);
        ak.add_action(Action::Focus);
    }
    (NodeId(node.id), ak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(title: &str, selected: bool, desc: &str, panes: Vec<PaneSnap>) -> TabSnap {
        TabSnap {
            title: title.into(),
            selected,
            description: desc.into(),
            panes,
        }
    }

    fn names(nodes: &[TreeNode]) -> Vec<(u64, &'static str, String, bool)> {
        nodes
            .iter()
            .map(|n| (n.id, n.role, n.name.clone(), n.selected))
            .collect()
    }

    #[test]
    fn window_and_tabs_are_real_nodes() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc — 2 tabs — 2 panes".into(),
            tabs: vec![
                tab("agents", true, "1 mail", vec![]),
                tab("logs", false, "unseen", vec![]),
            ],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, TAB_BASE);
        assert_eq!(
            names(&nodes),
            vec![
                (
                    WINDOW_ID,
                    "Window",
                    "Prismattyc — 2 tabs — 2 panes".into(),
                    false
                ),
                (TAB_LIST_ID, "TabList", "tabs".into(), false),
                (TAB_BASE, "Tab", "agents".into(), true),
                (TAB_BASE + 1, "Tab", "logs".into(), false),
                (LIVE_ID, "Status", "announce".into(), false),
            ]
        );
        assert_eq!(nodes[0].children, vec![TAB_LIST_ID, LIVE_ID]);
        assert!(nodes[4].value.is_empty());
        assert_eq!(nodes[1].children, vec![TAB_BASE, TAB_BASE + 1]);
        assert_eq!(nodes[2].description, "1 mail");
        assert_eq!(nodes[3].description, "unseen");
        assert_eq!(
            action_for(TAB_BASE + 1, &snap),
            Some(ChromeAction::SelectTab(1))
        );
    }

    #[test]
    fn focused_pane_takes_focus_when_tab_has_handles() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab(
                "main",
                true,
                "",
                vec![
                    PaneSnap {
                        name: "left".into(),
                        focused: false,
                    },
                    PaneSnap {
                        name: "right".into(),
                        focused: true,
                    },
                ],
            )],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, pane_id(0, 1));
        let panes: Vec<_> = nodes
            .iter()
            .filter(|n| n.role == "Pane")
            .map(|n| (n.name.as_str(), n.selected))
            .collect();
        assert_eq!(panes, vec![("left", false), ("right", true)]);
    }

    #[test]
    fn overlay_is_a_dialog_and_owns_focus() {
        let mut snap = ChromeSnapshot {
            window_title: "Prismattyc — command palette".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::Palette {
                rows: vec!["split_right".into(), "find".into()],
                selected: 1,
            },
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, OVERLAY_ROW_BASE + 1);
        let dialog = nodes.iter().find(|n| n.role == "Dialog").unwrap();
        assert_eq!(dialog.name, "Prismattyc — command palette");
        assert_eq!(
            action_for(OVERLAY_ROW_BASE, &snap),
            Some(ChromeAction::OverlayActivate(0))
        );
        snap.overlay = OverlayKind::RestorePrompt {
            rows: vec!["Restore".into(), "Start fresh".into()],
            selected: 1,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, OVERLAY_ROW_BASE + 1);
        assert_eq!(
            nodes.iter().find(|n| n.role == "Dialog").unwrap().name,
            "Restore last space?"
        );
        assert_eq!(
            overlay_rows(&snap.overlay),
            (
                vec!["Restore".into(), "Start fresh".into()],
                Some(1),
                String::new()
            )
        );
        assert_eq!(
            action_for(OVERLAY_ROW_BASE + 1, &snap),
            Some(ChromeAction::OverlayActivate(1))
        );
    }

    #[test]
    fn find_dialog_carries_the_query_in_description() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::Find {
                query: "TODO".into(),
            },
            scroll: Some("4/20".into()),
            rail: vec!["work".into()],
            rail_current: Some(0),
            document: None,
            live: None,
            caption: None,
        };
        let (_, nodes) = build_chrome_tree(&snap);
        let find = nodes.iter().find(|n| n.role == "Dialog").unwrap();
        assert_eq!(find.description, "TODO");
        let bar = nodes.iter().find(|n| n.role == "ScrollBar").unwrap();
        assert_eq!(bar.name, "4/20");
        let rail = nodes.iter().find(|n| n.role == "Complementary").unwrap();
        assert_eq!(rail.name, "spaces");
        assert!(
            nodes
                .iter()
                .find(|n| n.id == RAIL_CHIP_BASE)
                .unwrap()
                .selected
        );
        assert_eq!(
            action_for(RAIL_CHIP_BASE, &snap),
            Some(ChromeAction::OpenSpace(0))
        );
    }

    #[test]
    fn tab_description_joins_badges() {
        assert_eq!(tab_description(0, false, false), "");
        assert_eq!(tab_description(2, true, true), "2 mail, unseen, attention");
    }

    #[test]
    fn unknown_id_is_not_an_action() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        assert_eq!(action_for(WINDOW_ID, &snap), None);
        assert_eq!(action_for(TAB_BASE + 9, &snap), None);
    }

    #[test]
    fn chrome_change_moves_tree_focus() {
        let tab_only = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let palette = ChromeSnapshot {
            overlay: OverlayKind::Palette {
                rows: vec!["find".into()],
                selected: 0,
            },
            ..tab_only.clone()
        };
        let (tab_focus, _) = build_chrome_tree(&tab_only);
        let (overlay_focus, _) = build_chrome_tree(&palette);
        assert_eq!(tab_focus, TAB_BASE);
        assert_eq!(overlay_focus, OVERLAY_ROW_BASE);
        assert_ne!(tab_focus, overlay_focus);
    }

    #[test]
    fn tree_update_names_the_toolkit() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let update = tree_update(&snap);
        assert_eq!(update.focus, NodeId(TAB_BASE));
        let tree = update.tree.expect("full tree");
        assert_eq!(tree.root, NodeId(WINDOW_ID));
        assert_eq!(tree.toolkit_name.as_deref(), Some("prismattyc-host"));
        assert!(update.nodes.iter().any(|(id, _)| *id == NodeId(WINDOW_ID)));
    }

    #[test]
    fn viewport_document_is_row_major_plain_text() {
        let doc = viewport_document(&["hello   ".into(), "world".into()], Some((1, 2)), None);
        assert_eq!(doc.text, "hello\nworld");
        assert_eq!(doc.caret, 8);
        assert_eq!(doc.sel_anchor, None);
    }

    #[test]
    fn viewport_document_maps_selection_and_offscreen_caret() {
        let selected = viewport_document(
            &["ab  ".into(), "cd".into()],
            Some((0, 0)),
            Some(((0, 0), (1, 2))),
        );
        assert_eq!(selected.text, "ab\ncd");
        assert_eq!(selected.caret, 0);
        assert_eq!(selected.sel_anchor, Some(0));
        assert_eq!(selected.sel_focus, Some(5));
        let scrolled = viewport_document(&["prompt".into()], None, None);
        assert_eq!(scrolled.caret, 0);
    }

    #[test]
    fn wide_cell_before_caret_does_not_shift_the_offset() {
        // 中 is one char and two cells; the continuation writes nothing.
        let cell_chars = [1, 0, 1];
        assert_eq!(chars_before_cell(&cell_chars, 0), 0);
        assert_eq!(chars_before_cell(&cell_chars, 2), 1);
        let doc = viewport_document(
            &["中a".into()],
            Some((0, chars_before_cell(&cell_chars, 2))),
            None,
        );
        assert_eq!(doc.text, "中a");
        assert_eq!(doc.caret, 1);
    }

    #[test]
    fn focused_viewport_is_one_document_not_cells() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab(
                "main",
                true,
                "",
                vec![
                    PaneSnap {
                        name: "left".into(),
                        focused: false,
                    },
                    PaneSnap {
                        name: "right".into(),
                        focused: true,
                    },
                ],
            )],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: Some(viewport_document(&["$ ls".into()], Some((0, 4)), None)),
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, DOCUMENT_ID);
        let docs: Vec<_> = nodes.iter().filter(|n| n.role == "Document").collect();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].value, "$ ls");
        assert_eq!(docs[0].caret, Some(4));
        assert_eq!(docs[0].children, vec![TEXT_RUN_ID]);
        assert_eq!(nodes.iter().filter(|n| n.role == "TextRun").count(), 1);
        let left = nodes.iter().find(|n| n.name == "left").unwrap();
        assert!(left.children.is_empty());
        assert!(left.value.is_empty());
    }

    #[test]
    fn overlay_keeps_focus_when_document_is_present() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc — command palette".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::Palette {
                rows: vec!["find".into()],
                selected: 0,
            },
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: Some(viewport_document(&["x".into()], Some((0, 0)), None)),
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, OVERLAY_ROW_BASE);
        assert!(nodes.iter().any(|n| n.role == "Document"));
    }

    #[test]
    fn pane_working_name_adds_the_busy_hint() {
        assert_eq!(pane_working_name("build", false), "build");
        assert_eq!(pane_working_name("build", true), "build, working");
    }

    fn facts(enabled: bool) -> AnnounceFacts {
        AnnounceFacts {
            enabled,
            now_ms: 1_000,
            mail: None,
            attention: None,
            notice: None,
            selection: None,
            cursor_row: Some(2),
            cursor_line: "prompt".into(),
            pane: 1,
        }
    }

    #[test]
    fn announce_off_is_silent() {
        let (live, _) = announce_decision(
            &AnnounceFacts {
                mail: Some("grok-pc: 1 mail".into()),
                attention: Some("Grok needs you: yes?".into()),
                ..facts(false)
            },
            &AnnounceMemory::default(),
        );
        assert_eq!(live, None);
    }

    #[test]
    fn mail_and_attention_join_and_beat_the_grid() {
        let (live, _) = announce_decision(
            &AnnounceFacts {
                mail: Some("grok-pc: 1 mail".into()),
                attention: Some("Grok needs you: yes?".into()),
                selection: Some("copied".into()),
                ..facts(true)
            },
            &AnnounceMemory::default(),
        );
        let live = live.expect("announce");
        assert!(live.assertive);
        assert_eq!(live.text, "grok-pc: 1 mail. Grok needs you: yes?");
    }

    #[test]
    fn pane_title_notice_joins_the_assertive_band() {
        let (live, _) = announce_decision(
            &AnnounceFacts {
                notice: Some("Waiting for you".into()),
                ..facts(true)
            },
            &AnnounceMemory::default(),
        );
        let live = live.expect("announce");
        assert!(live.assertive);
        assert_eq!(live.text, "Waiting for you");
    }

    #[test]
    fn cursor_line_coalesces_until_the_gap() {
        let first = facts(true);
        let (live, mem1) = announce_decision(&first, &AnnounceMemory::default());
        assert_eq!(live.unwrap().text, "prompt");
        let (again, _) = announce_decision(
            &AnnounceFacts {
                now_ms: 1_200,
                cursor_row: Some(3),
                cursor_line: "next".into(),
                ..first.clone()
            },
            &mem1,
        );
        assert_eq!(again, None);
        let (later, _) = announce_decision(
            &AnnounceFacts {
                now_ms: 1_000 + GRID_ANNOUNCE_GAP_MS,
                cursor_row: Some(3),
                cursor_line: "next".into(),
                ..first
            },
            &mem1,
        );
        assert_eq!(later.unwrap().text, "next");
    }

    #[test]
    fn live_region_is_status_or_alert() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: Some(LiveSnap {
                text: "grok-pc: 1 mail".into(),
                assertive: true,
            }),
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, TAB_BASE);
        let alert = nodes.iter().find(|n| n.role == "Alert").unwrap();
        assert_eq!(alert.value, "grok-pc: 1 mail");
        assert_eq!(alert.id, LIVE_ID);
    }

    #[test]
    fn identical_utterances_change_the_live_value() {
        let facts = AnnounceFacts {
            mail: Some("grok-pc: 1 mail".into()),
            ..facts(true)
        };
        let (first, mem) = announce_decision(&facts, &AnnounceMemory::default());
        let (again, mem2) = announce_decision(&facts, &mem);
        let first = first.expect("first");
        let again = again.expect("repeat");
        assert_eq!(first.text, "grok-pc: 1 mail");
        assert_eq!(again.text, "grok-pc: 1 mail\u{200B}");
        assert_ne!(first.text, again.text);
        let (third, _) = announce_decision(&facts, &mem2);
        assert_eq!(third.expect("third").text, "grok-pc: 1 mail");
    }

    #[test]
    fn silent_frame_keeps_an_empty_live_node() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: None,
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_ne!(focus, LIVE_ID);
        let live = nodes.iter().find(|n| n.id == LIVE_ID).unwrap();
        assert_eq!(live.role, "Status");
        assert_eq!(live.name, "announce");
        assert!(live.value.is_empty());
    }

    #[test]
    fn caption_is_a_label_and_does_not_steal_focus() {
        let snap = ChromeSnapshot {
            window_title: "Prismattyc".into(),
            tabs: vec![tab("main", true, "", vec![])],
            overlay: OverlayKind::None,
            scroll: None,
            rail: Vec::new(),
            rail_current: None,
            document: None,
            live: None,
            caption: Some("Split the pane to the right. Ctrl+Shift+\\".into()),
        };
        let (focus, nodes) = build_chrome_tree(&snap);
        assert_eq!(focus, TAB_BASE);
        assert_ne!(focus, CAPTION_ID);
        let caption = nodes.iter().find(|n| n.id == CAPTION_ID).unwrap();
        assert_eq!(caption.role, "Label");
        assert_eq!(caption.name, "caption");
        assert_eq!(caption.value, "Split the pane to the right. Ctrl+Shift+\\");
        assert!(nodes[0].children.contains(&CAPTION_ID));
        assert_eq!(action_for(CAPTION_ID, &snap), None);
    }
}
