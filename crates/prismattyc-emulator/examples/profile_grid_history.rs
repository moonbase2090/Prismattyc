// SPDX-License-Identifier: MPL-2.0
//! Fixed-input grid and history probe. Args: workload units.
//!
//! For feed workloads, one unit replays one fixed ~1 MiB input block.
//! For `reflow`, one unit resizes a warmed 10,000-row primary history.
//! Run separate processes for repeated measurements and CPU samples.
use prismattyc_emulator::Emulator;
use std::{hint::black_box, time::Instant};

const COLUMNS: usize = 80;
const ROWS: usize = 24;
const HISTORY_ROWS: usize = 10_000;
const BLOCK_BYTES: usize = 1024 * 1024;

fn workload_line(name: &str) -> &'static [u8] {
    match name {
        "ascii" => b"Prismattyc grid profile: a plain ASCII line with stable width 0123456789\r\n",
        "scroll" => b"x\r\n",
        "history" => b"scrollback row: retain and evict a fixed 10000-line history\r\n",
        "unicode" => "表情🙂 café e\u{301} Ελληνικά 日本語 👩\u{200d}💻\r\n".as_bytes(),
        "sgr" => b"\x1b[38;2;12;143;229mblue\x1b[0m \x1b[1;4mbold underline\x1b[0m plain\r\n",
        _ => panic!("workload must be ascii, scroll, history, unicode, sgr, or reflow"),
    }
}

fn input_block(line: &[u8]) -> Vec<u8> {
    let mut block = Vec::with_capacity(BLOCK_BYTES);
    while block.len() + line.len() <= BLOCK_BYTES {
        block.extend_from_slice(line);
    }
    block
}

fn warm_history(emulator: &mut Emulator) {
    let warm_line = b"warm profile history row 0123456789\r\n";
    let warm = warm_line.repeat(HISTORY_ROWS + ROWS);
    for chunk in warm.chunks(8192) {
        black_box(emulator.feed(black_box(chunk)));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "usage: profile_grid_history <workload> <units>"
    );
    let workload = args[1].as_str();
    let units: usize = args[2].parse().expect("units must be an integer");
    assert!(units > 0, "units must be positive");

    let history_limit = if matches!(workload, "history" | "reflow") {
        HISTORY_ROWS
    } else {
        0
    };
    let mut emulator = Emulator::new(COLUMNS, ROWS, history_limit);
    if matches!(workload, "history" | "reflow") {
        warm_history(&mut emulator);
    }

    let block = if workload == "reflow" {
        Vec::new()
    } else {
        input_block(workload_line(workload))
    };
    let started = Instant::now();
    if workload == "reflow" {
        for index in 0..units {
            if index % 2 == 0 {
                emulator.resize(96, 30);
            } else {
                emulator.resize(COLUMNS, ROWS);
            }
        }
    } else {
        for _ in 0..units {
            for chunk in block.chunks(8192) {
                black_box(emulator.feed(black_box(chunk)));
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let screen = emulator.screen();
    let input_bytes = block.len().saturating_mul(units);
    let history_lines = screen.history_line_count();
    let visible_tail: Vec<String> = (history_lines.saturating_sub(ROWS)..history_lines)
        .map(|row| screen.history_line_text(row))
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "workload": workload,
            "units": units,
            "input_bytes": input_bytes,
            "seconds": elapsed,
            "mib_s": (input_bytes > 0).then(|| input_bytes as f64 / elapsed / 1_048_576.0),
            "history_limit": history_limit,
            "history_lines": history_lines,
            "scrollback_bytes": screen.scrollback_bytes(),
            "cursor": [screen.cursor().row, screen.cursor().column],
            "visible_tail": visible_tail,
            "cell_bytes": std::mem::size_of::<prismattyc_core::Cell>(),
        })
    );
    black_box(emulator);
}
