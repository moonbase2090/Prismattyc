//! Damage completeness (PT-242): incremental repaint from damage matches
//! a full snapshot. PT-243 trusts this so it never leaves stale pixels.

use prismattyc_core::Screen;
use prismattyc_emulator::Emulator;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.next() as usize % items.len()]
    }
}

fn screens_match(a: &Screen, b: &Screen) -> bool {
    if a.rows() != b.rows() || a.columns() != b.columns() {
        return false;
    }
    if a.alt_active() != b.alt_active() {
        return false;
    }
    if a.cursor() != b.cursor() {
        return false;
    }
    for row in 0..a.rows() {
        for col in 0..a.columns() {
            if a.view_cell(0, row, col) != b.view_cell(0, row, col) {
                return false;
            }
        }
    }
    true
}

fn chunk_for(rng: &mut XorShift) -> Vec<u8> {
    let ops: &[&[u8]] = &[
        b"A",
        b"xyz",
        b"0123456789",
        b"\r",
        b"\n",
        b"\x08",
        b"\x1b[A",
        b"\x1b[B",
        b"\x1b[C",
        b"\x1b[D",
        b"\x1b[H",
        b"\x1b[2;2H",
        b"\x1b[J",
        b"\x1b[2J",
        b"\x1b[K",
        b"\x1b[2K",
        b"\x1b[L",
        b"\x1b[M",
        b"\x1b[@",
        b"\x1b[P",
        b"\x1b[S",
        b"\x1b[T",
        b"\x1b[2;20r",
        b"\x1b[r",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?47h",
        b"\x1b[?47l",
        b"\x1bM",
        b"\x1b[3;1H\n",
    ];
    let n = 1 + (rng.next() as usize % 6);
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend_from_slice(rng.pick(ops));
    }
    out
}

fn run_seed(seed: u64) {
    let mut rng = XorShift(seed | 1);
    let mut live = Emulator::new(40, 12, 20);
    let mut replica_src = Emulator::new(40, 12, 20);
    let _ = live.take_damage();
    let _ = replica_src.take_damage();
    let mut replica = live.screen().clone();
    for chunk_i in 0..24 {
        if rng.next().is_multiple_of(17) {
            let cols = 20 + (rng.next() as usize % 21);
            let rows = 8 + (rng.next() as usize % 9);
            live.resize(cols, rows);
            replica_src.resize(cols, rows);
        } else {
            let chunk = chunk_for(&mut rng);
            let _ = live.feed(&chunk);
            let _ = replica_src.feed(&chunk);
        }
        let damage = replica_src.take_damage();
        replica.apply_damage(replica_src.screen(), &damage);
        if !screens_match(live.screen(), &replica) {
            let a = live.screen();
            let mut msg = format!(
                "damage mismatch seed={seed} chunk={chunk_i} alt {}/{} cursor {:?}/{:?} size {}x{} / {}x{} scroll={:?}",
                a.alt_active(),
                replica.alt_active(),
                a.cursor(),
                replica.cursor(),
                a.columns(),
                a.rows(),
                replica.columns(),
                replica.rows(),
                damage.scroll_events()
            );
            for row in 0..a.rows().min(replica.rows()) {
                for col in 0..a.columns().min(replica.columns()) {
                    let ca = a.view_cell(0, row, col);
                    let cb = replica.view_cell(0, row, col);
                    if ca != cb {
                        msg.push_str(&format!(" cell({row},{col}) {ca:?} vs {cb:?}"));
                        panic!("{msg}");
                    }
                }
            }
            panic!("{msg}");
        }
        let _ = live.take_damage();
    }
}

#[test]
fn damage_completeness_200_seeds() {
    for seed in 1..=200u64 {
        run_seed(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
}

#[test]
fn fast_producer_bounds_damage_and_preserves_full_replica() {
    let mut live = Emulator::new(40, 12, 20);
    live.feed(b"initial cells\r\n");
    let mut replica = live.screen().clone();
    let _ = live.take_damage();
    for _ in 0..50_000 {
        live.feed("e\u{301} 中 🇺🇸\r\n".as_bytes());
    }
    live.feed(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    let damage = live.take_damage();
    assert!(damage.scroll_events().len() <= 256);
    assert!(damage.scroll_events().is_empty());
    assert_eq!(damage.dirty_row_count(), 12);
    assert_eq!(damage.dirty_cell_count(), 40 * 12);
    replica.apply_damage(live.screen(), &damage);
    assert!(screens_match(live.screen(), &replica));
    let expected_rows = live.screen().export_state().primary.rows;
    assert!(expected_rows.iter().any(|row| row.wrapped));
    assert_eq!(replica.export_state().primary.rows, expected_rows);

    // A later frame returns to exact scroll events and incremental copying.
    live.feed(b"next frame\r\n");
    let next = live.take_damage();
    assert_eq!(next.scroll_events().len(), 1);
    replica.apply_damage(live.screen(), &next);
    assert!(screens_match(live.screen(), &replica));
}

#[test]
fn overflow_keeps_mixed_region_and_buffer_updates() {
    let mut live = Emulator::new(20, 8, 20);
    let mut replica = live.screen().clone();
    let _ = live.take_damage();
    for _ in 0..600 {
        live.feed(b"\x1b[2;7r\x1b[7;1Hdown\n\x1b[2;1H\x1bMup");
    }
    live.feed("\x1b[r\x1b[H\x1b[31mfinal e\u{301} 中".as_bytes());
    let damage = live.take_damage();
    assert!(damage.scroll_events().is_empty());
    assert_eq!(damage.dirty_cell_count(), 20 * 8);
    replica.apply_damage(live.screen(), &damage);
    assert!(screens_match(live.screen(), &replica));
    live.feed(b"\x1b[?1049h");
    for _ in 0..600 {
        live.feed(b"alternate\r\n");
    }
    let damage = live.take_damage();
    assert!(damage.scroll_events().is_empty());
    assert_eq!(damage.dirty_cell_count(), 20 * 8);
    replica.apply_damage(live.screen(), &damage);
    assert!(screens_match(live.screen(), &replica));
}
