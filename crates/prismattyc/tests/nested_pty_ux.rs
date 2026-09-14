//! Nested outer-PTY **host UX** scripts (automated dogfood).
//!
//! These drive a real `prismattyc` binary under `portable-pty`, inject
//! keyboard/mouse encodings, and assert on painted output (child markers,
//! find chrome, OSC scroll title).
//!
//! Complements unit tests in `main.rs` (synthetic crossterm events) and human
//! Kitty/Ghostty smoke. Outer-host chord theft is intentionally out of scope.
//!
//! Unix-only (PTY).

#![cfg(unix)]

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;

use support::PtyUx;

const T: Duration = Duration::from_secs(8);

/// Scrollback fixture: enough lines to pan, plus a find needle.
fn scroll_find_fixture() -> &'static str {
    r#"
i=0
while [ "$i" -lt 40 ]; do
  printf 'LINE_%02d_SCROLL\n' "$i"
  i=$((i + 1))
done
printf 'FINDME_UniqueToken_xyz\n'
printf 'FixtureReady\n'
"#
}

/// Last OSC window title scroll offset from a host transcript.
/// - `OSC 0;prismattyc — scroll N/M BEL` → `Some(N)`
/// - `OSC 0;prismattyc BEL` (live) → clears to `None`
fn last_scroll_offset(t: &str) -> Option<usize> {
    let mut last: Option<usize> = None;
    let mut rest = t;
    while let Some(rel) = rest.find("\u{1b}]0;") {
        let after = &rest[rel + 4..];
        let end = after.find('\u{07}').unwrap_or(after.len());
        let title = &after[..end];
        if let Some(body) = title.strip_prefix("prismattyc — scroll ") {
            let n_str = body.split(['/', ' ']).next().unwrap_or("");
            if let Ok(n) = n_str.parse::<usize>() {
                last = Some(n);
            }
        } else if title == "prismattyc" {
            last = None;
        }
        rest = if end < after.len() {
            &after[end + 1..]
        } else {
            ""
        };
    }
    last
}

#[test]
fn ux_scroll_shift_pageup_sets_osc_title() {
    let mut ux = PtyUx::spawn_fixture_script(scroll_find_fixture());
    ux.wait_for("FixtureReady", T);
    // Give the host a paint tick after the flood.
    std::thread::sleep(Duration::from_millis(80));

    // One Shift+PageUp should leave live view when history exists.
    ux.key_shift_page_up();
    ux.wait_for("prismattyc — scroll", T);

    // Shift+End returns to live → title resets to bare "prismattyc".
    ux.key_shift_end();
    ux.wait_until(
        |t| {
            // After live, sync_host_title writes OSC `prismattyc` without "scroll".
            t.contains("\u{1b}]0;prismattyc\u{07}")
                && t.rfind("prismattyc — scroll").map(|i| {
                    // A later bare title exists after the last scroll title.
                    t[i..].contains("\u{1b}]0;prismattyc\u{07}")
                }) == Some(true)
        },
        T,
        "OSC title back to live prismattyc",
    );
}

#[test]
fn ux_scroll_mouse_wheel_sets_osc_title() {
    let mut ux = PtyUx::spawn_fixture_script(scroll_find_fixture());
    ux.wait_for("FixtureReady", T);
    std::thread::sleep(Duration::from_millis(80));

    // Several wheel notches (~3 rows each) to guarantee offset > 0.
    for _ in 0..5 {
        ux.mouse_wheel_up(40, 12);
        std::thread::sleep(Duration::from_millis(20));
    }
    ux.wait_for("prismattyc — scroll", T);
}

#[test]
fn ux_find_opens_and_live_matches_case_insensitive() {
    let mut ux = PtyUx::spawn_fixture_script(scroll_find_fixture());
    ux.wait_for("FINDME_UniqueToken_xyz", T);
    ux.wait_for("FixtureReady", T);
    std::thread::sleep(Duration::from_millis(80));

    // Kitty progressive encoding: Ctrl+Shift+; (plain PTY has no real Kitty UI).
    ux.open_find();
    ux.wait_for(" Find:", T);

    // Case-insensitive: query lower-case against mixed marker.
    ux.type_text("findme_uniquetoken");
    ux.wait_for(" Find: findme_uniquetoken", T);
    // Match counter chrome (unique needle → 1/1).
    ux.wait_for("1/1", T);

    ux.key_esc();
    // Esc clears find mode; subsequent paint should not keep expanding the prompt
    // with new typed text going only to the child.
    std::thread::sleep(Duration::from_millis(50));
    let before = ux.transcript();
    ux.type_text("Z");
    std::thread::sleep(Duration::from_millis(80));
    let after = ux.transcript();
    let new = &after[before.len().min(after.len())..];
    assert!(
        !new.contains(" Find: findme_uniquetokenZ") && !new.contains("Find: Z"),
        "find mode should exit on Esc; new output tail={new:?}"
    );
}

/// Review: prompt echo alone is a false positive — require scroll jump on hit,
/// and no *new* scroll title on a guaranteed miss.
#[test]
fn ux_find_hit_scrolls_history_miss_does_not() {
    let script = r#"
printf 'NEEDLE_ONLY_AT_TOP unique_scroll_hit\n'
i=0
while [ "$i" -lt 50 ]; do
  printf 'pad_line_%02d\n' "$i"
  i=$((i + 1))
done
printf 'FixtureReady\n'
"#;
    let mut ux = PtyUx::spawn_fixture_script(script);
    ux.wait_for("FixtureReady", T);
    std::thread::sleep(Duration::from_millis(100));

    ux.open_find();
    ux.wait_for(" Find:", T);
    ux.type_text("unique_scroll_hit");
    // Match above the live viewport → host sets view_scroll and OSC title.
    ux.wait_for("prismattyc — scroll", T);

    ux.key_esc();
    std::thread::sleep(Duration::from_millis(40));
    // Return to live so a miss cannot inherit a confusing mid-history title update.
    ux.key_shift_end();
    std::thread::sleep(Duration::from_millis(40));

    // Return to live again after esc (find may leave scroll where the hit was).
    ux.key_shift_end();
    std::thread::sleep(Duration::from_millis(60));
    ux.wait_until(
        |t| {
            t.rfind("prismattyc — scroll")
                .map(|i| t[i..].contains("\u{1b}]0;prismattyc\u{07}"))
                .unwrap_or(true)
        },
        T,
        "back to live before miss query",
    );

    ux.open_find();
    ux.wait_for(" Find:", T);
    let before_miss = ux.transcript();
    // No prefix of this string appears in the fixture (avoid live-first-match
    // on single letters like "n" → NEEDLE).
    ux.type_text("@@@nomatch@@@");
    ux.wait_for(" Find: @@@nomatch@@@", T);
    let after = ux.transcript();
    let new = &after[before_miss.len().min(after.len())..];
    assert!(
        !new.contains("prismattyc — scroll"),
        "absent find must not open a new scroll title; new={new:?}"
    );
}

/// Dual-sign review: must fail if Enter / Shift+Enter are no-ops.
/// Assert **scroll offset** changes on next and restores on prev — not mere
/// transcript growth or prompt echo.
#[test]
fn ux_find_enter_advances_when_multiple_matches() {
    // Early match high in history; late match near live → different view_scroll.
    let script = r#"
printf 'alpha_match_early\n'
i=0
while [ "$i" -lt 40 ]; do
  printf 'gap_%02d\n' "$i"
  i=$((i + 1))
done
printf 'alpha_match_late\n'
printf 'FixtureReady\n'
"#;
    let mut ux = PtyUx::spawn_fixture_script(script);
    ux.wait_for("FixtureReady", T);
    std::thread::sleep(Duration::from_millis(100));

    ux.open_find();
    ux.wait_for(" Find:", T);
    ux.type_text("alpha_match");
    ux.wait_for(" Find: alpha_match", T);
    ux.wait_for("1/2", T);
    ux.wait_for("prismattyc — scroll", T);

    let off_first = last_scroll_offset(&ux.transcript()).expect("first match scrolls");
    assert!(
        off_first > 0,
        "early match must leave live (offset>0), got {off_first}"
    );

    // Enter → next match (late, nearer live): offset must change (often → 0 / live).
    ux.key_enter();
    ux.wait_until(
        |t| match last_scroll_offset(t) {
            Some(n) if n != off_first => true,
            None => true, // live title — offset cleared
            Some(_) => false,
        },
        T,
        "Enter next changes scroll offset",
    );
    let off_next = last_scroll_offset(&ux.transcript());
    assert_ne!(
        off_next,
        Some(off_first),
        "Enter must move match; still at offset {off_first}"
    );
    assert!(
        ux.transcript().contains(" Find: alpha_match"),
        "find prompt remains after Enter"
    );
    ux.wait_for("2/2", T);

    // Shift+Enter → previous: restore original early-match offset.
    ux.key_shift_enter();
    ux.wait_until(
        |t| last_scroll_offset(t) == Some(off_first),
        T,
        "Shift+Enter prev restores first scroll offset",
    );
    ux.wait_for("1/2", T);
    assert_eq!(
        last_scroll_offset(&ux.transcript()),
        Some(off_first),
        "prev must return to first match offset"
    );
}

#[test]
fn ux_fast_child_still_paints_before_exit() {
    // Regression already covered in live_pty_fast_child; keep one harness-path
    // smoke so the PtyUx helper is exercised for short-lived children too.
    let ux = PtyUx::spawn(&["--", "/usr/bin/printf", "HARNESS_FAST_OK"]);
    ux.wait_for("HARNESS_FAST_OK", Duration::from_secs(5));
    // Child exits; prismattyc should exit soon. Don't require exit code (Drop kills).
}
