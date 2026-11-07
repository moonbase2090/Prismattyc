// SPDX-License-Identifier: MIT

use lunatui_term::{
    KeyCode, KeyEvent, KeyKind, Modifiers, MouseButton, MouseEvent, MouseKind, Scroll,
};
use pmux_plugin_api::host::pmux::view::frame;
use pmux_plugin_api::modifiers as bits;

fn modifier_bits(modifiers: Modifiers) -> u8 {
    let mut flags = 0;
    for (set, bit) in [
        (modifiers.is_shift(), bits::SHIFT),
        (modifiers.is_alt(), bits::ALT),
        (modifiers.is_control(), bits::CONTROL),
        (modifiers.is_super(), bits::SUPER),
        (modifiers.is_hyper(), bits::HYPER),
        (modifiers.is_meta(), bits::META),
        (modifiers.is_caps_lock(), bits::CAPS_LOCK),
        (modifiers.is_num_lock(), bits::NUM_LOCK),
    ] {
        if set {
            flags |= bit;
        }
    }
    flags
}

pub(super) fn key(key: &KeyEvent) -> Option<frame::Key> {
    if key.kind == KeyKind::Release {
        return None;
    }
    let modifiers = key.modifiers;
    let flags = modifier_bits(modifiers);
    let code = match key.code {
        KeyCode::Char(character) => frame::KeyCode::Character(character),
        KeyCode::F(number) => frame::KeyCode::Function(number),
        code => frame::KeyCode::Named(match code {
            KeyCode::Esc => frame::NamedKey::Escape,
            KeyCode::Enter => frame::NamedKey::Enter,
            KeyCode::Tab if modifiers.is_shift() => frame::NamedKey::BackTab,
            KeyCode::Tab => frame::NamedKey::Tab,
            KeyCode::Backspace => frame::NamedKey::Backspace,
            KeyCode::Delete => frame::NamedKey::Delete,
            KeyCode::Insert => frame::NamedKey::Insert,
            KeyCode::Up => frame::NamedKey::Up,
            KeyCode::Down => frame::NamedKey::Down,
            KeyCode::Left => frame::NamedKey::Left,
            KeyCode::Right => frame::NamedKey::Right,
            KeyCode::Home => frame::NamedKey::Home,
            KeyCode::End => frame::NamedKey::End,
            KeyCode::PageUp => frame::NamedKey::PageUp,
            KeyCode::PageDown => frame::NamedKey::PageDown,
            _ => return None,
        }),
    };
    Some(frame::Key {
        code,
        modifiers: flags,
    })
}

/// Presses, releases and the wheel. Motion is dropped here: every event is a
/// round trip to the plug-in and a new frame, and the views act on clicks.
pub(super) fn mouse(mouse: &MouseEvent) -> Option<frame::Mouse> {
    fn button(button: MouseButton) -> Option<frame::MouseButton> {
        match button {
            MouseButton::Left => Some(frame::MouseButton::Left),
            MouseButton::Middle => Some(frame::MouseButton::Middle),
            MouseButton::Right => Some(frame::MouseButton::Right),
            MouseButton::Extra(_) => None,
        }
    }
    if mouse.unit != lunatui_term::MouseUnit::Cells {
        return None;
    }
    let action = match mouse.kind {
        MouseKind::Down(pressed) => frame::MouseAction::Press(button(pressed)?),
        MouseKind::Up(released) => frame::MouseAction::Release(button(released)?),
        MouseKind::Scroll(Scroll::Up) => frame::MouseAction::Wheel(frame::Wheel::Up),
        MouseKind::Scroll(Scroll::Down) => frame::MouseAction::Wheel(frame::Wheel::Down),
        MouseKind::Scroll(Scroll::Left) => frame::MouseAction::Wheel(frame::Wheel::Left),
        MouseKind::Scroll(Scroll::Right) => frame::MouseAction::Wheel(frame::Wheel::Right),
        MouseKind::Drag(_) | MouseKind::Move => return None,
    };
    Some(frame::Mouse {
        action,
        column: mouse.column,
        row: mouse.row,
        modifiers: modifier_bits(mouse.modifiers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunatui_term::Modifiers;

    #[test]
    fn modifiers_preserve_semantics_and_releases_do_not_become_presses() {
        let native = KeyEvent::press(KeyCode::Tab).modifiers(Modifiers::new().shift().control());
        let mapped = key(&native).unwrap();
        assert!(matches!(
            mapped.code,
            frame::KeyCode::Named(frame::NamedKey::BackTab)
        ));
        assert_eq!(mapped.modifiers, 5);
        assert!(key(&native.kind(KeyKind::Release)).is_none());
    }

    #[test]
    fn clicks_and_the_wheel_reach_the_view_but_motion_does_not() {
        let at = |kind| MouseEvent {
            kind,
            column: 7,
            row: 3,
            modifiers: Modifiers::new().shift(),
            unit: lunatui_term::MouseUnit::Cells,
        };
        let press = mouse(&at(MouseKind::Down(MouseButton::Left))).unwrap();
        assert!(matches!(
            press.action,
            frame::MouseAction::Press(frame::MouseButton::Left)
        ));
        assert_eq!((press.column, press.row, press.modifiers), (7, 3, 1));
        assert!(matches!(
            mouse(&at(MouseKind::Scroll(Scroll::Down))).unwrap().action,
            frame::MouseAction::Wheel(frame::Wheel::Down)
        ));
        assert!(mouse(&at(MouseKind::Move)).is_none());
        assert!(mouse(&at(MouseKind::Drag(MouseButton::Left))).is_none());
        assert!(mouse(&at(MouseKind::Down(MouseButton::Extra(8)))).is_none());
    }
}
