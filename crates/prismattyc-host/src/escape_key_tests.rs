//! Escape must close every dialog, menu and overlay however the platform
//! reports the key: as the named key, as the control character, as only the
//! physical key, and with a modifier the host believes is held.

use super::*;

const SPELLINGS: [&str; 4] = [
    "named",
    "control character as the key",
    "control character as text only",
    "physical key only",
];

fn escape_as(spelling: &str) -> (Key, Key, Option<&'static str>, PhysicalKey) {
    let other = Key::Named(NamedKey::F20);
    match spelling {
        "named" => (
            Key::Named(NamedKey::Escape),
            Key::Named(NamedKey::Escape),
            None,
            PhysicalKey::Code(KeyCode::KeyQ),
        ),
        "control character as the key" => (
            Key::Character("\u{1b}".into()),
            other,
            None,
            PhysicalKey::Code(KeyCode::KeyQ),
        ),
        "control character as text only" => (
            other.clone(),
            other,
            Some("\u{1b}"),
            PhysicalKey::Code(KeyCode::KeyQ),
        ),
        "physical key only" => (
            other.clone(),
            other,
            None,
            PhysicalKey::Code(KeyCode::Escape),
        ),
        _ => unreachable!(),
    }
}

#[test]
fn every_spelling_of_escape_is_recognised() {
    for spelling in SPELLINGS {
        let (logical, unmodified, text, physical) = escape_as(spelling);
        assert!(
            is_escape_press(&logical, &unmodified, text, physical),
            "{spelling}"
        );
    }
}

#[test]
fn other_keys_are_not_mistaken_for_escape() {
    let cases = [
        (
            Key::Named(NamedKey::Enter),
            Key::Named(NamedKey::Enter),
            Some("\r"),
            PhysicalKey::Code(KeyCode::Enter),
        ),
        (
            Key::Character("a".into()),
            Key::Character("a".into()),
            Some("a"),
            PhysicalKey::Code(KeyCode::KeyA),
        ),
        (
            Key::Named(NamedKey::F1),
            Key::Named(NamedKey::F1),
            None,
            PhysicalKey::Code(KeyCode::F1),
        ),
        (
            Key::Named(NamedKey::Backspace),
            Key::Named(NamedKey::Backspace),
            Some("\u{8}"),
            PhysicalKey::Code(KeyCode::Backspace),
        ),
    ];
    for (logical, unmodified, text, physical) in cases {
        assert!(
            !is_escape_press(&logical, &unmodified, text, physical),
            "{logical:?}"
        );
    }
}

#[test]
fn escape_reaches_a_text_field_under_a_held_command_modifier_but_other_keys_do_not() {
    let escape = Key::Named(NamedKey::Escape);
    let letter = Key::Character("a".into());
    for held in [
        ModifiersState::CONTROL,
        ModifiersState::ALT,
        ModifiersState::SUPER,
        ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER,
    ] {
        assert!(!command_modifier_blocks_text(&escape, held), "{held:?}");
        assert!(command_modifier_blocks_text(&letter, held), "{held:?}");
    }
    for plain in [ModifiersState::empty(), ModifiersState::SHIFT] {
        assert!(!command_modifier_blocks_text(&letter, plain));
        assert!(!command_modifier_blocks_text(&escape, plain));
    }
}

#[test]
fn escape_leaves_the_space_rail_under_a_held_modifier() {
    for held in [
        ModifiersState::empty(),
        ModifiersState::CONTROL,
        ModifiersState::ALT,
        ModifiersState::SUPER,
    ] {
        assert_eq!(
            space_rail_key_decision(
                true,
                space_rail::RailSide::Left,
                held,
                &Key::Named(NamedKey::Escape)
            ),
            SpaceRailKeyDecision::Key(space_rail::RailKey::Escape),
            "{held:?}"
        );
    }
    assert_eq!(
        space_rail_key_decision(
            true,
            space_rail::RailSide::Left,
            ModifiersState::CONTROL,
            &Key::Named(NamedKey::Enter)
        ),
        SpaceRailKeyDecision::Leave
    );
}
