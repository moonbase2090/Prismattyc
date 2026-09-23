// SPDX-License-Identifier: MPL-2.0
use super::Emulator;

#[test]
fn split_scalar_preserves_following_ascii() {
    for text in [
        "é é",
        "éaé",
        "café Ελληνικά",
        "€ x€",
        "😀 x😀",
        "¢a¢",
        "߿a߿",
        "ࠀaࠀ",
        "\u{ffff}a\u{ffff}",
        "𐀀a𐀀",
        "\u{10ffff}a\u{10ffff}",
    ] {
        for split in 0..=text.len() {
            let mut emulator = Emulator::new(80, 24, 0);
            emulator.feed(&text.as_bytes()[..split]);
            emulator.feed(&[]);
            emulator.feed(&text.as_bytes()[split..]);
            let mut whole = Emulator::new(80, 24, 0);
            whole.feed(text.as_bytes());
            assert_eq!(
                emulator.screen().export_state(),
                whole.screen().export_state(),
                "text {text:?}, split {split}"
            );
        }
    }
    let mut emulator = Emulator::new(80, 24, 0);
    emulator.feed(b"\xc3");
    emulator.feed(b"\xa9 \xc3\xa9");
    assert_eq!(emulator.screen().history_line_text(0), "é é");
}

#[test]
fn chunk_partitions_preserve_unicode_screen_and_modes() {
    let input = concat!(
        "é é 日本語 😀 e\u{301}\r\n",
        "\x1b[31mred é\x1b[0m\twide界\r\n",
        "👩\u{200d}💻 🇺🇸 👍🏽\r\n",
        "\x1b]8;;https://example.test/é\x1b\\linked\x1b]8;;\x1b\\\r\n",
        "\x1b[?1049halternate界\x1b[?1049l",
        "\x1b[?2004h\x1b[?1006h\x1b[?25l",
        "tailé\x07\r\nend",
    );
    for history in [0, 8] {
        let mut whole = Emulator::new(12, 3, history);
        whole.feed(input.as_bytes());
        let expected = whole.export_state().unwrap();
        let expected_bell = whole.take_pending_bell();
        for chunk in 1..=9 {
            let mut split = Emulator::new(12, 3, history);
            for bytes in input.as_bytes().chunks(chunk) {
                split.feed(bytes);
                split.feed(&[]);
            }
            assert_eq!(
                split.export_state().unwrap(),
                expected,
                "history {history}, chunk {chunk}"
            );
            assert_eq!(split.take_pending_bell(), expected_bell);
        }
    }
}

#[test]
fn malformed_and_incomplete_sequences_remain_streaming() {
    for input in [
        &b"\xc3x\xc3\xa9"[..],
        &b"\xe2\x82\x1b[31mred\x1b[0m"[..],
        &b"\xf0\x9f\x98\r\nnext"[..],
        &b"\xe0\x80x\xc3\xa9"[..],
        &b"\xed\xa0\x80x"[..],
        &b"\xf4\x90\x80\x80x"[..],
        &b"\xc3\xc3\xa9"[..],
    ] {
        let mut whole = Emulator::new(80, 24, 8);
        whole.feed(input);
        let expected = whole.screen().export_state();
        for split in 0..=input.len() {
            let mut emulator = Emulator::new(80, 24, 8);
            emulator.feed(&input[..split]);
            emulator.feed(&[]);
            emulator.feed(&input[split..]);
            assert_eq!(
                emulator.screen().export_state(),
                expected,
                "input {input:?}, split {split}"
            );
        }
    }
    let mut emulator = Emulator::new(80, 24, 0);
    emulator.feed(b"\xf0\x9f");
    assert!(emulator.export_state().is_err());
    emulator.feed(&[]);
    assert_eq!(emulator.screen().history_line_text(0), "");
    emulator.feed(b"\x98\x80");
    assert_eq!(emulator.screen().history_line_text(0), "😀");
    assert!(emulator.export_state().is_ok());
}
