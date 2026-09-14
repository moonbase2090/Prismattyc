//! PT-114 state snapshots are pure, versioned DTOs with validated imports.

use prismattyc_emulator::{Emulator, EmulatorStateV1, StateError, EMULATOR_STATE_FORMAT_VERSION};

const COLS: usize = 80;
const ROWS: usize = 24;
const SCROLLBACK: usize = 128;
const RED_1X1_PNG_B64: &[u8] =
    b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
const FIXTURE: &[u8] = include_bytes!("fixtures/pt72-session.bin");

#[test]
fn legacy_snapshot_defaults_the_history_byte_budget() {
    let mut original = Emulator::new(8, 1, 10);
    original.feed("e\u{0301}\r\n🇺🇸\r\n".as_bytes());
    let state = original.export_state().unwrap();
    let mut legacy = serde_json::to_value(&state).unwrap();
    assert!(legacy["screen"]
        .as_object_mut()
        .unwrap()
        .remove("max_scrollback_bytes")
        .is_some());
    let restored = Emulator::import_state(serde_json::from_value(legacy).unwrap()).unwrap();
    assert_eq!(restored.screen(), original.screen());
    assert_eq!(restored.screen().scrollback_byte_budget(), 96 * 1024 * 1024);
}

fn direct_image_apc() -> Vec<u8> {
    let mut bytes = b"\x1b_Ga=T,t=d,f=100,i=7;".to_vec();
    bytes.extend_from_slice(RED_1X1_PNG_B64);
    bytes.extend_from_slice(b"\x1b\\");
    bytes
}

#[test]
fn fixture_round_trips_and_continues_deterministically() {
    let mut original = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    original.set_retain_alt_history(true);
    original.feed(FIXTURE);
    original.feed(b"\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[=1;2u");
    original.feed(b"\x1b]8;;https://example.test\x1b\\linked\x1b]8;;\x1b\\");
    let mut graphics = b"\x1b_Ga=T,t=d,f=100,i=7;".to_vec();
    graphics.extend_from_slice(RED_1X1_PNG_B64);
    graphics.extend_from_slice(b"\x1b\\");
    original.feed(&graphics);
    original.set_cell_pixels(11, 22);

    let exported = original
        .export_state()
        .expect("fixture ends at parser boundary");
    assert_eq!(exported.format_version, EMULATOR_STATE_FORMAT_VERSION);
    let mut restored = Emulator::import_state(exported.clone()).expect("valid snapshot");
    assert_eq!(
        restored.export_state().expect("restored boundary"),
        exported
    );
    assert_eq!(restored.screen(), original.screen());
    assert!(restored.collects_apc());
    assert_eq!(restored.mouse_tracking(), original.mouse_tracking());
    assert_eq!(restored.keyboard_flags(), original.keyboard_flags());

    let tail = b"\x1b[38;5;196mPT114\x1b[0m\r\n";
    original.feed(tail);
    restored.feed(tail);
    assert_eq!(restored.screen(), original.screen());
    assert_eq!(
        restored.export_state().expect("continued boundary"),
        original.export_state().expect("continued boundary")
    );
}

#[test]
fn direct_image_anchor_is_invariant_to_feed_chunking() {
    let mut one_chunk = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let mut split = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let image = direct_image_apc();
    let mut bytes = b"\x1b[3;5H".to_vec();
    bytes.extend_from_slice(&image);
    let split_at = bytes.len();
    bytes.extend_from_slice("\r\n".repeat(30).as_bytes());

    one_chunk.feed(&bytes);
    split.feed(&bytes[..split_at]);
    split.feed(&bytes[split_at..]);

    let one_state = one_chunk.export_state().expect("one-chunk boundary");
    let split_state = split.export_state().expect("split boundary");
    assert_eq!(one_state.graphics, split_state.graphics);
    let placed = one_state.graphics.images.first().expect("placed image");
    assert_eq!(placed.anchor_abs_line, 2);
    assert_eq!(placed.anchor_col, 4);
}

#[test]
fn direct_image_anchor_round_trips_through_snapshot() {
    let mut original = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    original.feed(b"\x1b[4;6H");
    original.feed(&direct_image_apc());

    let exported = original.export_state().expect("image boundary");
    let placed = exported.graphics.images.first().expect("placed image");
    assert_eq!(placed.anchor_abs_line, 3);
    assert_eq!(placed.anchor_col, 5);

    let restored = Emulator::import_state(exported.clone()).expect("valid snapshot");
    assert_eq!(
        restored.export_state().expect("restored boundary"),
        exported
    );
}

#[test]
fn graphics_clear_before_and_after_image_respects_stream_order() {
    let mut clear_before = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let mut before = b"\x1b[2J\x1b[3;5H".to_vec();
    before.extend_from_slice(&direct_image_apc());
    clear_before.feed(&before);
    let before_state = clear_before.export_state().expect("clear-before boundary");
    let before_image = before_state
        .graphics
        .images
        .first()
        .expect("image after clear");
    assert_eq!(before_state.graphics.images.len(), 1);
    assert_eq!(before_image.anchor_abs_line, 2);
    assert_eq!(before_image.anchor_col, 4);

    let mut clear_after = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let mut after = direct_image_apc();
    after.extend_from_slice(b"\x1b[2J");
    clear_after.feed(&after);
    assert!(clear_after
        .export_state()
        .expect("clear-after boundary")
        .graphics
        .images
        .is_empty());
}

#[test]
fn graphics_alt_screen_switch_before_and_after_image_respects_stream_order() {
    let mut alt_before = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let mut before = b"\x1b[?1049h".to_vec();
    before.extend_from_slice(&direct_image_apc());
    alt_before.feed(&before);
    assert_eq!(
        alt_before
            .export_state()
            .expect("alt-before boundary")
            .graphics
            .images
            .len(),
        1
    );

    let mut alt_after = Emulator::new_experimental(COLS, ROWS, SCROLLBACK);
    let mut after = direct_image_apc();
    after.extend_from_slice(b"\x1b[?1049h");
    alt_after.feed(&after);
    assert!(alt_after
        .export_state()
        .expect("alt-after boundary")
        .graphics
        .images
        .is_empty());
}

#[test]
fn ten_thousand_scrollback_snapshot_stays_compact() {
    let mut emulator = Emulator::new(COLS, ROWS, 10_000);
    for _ in 0..32 {
        emulator.feed(FIXTURE);
    }
    let state = emulator.export_state().expect("fixture ends at boundary");
    let encoded = serde_json::to_vec(&state).expect("state serializes");
    println!("compact PT-114 snapshot: {} bytes", encoded.len());
    assert!(
        encoded.len() < 4 * 1024 * 1024,
        "compact snapshot is {} bytes",
        encoded.len()
    );
}

#[test]
fn export_rejects_incomplete_parser_sequences() {
    for bytes in [
        b"\x1b[".as_slice(),
        b"\x1b]0;title".as_slice(),
        "é".as_bytes().get(..1).unwrap(),
    ] {
        let mut emulator = Emulator::new(COLS, ROWS, SCROLLBACK);
        emulator.feed(bytes);
        assert!(matches!(
            emulator.export_state(),
            Err(StateError::ParserNotAtBoundary)
        ));
    }

    let mut emulator = Emulator::new(COLS, ROWS, SCROLLBACK);
    emulator.feed(b"\x1b_Ga=T,m=1;AAAA");
    assert!(matches!(
        emulator.export_state(),
        Err(StateError::ParserNotAtBoundary)
    ));
}

#[test]
fn import_rejects_zero_sized_screen() {
    let emulator = Emulator::new(COLS, ROWS, SCROLLBACK);
    let mut state = emulator.export_state().expect("ground state");
    state.screen.columns = 0;
    assert!(matches!(
        Emulator::import_state(state),
        Err(StateError::Invalid(message)) if message.contains("dimensions")
    ));
}

#[test]
fn import_rejects_scrollback_beyond_configured_bound() {
    let emulator = Emulator::new(COLS, ROWS, SCROLLBACK);
    let mut state = emulator.export_state().expect("ground state");
    state.screen.max_scrollback = 0;
    state
        .screen
        .scrollback
        .push(state.screen.primary.rows[0].clone());
    assert!(matches!(
        Emulator::import_state(state),
        Err(StateError::Invalid(message)) if message.contains("scrollback length")
    ));
}

#[test]
fn edge_graphemes_round_trip() {
    let cases: &[&[u8]] = &[
        "\u{1F1FA}\u{1F1F8}\u{1F1EF}\r\n".as_bytes(),
        "\u{1F1FA}\u{1F1F8}\u{1F1EF}\u{1F1F5}\r\n".as_bytes(),
        "\u{4E2D}\u{1F3FD}x\r\n".as_bytes(),
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}!\r\n".as_bytes(),
        " \u{0301}a\u{0301}\u{0302}\r\n".as_bytes(),
        "\u{0301}\u{0301}z\r\n".as_bytes(),
        "\u{1F600}\u{FE0F}\u{200D}\r\n".as_bytes(),
        "e\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}q\r\n".as_bytes(),
        "\x1b[31mab\x1b[0m\u{4E2D}\x1b[4mc\x1b]8;;http://x\x07d\x1b]8;;\x07\r\n".as_bytes(),
    ];
    let mut failures = Vec::new();
    for (i, bytes) in cases.iter().enumerate() {
        let mut live = Emulator::new(20, 4, 100);
        live.feed(bytes);
        let state = match live.export_state() {
            Ok(state) => state,
            Err(error) => {
                failures.push(format!("case {i}: export {error:?}"));
                continue;
            }
        };
        let json = serde_json::to_vec(&state).unwrap();
        let back: EmulatorStateV1 = serde_json::from_slice(&json).unwrap();
        match Emulator::import_state(back) {
            Ok(restored) => {
                if restored.screen() != live.screen() {
                    failures.push(format!("case {i}: screen mismatch"));
                } else if restored.export_state().unwrap() != state {
                    failures.push(format!("case {i}: export mismatch"));
                }
            }
            Err(error) => failures.push(format!("case {i}: import {error:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "edge round-trip failures: {failures:#?}"
    );
}

#[test]
fn last_column_wide_clusters_round_trip() {
    let pad = "x".repeat(19);
    let cases: Vec<Vec<u8>> = vec![
        format!("{pad}\u{1F600}\u{FE0F}\r\n").into_bytes(),
        format!("{pad}\u{1F1FA}\u{1F1F8}\r\n").into_bytes(),
        format!("{pad}\u{263A}\u{FE0F}y\r\n").into_bytes(),
        format!("\x1b[?7l{pad}\u{1F600}\u{FE0F}zz\x1b[?7h\r\n").into_bytes(),
        format!("{}\u{1F600}\u{FE0F}\r\n", "x".repeat(18)).into_bytes(),
    ];
    let mut failures = Vec::new();
    for (i, bytes) in cases.iter().enumerate() {
        let mut live = Emulator::new(20, 4, 100);
        live.feed(bytes);
        let state = match live.export_state() {
            Ok(state) => state,
            Err(error) => {
                failures.push(format!("case {i}: export {error:?}"));
                continue;
            }
        };
        let back: EmulatorStateV1 =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        match Emulator::import_state(back) {
            Ok(restored) => {
                if restored.screen() != live.screen() {
                    failures.push(format!("case {i}: screen mismatch"));
                } else if restored.export_state().unwrap() != state {
                    failures.push(format!("case {i}: export mismatch"));
                }
            }
            Err(error) => failures.push(format!("case {i}: import {error:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "last-column round-trip failures: {failures:#?}"
    );
}

#[test]
fn import_rejects_unknown_format_version() {
    let mut emulator = Emulator::new(COLS, ROWS, SCROLLBACK);
    emulator.feed(b"PT-114");
    let mut state = emulator.export_state().expect("ground state");
    state.format_version = EMULATOR_STATE_FORMAT_VERSION + 1;
    assert!(matches!(
        Emulator::import_state(state),
        Err(StateError::UnsupportedVersion { .. })
    ));
}
