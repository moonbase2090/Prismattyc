//! PT-72 spike evidence: the emulator is a deterministic state machine over
//! an ordered log of `Output(bytes)` and `Resize(cols, rows)` events.
//! Replaying the same log with any chunking yields an identical `Screen`;
//! resizes replay identically when they sit at the same byte offset, and a
//! resize at a different offset is a different state. This is the property a
//! replicated pane log (snapshot + tail, client-side replicas) relies on.
//!
//! The fixture is 12 KB recorded from a real PTY session (`ls --color`,
//! SGR/256-color, wide chars and emoji, `\e[2J`, `top`, the alternate
//! screen, a scroll region), so cuts fall inside escape sequences and
//! multi-byte UTF-8 as well as between them.

use prismattyc_emulator::Emulator;

const COLS: usize = 80;
const ROWS: usize = 24;
const SCROLLBACK: usize = 10_000;
const FIXTURE: &[u8] = include_bytes!("fixtures/pt72-session.bin");

#[derive(Clone, Copy)]
enum Ev {
    Out(usize, usize),
    Resize(usize, usize),
}

fn replay(bytes: &[u8], plan: &[Ev]) -> Emulator {
    let mut e = Emulator::new(COLS, ROWS, SCROLLBACK);
    for ev in plan {
        match *ev {
            Ev::Out(a, b) => {
                let _ = e.feed(&bytes[a..b]);
                let _ = e.take_pending_replies();
            }
            Ev::Resize(c, r) => e.resize(c, r),
        }
    }
    e
}

/// Split `[0, len)` at `cuts`; insert each resize before the chunk that
/// starts at its offset.
fn plan(len: usize, cuts: &[usize], resizes: &[(usize, usize, usize)]) -> Vec<Ev> {
    let mut points: Vec<usize> = cuts
        .iter()
        .copied()
        .chain(resizes.iter().map(|r| r.0))
        .filter(|&o| o > 0 && o < len)
        .collect();
    points.push(0);
    points.push(len);
    points.sort_unstable();
    points.dedup();
    let mut out = Vec::new();
    for w in points.windows(2) {
        for r in resizes {
            if r.0 == w[0] {
                out.push(Ev::Resize(r.1, r.2));
            }
        }
        out.push(Ev::Out(w[0], w[1]));
    }
    out
}

fn every(len: usize, n: usize) -> Vec<usize> {
    (1..len).filter(|o| o % n == 0).collect()
}

fn lcg_cuts(len: usize, seed: u64, max: usize) -> Vec<usize> {
    let (mut s, mut o, mut v) = (seed, 0usize, Vec::new());
    while o < len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        o += 1 + (s >> 33) as usize % max;
        v.push(o);
    }
    v
}

const NO_RESIZE: [(usize, usize, usize); 0] = [];

#[test]
fn fixture_is_the_recorded_session() {
    assert!(
        FIXTURE.len() > 10_000,
        "fixture too small: {}",
        FIXTURE.len()
    );
    assert!(
        FIXTURE.windows(2).any(|w| w == b"\x1b["),
        "fixture carries CSI sequences"
    );
    assert!(
        FIXTURE.iter().any(|b| *b >= 0x80),
        "fixture carries multi-byte UTF-8"
    );
}

#[test]
fn replay_is_independent_of_chunking() {
    let len = FIXTURE.len();
    let whole = replay(FIXTURE, &plan(len, &[], &NO_RESIZE));
    let bytewise = replay(FIXTURE, &plan(len, &every(len, 1), &NO_RESIZE));
    let threes = replay(FIXTURE, &plan(len, &every(len, 3), &NO_RESIZE));
    let random = replay(FIXTURE, &plan(len, &lcg_cuts(len, 7, 97), &NO_RESIZE));
    assert_eq!(whole.screen(), bytewise.screen(), "byte-by-byte feed");
    assert_eq!(
        whole.screen(),
        threes.screen(),
        "3-byte chunks split escapes and UTF-8"
    );
    assert_eq!(whole.screen(), random.screen(), "random chunks");
    assert!(whole.screen().history_len() > 100, "the session scrolled");
}

#[test]
fn resizes_replay_identically_at_the_same_log_offset() {
    let len = FIXTURE.len();
    let resizes = [(len * 30 / 100, 100, 30), (len * 65 / 100, 60, 20)];
    let chunked = replay(FIXTURE, &plan(len, &[], &resizes));
    let bytewise = replay(FIXTURE, &plan(len, &every(len, 1), &resizes));
    let random = replay(FIXTURE, &plan(len, &lcg_cuts(len, 99, 250), &resizes));
    assert_eq!(chunked.screen(), bytewise.screen());
    assert_eq!(chunked.screen(), random.screen());
}

#[test]
fn resize_position_is_part_of_the_state() {
    // Reflow makes plain output converge after a later resize. Use cursor
    // addressing instead: column 70 clamps to the current right edge.
    let bytes = b"prefix\x1b[1;70H!";
    let at = [(6, 40, 24)];
    let shifted = [(bytes.len() - 1, 40, 24)];
    let a = replay(bytes, &plan(bytes.len(), &[], &at));
    let b = replay(bytes, &plan(bytes.len(), &[], &shifted));
    assert!(
        a.screen() != b.screen(),
        "cursor addressing must use the dimensions at its log offset"
    );
}
