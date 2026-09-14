# Host UX testing

Prismattyc is dogfooded both **by hand** (real Kitty/Ghostty) and **in CI** (nested PTY).

## Layers

| Layer | Where | What it proves | Gaps |
|-------|--------|----------------|------|
| Unit (host logic) | `crates/prismattyc/src/main.rs` tests | Synthetic crossterm keys/mouse → scroll, find, select policy | No real paint / PTY |
| Core / render | `prismattyc-core`, `prismattyc-render` | History find, grid, ANSI selection | No host loop |
| Nested outer-PTY UX | `crates/prismattyc/tests/nested_pty_ux.rs` + `tests/support/` | Real `prismattyc` binary under PTY; inject Kitty/xterm key & SGR mouse encodings; assert transcript (find chrome, OSC title, child markers) | No outer GUI; no real Kitty chord theft |
| Live PTY lifecycle | `crates/prismattyc/tests/live_pty_fast_child.rs` | EOF paint, alt paint, SIGTERM under flood | Not interactive UX scripts |
| Human smoke | Bug-log H-matrix / ad-hoc | Outer-host chords, IME, desktop grabs (Ctrl+Space), feel | Not automated |

## Nested PTY harness (`tests/support`)

`PtyUx`:

1. Opens a PTY and spawns `CARGO_BIN_EXE_prismattyc` with a fixture child.
2. Drains master output into a shared transcript.
3. Writes **raw encodings** that crossterm accepts on a dumb PTY:
   - Kitty keyboard: `CSI codepoint ; modifier u` (e.g. Ctrl+Shift+`;` → `\x1b[59;6u`)
   - xterm modified specials: Shift+PageUp `\x1b[5;2~`, Shift+Home/End, …
   - SGR mouse wheel: `\x1b[<64;col;rowM`
4. `wait_for` / `wait_for_stripped` for markers.

Fixture children print markers then `sleep` so the host loop stays alive.

## Human dogfood (keep doing this)

Still required for:

- Kitty may steal **Ctrl+Shift+F** or **Ctrl+Shift+/**. Use **Ctrl+Shift+;**.
- Desktop/IME may consume **Ctrl+Space** or **Ctrl+2** before the host sees them.
- Linux IME acceptance: check `fcitx5-diagnose` or `ibus engine` to identify
  the active input method. In `prismattyc-host`, compose a short CJK string.
  Confirm that the uncommitted text stays in the focused pane, has an underline,
  and does not reach the child PTY. Press Enter to commit it, then confirm the
  child receives the exact UTF-8 text. Start a new composition and dismiss it
  to confirm the preedit disappears without a commit.
- Treat Ctrl+Space results as environment-dependent. The active IME or desktop
  may consume the chord. Use Ctrl+2 or Shift+arrow for a host mark when needed.
- Real font/glyph, GPU window, multi-monitor feel
- “Does this feel right?” timing and chrome

Automated tests intentionally use Kitty progressive *encodings* on a plain PTY so CI can deliver Ctrl+Shift without a GUI Kitty.

## Adding a script

1. Prefer a **unit test** if pure host policy (no need for paint bytes).
2. Else add a `#[test]` in `nested_pty_ux.rs` with a small shell fixture and a stable marker string.
3. Assert on:
   - child text markers,
   - ` Find:` prompt,
   - OSC `prismattyc — scroll N/M`,
   - not on pixel-perfect layouts.
4. Timeouts ~5–8s; always `Drop`/`kill` via `PtyUx`.

## Running

```sh
cargo test -p prismattyc --test nested_pty_ux --locked
cargo test -p prismattyc --test live_pty_fast_child --locked
cargo test --workspace --locked
```
