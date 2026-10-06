//! Sidebar and vertical-rail context menus (issue #181).
//!
//! Pure row model: labels, action mapping, and confirmation gating. The host
//! maps [`RailSpaceAction`] / [`RailSessionAction`] / [`RailPaneAction`] onto
//! existing command paths.

use crate::palette::ContextMenuKind;

pub const RAIL_SPACE_ROWS: usize = 6;
pub const RAIL_SESSION_ROWS: usize = 4;
pub const RAIL_SESSION_SOLO_ROWS: usize = 5;
pub const RAIL_PANE_ROWS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailSpaceAction {
    OpenFocus,
    Rename,
    SaveNow,
    AddSession,
    CloseSpace,
    RemoveSaved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailSessionAction {
    Focus,
    Rename,
    Stop,
    MoveToSpace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailPaneAction {
    Focus,
    ClosePane,
}

/// Session row when the tab has a single pane: session plus pane actions (#181).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailSessionSoloAction {
    Focus,
    Rename,
    ClosePane,
    Stop,
    MoveToSpace,
}

pub fn row_count(kind: ContextMenuKind) -> Option<usize> {
    match kind {
        ContextMenuKind::RailSpace => Some(RAIL_SPACE_ROWS),
        ContextMenuKind::RailSession => Some(RAIL_SESSION_ROWS),
        ContextMenuKind::RailSessionSolo => Some(RAIL_SESSION_SOLO_ROWS),
        ContextMenuKind::RailPane => Some(RAIL_PANE_ROWS),
        _ => None,
    }
}

pub fn space_action(index: usize) -> Option<RailSpaceAction> {
    match index {
        0 => Some(RailSpaceAction::OpenFocus),
        1 => Some(RailSpaceAction::Rename),
        2 => Some(RailSpaceAction::SaveNow),
        3 => Some(RailSpaceAction::AddSession),
        4 => Some(RailSpaceAction::CloseSpace),
        5 => Some(RailSpaceAction::RemoveSaved),
        _ => None,
    }
}

/// Visible menu rows for a space. Close space applies only to the current space.
pub fn space_menu_row_indices(is_current_space: bool) -> impl Iterator<Item = usize> + Clone {
    (0..RAIL_SPACE_ROWS).filter(move |&index| is_current_space || index != 4)
}

pub fn space_menu_action_index(is_current_space: bool, visible_index: usize) -> Option<usize> {
    space_menu_row_indices(is_current_space).nth(visible_index)
}

pub fn session_action(index: usize) -> Option<RailSessionAction> {
    match index {
        0 => Some(RailSessionAction::Focus),
        1 => Some(RailSessionAction::Rename),
        2 => Some(RailSessionAction::Stop),
        3 => Some(RailSessionAction::MoveToSpace),
        _ => None,
    }
}

pub fn pane_action(index: usize) -> Option<RailPaneAction> {
    match index {
        0 => Some(RailPaneAction::Focus),
        1 => Some(RailPaneAction::ClosePane),
        _ => None,
    }
}

pub fn session_solo_action(index: usize) -> Option<RailSessionSoloAction> {
    match index {
        0 => Some(RailSessionSoloAction::Focus),
        1 => Some(RailSessionSoloAction::Rename),
        2 => Some(RailSessionSoloAction::ClosePane),
        3 => Some(RailSessionSoloAction::Stop),
        4 => Some(RailSessionSoloAction::MoveToSpace),
        _ => None,
    }
}

pub fn needs_confirmation(kind: ContextMenuKind, index: usize, confirmed: bool) -> bool {
    if confirmed {
        return false;
    }
    match kind {
        ContextMenuKind::RailSpace => matches!(index, 4 | 5),
        ContextMenuKind::RailSession => index == 2,
        ContextMenuKind::RailSessionSolo => matches!(index, 2 | 3),
        ContextMenuKind::RailPane => index == 1,
        _ => false,
    }
}

pub fn space_label(index: usize) -> Option<(&'static str, &'static str)> {
    match index {
        0 => Some(("Open / Focus", "switch to this space")),
        1 => Some(("Rename", "change the space name")),
        2 => Some(("Save now", "write the live layout to disk")),
        3 => Some(("Add session…", "create a new shell session")),
        4 => Some((
            "Close space…",
            "stop sessions and close this window’s views",
        )),
        5 => Some(("Remove saved space…", "delete the space file from disk")),
        _ => None,
    }
}

pub fn session_label(index: usize) -> Option<(&'static str, &'static str)> {
    match index {
        0 => Some(("Focus", "bring this session to the keyboard")),
        1 => Some(("Rename", "change the tab or session title")),
        2 => Some(("Stop…", "remove from the space and stop its processes")),
        3 => Some((
            "Move to space…",
            "move the session into another saved space",
        )),
        _ => None,
    }
}

pub fn pane_label(index: usize) -> Option<(&'static str, &'static str)> {
    match index {
        0 => Some(("Focus", "bring this pane to the keyboard")),
        1 => Some(("Close pane…", "close this pane in the layout")),
        _ => None,
    }
}

pub fn session_solo_label(index: usize) -> Option<(&'static str, &'static str)> {
    match index {
        0 => Some(("Focus", "bring this session to the keyboard")),
        1 => Some(("Rename", "change the tab or session title")),
        2 => Some(("Close pane…", "close this pane in the layout")),
        3 => Some(("Stop…", "remove from the space and stop its processes")),
        4 => Some((
            "Move to space…",
            "move the session into another saved space",
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_menu_lists_every_action_in_order() {
        let expected = [
            RailSpaceAction::OpenFocus,
            RailSpaceAction::Rename,
            RailSpaceAction::SaveNow,
            RailSpaceAction::AddSession,
            RailSpaceAction::CloseSpace,
            RailSpaceAction::RemoveSaved,
        ];
        for (index, action) in expected.into_iter().enumerate() {
            assert_eq!(space_action(index), Some(action), "row {index}");
            assert!(space_label(index).is_some(), "label {index}");
        }
        assert_eq!(space_action(6), None);
        assert_eq!(row_count(ContextMenuKind::RailSpace), Some(RAIL_SPACE_ROWS));
    }

    #[test]
    fn session_menu_lists_every_action_in_order() {
        let expected = [
            RailSessionAction::Focus,
            RailSessionAction::Rename,
            RailSessionAction::Stop,
            RailSessionAction::MoveToSpace,
        ];
        for (index, action) in expected.into_iter().enumerate() {
            assert_eq!(session_action(index), Some(action), "row {index}");
            assert!(session_label(index).is_some(), "label {index}");
        }
        assert_eq!(session_action(4), None);
        assert_eq!(
            row_count(ContextMenuKind::RailSession),
            Some(RAIL_SESSION_ROWS)
        );
    }

    #[test]
    fn pane_menu_lists_focus_and_close_in_order() {
        let expected = [RailPaneAction::Focus, RailPaneAction::ClosePane];
        for (index, action) in expected.into_iter().enumerate() {
            assert_eq!(pane_action(index), Some(action), "row {index}");
            assert!(pane_label(index).is_some(), "label {index}");
        }
        assert_eq!(pane_action(2), None);
        assert_eq!(row_count(ContextMenuKind::RailPane), Some(RAIL_PANE_ROWS));
        assert_eq!(pane_label(1).map(|(label, _)| label), Some("Close pane…"));
    }

    #[test]
    fn close_space_row_only_when_space_is_current() {
        let other: Vec<_> = space_menu_row_indices(false).collect();
        assert_eq!(other.len(), RAIL_SPACE_ROWS - 1);
        assert!(!other.contains(&4));
        assert_eq!(
            space_menu_row_indices(true).collect::<Vec<_>>().len(),
            RAIL_SPACE_ROWS
        );
        assert_eq!(space_menu_action_index(false, 4), Some(5));
    }

    #[test]
    fn destructive_rows_require_confirmation_once() {
        for index in [4, 5] {
            assert!(
                needs_confirmation(ContextMenuKind::RailSpace, index, false),
                "space row {index}"
            );
            assert!(
                !needs_confirmation(ContextMenuKind::RailSpace, index, true),
                "space row {index} confirmed"
            );
        }
        assert!(!needs_confirmation(ContextMenuKind::RailSpace, 2, false));
        assert!(needs_confirmation(ContextMenuKind::RailSession, 2, false));
        assert!(!needs_confirmation(ContextMenuKind::RailSession, 2, true));
        assert!(!needs_confirmation(ContextMenuKind::RailSession, 1, false));
        assert!(needs_confirmation(ContextMenuKind::RailPane, 1, false));
        assert!(!needs_confirmation(ContextMenuKind::RailPane, 1, true));
        assert!(!needs_confirmation(ContextMenuKind::RailPane, 0, false));
        assert!(needs_confirmation(
            ContextMenuKind::RailSessionSolo,
            2,
            false
        ));
        assert!(needs_confirmation(
            ContextMenuKind::RailSessionSolo,
            3,
            false
        ));
        assert!(!needs_confirmation(
            ContextMenuKind::RailSessionSolo,
            2,
            true
        ));
    }

    #[test]
    fn solo_session_menu_merges_session_and_pane_actions() {
        let expected = [
            RailSessionSoloAction::Focus,
            RailSessionSoloAction::Rename,
            RailSessionSoloAction::ClosePane,
            RailSessionSoloAction::Stop,
            RailSessionSoloAction::MoveToSpace,
        ];
        for (index, action) in expected.into_iter().enumerate() {
            assert_eq!(session_solo_action(index), Some(action), "row {index}");
            assert!(session_solo_label(index).is_some(), "label {index}");
        }
        assert_eq!(
            row_count(ContextMenuKind::RailSessionSolo),
            Some(RAIL_SESSION_SOLO_ROWS)
        );
    }
}
