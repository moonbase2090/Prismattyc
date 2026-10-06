//! Sidebar and vertical-rail context menus (issue #181).
//!
//! Pure row model: labels, action mapping, and confirmation gating. The host
//! maps [`RailSpaceAction`] / [`RailSessionAction`] onto existing command paths.

use crate::palette::ContextMenuKind;

pub const RAIL_SPACE_ROWS: usize = 6;
pub const RAIL_SESSION_ROWS: usize = 4;

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

pub fn row_count(kind: ContextMenuKind) -> Option<usize> {
    match kind {
        ContextMenuKind::RailSpace => Some(RAIL_SPACE_ROWS),
        ContextMenuKind::RailSession => Some(RAIL_SESSION_ROWS),
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

pub fn session_action(index: usize) -> Option<RailSessionAction> {
    match index {
        0 => Some(RailSessionAction::Focus),
        1 => Some(RailSessionAction::Rename),
        2 => Some(RailSessionAction::Stop),
        3 => Some(RailSessionAction::MoveToSpace),
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
    }
}
