//! Isolate terminal parsing and screen mutation from PTY and renderer overhead.
use prismattyc_emulator::Emulator;
use std::{hint::black_box, time::Instant};

fn main() {
    println!(
        "cell_bytes={}",
        std::mem::size_of::<prismattyc_core::Cell>()
    );
    let lines =
        b"abcdefghijklmnopqrstuvxyz0123456789 abcdefghijklmnopqrstuvwxyz\r\n".repeat(50_000);
    let flat = b"abcdefghijklmnopqrstuvxyz0123456789 abcdefghijklmnopqrstuvwxyz".repeat(50_000);
    for (name, data) in [("lines", lines), ("wrapped", flat)] {
        for history in [0, 10_000] {
            for rows in [24, 80] {
                let mut emulator = Emulator::new(80, rows, history);
                let start = Instant::now();
                for chunk in data.chunks(8192) {
                    black_box(emulator.feed(black_box(chunk)));
                }
                let elapsed = start.elapsed().as_secs_f64();
                println!("{name} rows={rows} history={history} bytes={} seconds={elapsed:.6} mib_s={:.2}", data.len(), data.len() as f64 / elapsed / 1048576.0);
                black_box(emulator);
            }
        }
    }
}
