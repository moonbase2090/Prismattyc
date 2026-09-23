// SPDX-License-Identifier: MPL-2.0
//! Print chunk-sensitive behavior in the parser and emulator independently.
use prismattyc_emulator::Emulator;

#[derive(Default)]
struct Printed(String);
impl vte::Perform for Printed {
    fn print(&mut self, character: char) {
        self.0.push(character);
    }
}

fn main() {
    for text in [
        "é é",
        "éaé",
        "日本語",
        "e\u{301} x",
        "😀 x",
        "café Ελληνικά",
    ] {
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let mut parser = vte::Parser::new();
            let mut printed = Printed::default();
            let mut emulator = Emulator::new(80, 24, 0);
            for chunk in [&bytes[..split], &bytes[split..]] {
                parser.advance(&mut printed, chunk);
                let _ = emulator.feed(chunk);
            }
            println!(
                "{}",
                serde_json::json!({
                    "input": text, "split": split, "parser_printed": printed.0,
                    "screen": emulator.screen().history_line_text(0),
                    "parser_equal": printed.0 == text,
                })
            );
        }
    }
}
