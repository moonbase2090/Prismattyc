// SPDX-License-Identifier: MPL-2.0
//! Fixed-input parser/screen probe. Args: workload history rows chunk repetitions.
//! Prints one JSON record; use separate processes for repeated measurements.
use prismattyc_emulator::Emulator;
use std::{hint::black_box, time::Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args.len(), 6, "workload history rows chunk repetitions");
    let history: usize = args[2].parse().unwrap();
    let rows: usize = args[3].parse().unwrap();
    let chunk: usize = args[4].parse().unwrap();
    let repeats: usize = args[5].parse().unwrap();
    assert!(rows > 0 && chunk > 0 && repeats > 0);
    let line = match args[1].as_str() {
        "ascii" => "abcdefghijklmnopqrstuvxyz0123456789 abcdefghijklmnopqrstuvwxyz\r\n",
        "wrapped" => "abcdefghijklmnopqrstuvxyz0123456789 abcdefghijklmnopqrstuvwxyz",
        "unicode" => "café Ελληνικά 日本語 e\u{301} 😀\r\n",
        "escapes" => "\x1b[31mred\x1b[0m plain \x1b[1mbold\x1b[0m\r\n",
        _ => panic!("unknown workload"),
    };
    let data = line.as_bytes().repeat(repeats);
    let mut emulator = Emulator::new(80, rows, history);
    // Populate history before measuring steady-state ingestion. Allocation
    // and cold-start costs are outside this timing but remain in process RSS.
    let warm = b"warm history row\r\n".repeat(history + rows);
    for bytes in warm.chunks(8192) {
        black_box(emulator.feed(bytes));
    }
    let started = Instant::now();
    for bytes in data.chunks(chunk) {
        black_box(emulator.feed(black_box(bytes)));
    }
    let seconds = started.elapsed().as_secs_f64();
    // A deterministic screen fingerprint lets chunk-size runs be compared.
    // This is a probe receipt, not a comprehensive correctness oracle.
    let screen = emulator.screen();
    let text: Vec<String> = (0..screen.history_line_count())
        .rev()
        .take(rows)
        .map(|row| screen.history_line_text(row))
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "workload": args[1], "history_limit": history, "rows": rows,
            "chunk": chunk, "repetitions": repeats, "bytes": data.len(),
            "seconds": seconds, "mib_s": data.len() as f64 / seconds / 1048576.0,
            "history_lines": screen.history_line_count(),
            "scrollback_bytes": screen.scrollback_bytes(),
            "scrollback_byte_budget": screen.scrollback_byte_budget(),
            "cell_bytes": std::mem::size_of::<prismattyc_core::Cell>(),
            "cursor": [screen.cursor().row, screen.cursor().column], "screen": text,
        })
    );
}
