// SPDX-License-Identifier: MIT

use lunatui_term::{KeyCode, KeyEvent, KeyKind};
use pmux_plugin_api::host::pmux::view::frame;
use pmux_plugin_api::modifiers as bits;

pub(super) fn key(key: &KeyEvent) -> Option<frame::Key> {
    if key.kind == KeyKind::Release {
        return None;
    }
    let modifiers = key.modifiers;
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
}
