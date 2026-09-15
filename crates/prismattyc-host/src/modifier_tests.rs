use super::*;

#[test]
fn logical_modifier_transitions_preserve_other_pressed_modifiers() {
    for (key, flag) in [
        (NamedKey::Control, ModifiersState::CONTROL),
        (NamedKey::Shift, ModifiersState::SHIFT),
        (NamedKey::Alt, ModifiersState::ALT),
        (NamedKey::AltGraph, ModifiersState::ALT),
        (NamedKey::Super, ModifiersState::SUPER),
        (NamedKey::Meta, ModifiersState::SUPER),
        (NamedKey::Hyper, ModifiersState::SUPER),
    ] {
        let others = ModifiersState::all() & !flag;
        let mut state = others;
        apply_modifier_key_event(&mut state, &Key::Named(key), ElementState::Pressed);
        assert_eq!(state, others | flag, "press {key:?}");
        apply_modifier_key_event(&mut state, &Key::Named(key), ElementState::Released);
        assert_eq!(state, others, "release {key:?}");
        apply_modifier_key_event(&mut state, &Key::Named(key), ElementState::Released);
        assert_eq!(state, others, "duplicate release {key:?}");
    }
    for key in [Key::Named(NamedKey::Enter), Key::Character("x".into())] {
        let mut state = ModifiersState::CONTROL | ModifiersState::SHIFT;
        apply_modifier_key_event(&mut state, &key, ElementState::Released);
        assert_eq!(state, ModifiersState::CONTROL | ModifiersState::SHIFT);
    }
}

#[test]
fn physical_modifiers_support_both_sides_and_ignore_unrelated_keys() {
    for (left, right, flag) in [
        (
            KeyCode::ControlLeft,
            KeyCode::ControlRight,
            ModifiersState::CONTROL,
        ),
        (
            KeyCode::ShiftLeft,
            KeyCode::ShiftRight,
            ModifiersState::SHIFT,
        ),
        (KeyCode::AltLeft, KeyCode::AltRight, ModifiersState::ALT),
        (
            KeyCode::SuperLeft,
            KeyCode::SuperRight,
            ModifiersState::SUPER,
        ),
    ] {
        for code in [left, right] {
            let others = ModifiersState::all() & !flag;
            let mut state = others;
            apply_modifier_physical(&mut state, PhysicalKey::Code(code), ElementState::Pressed);
            assert_eq!(state, others | flag, "press {code:?}");
            apply_modifier_physical(&mut state, PhysicalKey::Code(code), ElementState::Released);
            assert_eq!(state, others, "release {code:?}");
        }
    }
    for physical in [
        PhysicalKey::Code(KeyCode::KeyA),
        PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
    ] {
        let mut state = ModifiersState::ALT;
        apply_modifier_physical(&mut state, physical, ElementState::Pressed);
        assert_eq!(state, ModifiersState::ALT);
    }
}

#[test]
fn mark_chord_accepts_control_space_variants_without_stealing_alt_or_shift() {
    for key in [
        Key::Named(NamedKey::Space),
        Key::Character(" ".into()),
        Key::Character("2".into()),
    ] {
        assert!(is_mark_key(&key, ModifiersState::CONTROL));
        for modifiers in [
            ModifiersState::empty(),
            ModifiersState::ALT,
            ModifiersState::CONTROL | ModifiersState::ALT,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        ] {
            assert!(!is_mark_key(&key, modifiers), "{key:?} with {modifiers:?}");
        }
    }
    assert!(!is_mark_key(
        &Key::Character("x".into()),
        ModifiersState::CONTROL
    ));
    assert!(!is_mark_key(
        &Key::Named(NamedKey::Enter),
        ModifiersState::CONTROL
    ));
}

#[test]
fn rich_focus_tokens_preserve_modifiers_and_reject_compound_text() {
    let modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT;
    for (key, token) in [
        (NamedKey::Enter, "Enter"),
        (NamedKey::Tab, "Tab"),
        (NamedKey::Backspace, "Backspace"),
        (NamedKey::Delete, "Delete"),
        (NamedKey::ArrowUp, "Up"),
        (NamedKey::ArrowDown, "Down"),
        (NamedKey::ArrowLeft, "Left"),
        (NamedKey::ArrowRight, "Right"),
        (NamedKey::Home, "Home"),
        (NamedKey::End, "End"),
        (NamedKey::PageUp, "PageUp"),
        (NamedKey::PageDown, "PageDown"),
        (NamedKey::Space, "Space"),
    ] {
        assert_eq!(
            rich_focus_key_token(&Key::Named(key), modifiers),
            Some(format!("C-S-A-{token}"))
        );
    }
    assert_eq!(
        rich_focus_key_token(&Key::Character("é".into()), modifiers).as_deref(),
        Some("C-S-A-é")
    );
    for text in ["", "two", "e\u{301}"] {
        assert_eq!(
            rich_focus_key_token(&Key::Character(text.into()), modifiers),
            None
        );
    }
    assert_eq!(
        rich_focus_key_token(&Key::Named(NamedKey::F1), modifiers),
        None
    );
}
