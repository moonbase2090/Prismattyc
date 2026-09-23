// SPDX-License-Identifier: MPL-2.0
use super::*;

fn assert_scalar_matches_general_path(screen: &mut Screen, character: char) {
    let expected_join = screen.previous_base_column().is_some_and(|column| {
        screen.active().cells.row(screen.cursor().row)[column].ends_with_zwj()
    });
    assert_eq!(screen.previous_base_ends_with_zwj(), expected_join);
    let mut reference = screen.clone();
    // An unused interned tail forces the general lookup without modifying any
    // cell. Both screens still have the same externally observable state.
    reference.clusters.intern(&['\u{301}']);
    screen.put_char(character);
    reference.put_char(character);
    assert_eq!(screen.export_state(), reference.export_state());
    assert_eq!(screen.content_epoch(), reference.content_epoch());
    assert_eq!(screen.take_damage(), reference.take_damage());
}

#[test]
fn plain_output_matches_general_path_through_screen_transitions() {
    for columns in [1, 2, 7, 80] {
        for history in [0, 8] {
            let mut screen = Screen::new(columns, 3, history);
            for round in 0..8 {
                screen.set_style(Style {
                    bold: round % 2 == 0,
                    ..Style::default()
                });
                assert!(screen.set_hyperlink(Some("fixture"), "https://example.test/plain"));
                for character in "plain output wraps 0123456789".chars() {
                    assert_scalar_matches_general_path(&mut screen, character);
                }
                screen.carriage_return();
                screen.line_feed();
                screen.set_cursor_position(0, 0);
                for character in "界xé\u{301}👩\u{200d}💻🇺🇸👍🏽".chars() {
                    assert_scalar_matches_general_path(&mut screen, character);
                }
                screen.set_cursor_position(0, 1);
                assert_scalar_matches_general_path(&mut screen, 'x');
                screen.resize(columns + 1, 4);
                screen.resize(columns, 3);
                screen.collect_clusters();
                screen = Screen::import_state(screen.export_state()).unwrap();
                assert_scalar_matches_general_path(&mut screen, 'z');
                screen.ris_reset();
            }
        }
    }
}

#[test]
fn imported_and_collected_zwj_tail_still_joins() {
    let mut screen = Screen::new(8, 3, 8);
    for character in "👩\u{200d}".chars() {
        screen.put_char(character);
    }
    screen = Screen::import_state(screen.export_state()).unwrap();
    screen.collect_clusters();
    assert!(screen.previous_base_ends_with_zwj());
    let cursor = screen.cursor();
    assert_scalar_matches_general_path(&mut screen, '💻');
    assert_eq!(screen.cursor(), cursor);
    assert_scalar_matches_general_path(&mut screen, 'x');
    screen.ris_reset();
    screen.collect_clusters();
    assert_eq!(screen.clusters.len(), 0);
    assert_scalar_matches_general_path(&mut screen, 'a');
}

#[test]
fn copied_and_alternate_tails_keep_general_join_semantics() {
    let mut source = Screen::new(8, 3, 8);
    for character in "👩\u{200d}".chars() {
        source.put_char(character);
    }
    let mut screen = Screen::new(8, 3, 8);
    let damage = source.take_damage();
    screen.apply_damage(&source, &damage);
    assert!(screen.previous_base_ends_with_zwj());
    assert_scalar_matches_general_path(&mut screen, '💻');
    for mode in [
        AltScreenMode::Mode47,
        AltScreenMode::Mode1047,
        AltScreenMode::Mode1049,
    ] {
        screen.enter_alt_screen(mode);
        for character in "ASCII界\u{200d}a".chars() {
            assert_scalar_matches_general_path(&mut screen, character);
        }
        screen.leave_alt_screen(mode);
        screen.collect_clusters();
        screen.set_autowrap(false);
        for character in "overwrite last cell".chars() {
            assert_scalar_matches_general_path(&mut screen, character);
        }
        screen.set_autowrap(true);
    }
}
