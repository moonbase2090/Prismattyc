// Compile against each checkout with rustc --extern prismattyc_core=... .
// Compare runs with the same inputs and compiler settings.
use prismattyc_core::{Cell, GridDamage, Screen, ScrollDamage};
use std::{hint::black_box, time::Instant};
fn require_copy<T: Copy>() {}
fn main() {
    require_copy::<Cell>();
    let args: Vec<_> = std::env::args().collect();
    let mode = &args[1];
    let rounds: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(200_000);
    let columns = 92;
    let rows = 42;
    let mut screen = Screen::new(columns, rows, 10_000);
    let unicode = mode.contains("unicode");
    for row in 0..rows {
        screen.set_cursor_position(row, 0);
        for _ in 0..columns - 1 {
            screen.put_char('e');
            if unicode {
                screen.put_char('\u{301}');
            }
        }
    }
    screen.carriage_return();
    let mut replica = screen.clone();
    let mut damage = GridDamage::empty(rows, columns);
    damage.push_scroll(ScrollDamage {
        top: 0,
        bottom: rows - 1,
        delta: 1,
    });
    damage.mark_row_cells(rows - 1);
    #[cfg(pt263_audit)]
    prismattyc_core::pt263_audit_reset();
    let started = Instant::now();
    for _ in 0..rounds {
        if mode.starts_with("replica") {
            replica.apply_damage(&screen, &damage);
        } else {
            if mode.starts_with("flood") {
                for ch in "1234567890".chars() {
                    screen.put_char(ch);
                }
                if unicode {
                    screen.put_char('\u{301}');
                }
            }
            screen.carriage_return();
            screen.line_feed();
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!("mode={mode} cell_bytes={} rounds={rounds} seconds={elapsed:.6} shifted_cells_per_second={:.0}",std::mem::size_of::<Cell>(),rounds as f64 * columns as f64 * (rows-1) as f64 / elapsed);
    #[cfg(pt263_audit)]
    println!("audit={:?}", prismattyc_core::pt263_audit_take());
    // Export outside the timed region. The driver compares all DTO fields,
    // except the new budget field, across the two library versions.
    if let Some(path) = args.get(3) {
        let state = if mode.starts_with("replica") { &replica } else { &screen };
        std::fs::write(path, format!("{:?}", state.export_state())).unwrap();
        println!("history_rows={} scrolled_lines={}", state.history_len(), state.scrolled_lines());
    }
    black_box(screen);
    black_box(replica);
}
