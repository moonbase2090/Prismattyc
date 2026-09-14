# ADR-0006: Windowed host

- **Status:** Accepted
- **Date:** 2026-08-04
- **Related:** [workspace.md](../workspace.md), charter “native binary”

## Context

`prismattyc` (nested host) is a high-fidelity classic emulator that **paints into an outer terminal** (Kitty/Ghostty) via crossterm/ANSI. That path:

- Is excellent for CI, exact-head gates, and development.
- Is a poor **daily driver**: outer hosts steal chords, own the window, fonts, and clipboard path.

The charter requires a **native host binary**. GPU/Wayland/X11 come after software-first correctness; nesting inside another terminal emulator is not a permanent requirement.

Owner adoption gate: use Prismattyc only when it has **its own OS window** (no Kitty/Ghostty required for normal use).

## Decision

### D-W1 — Two host fronts, one brain

| Binary | Role |
|--------|------|
| `prismattyc` | Nested classic host (TTY-backed). **Claim gate** for `prismattyc-classic/*` remains here. |
| `prismattyc-host` | **Windowed** OS host. Own window, fonts, input, clipboard path over time. |

Both reuse `prismattyc-core`, `prismattyc-emulator`, `prismattyc-protocol`, and (where useful) `prismattyc-render`. Nested and windowed must **not** fork VT semantics.

### D-W2 — MVP vertical slice

Ship a usable Linux-first windowed path that:

1. Opens a window titled for Prismattyc.
2. Spawns a child shell in a PTY (`PtySession`).
3. Parses child output with the same `Emulator` as nested.
4. Software-rasterizes the cell grid into the window (monospace font + palette).
5. Forwards basic keyboard input to the child.
6. Resizes PTY/grid when the window size changes (cell-metric based).
7. Exits cleanly on window close (child reaped via existing `PtySession` Drop).

**Out of this MVP (follow-ons):** host selection/clipboard parity with ADR-0001, hybrid mouse, scrollback chrome, Kitty CSI-u depth, GPU backends, macOS/Windows polish, mux panes.

### D-W3 — Software raster first

Default windowed paint is **CPU raster** (softbuffer or equivalent) + system monospace font (`fontdue` or successor). GPU present is optional capacity: `--features gpu --gpu` uploads the same CPU `u32` buffer. Not a production backend; softbuffer stays the default and the fallback.

### D-W4 — Classic claim boundary

`prismattyc-classic/*` fidelity matrix evidence stays on the **nested** path until a separate matrix (or additive rows) is written for windowed. Windowed bugs that break shared emulator/core are still P0 for the classic library claim.

### D-W5 — Feature and CI posture

- `prismattyc-host` is a workspace member; `cargo check/test -p prismattyc-host` must compile on Linux CI.
- The opt-in `gpu` feature is also `cargo check`'d in CI so it does not rot. Default tests do not require a GPU.
- GUI runtime tests may be `#[ignore]` or gated on `DISPLAY`/`WAYLAND_DISPLAY`.
- Nested `prismattyc` remains the default `cargo run -p prismattyc` path.

## Consequences

- Daily-driver dogfood moves to `prismattyc-host`.
- Nested `prismattyc` remains the automated claim harness.
- Input encoding and selection policy should eventually share modules between hosts (extract from `prismattyc` main when the windowed path grows).
- Document launch: `cargo run -p prismattyc-host -- /bin/sh`.

## Alternatives considered

| Option | Why not (now) |
|--------|----------------|
| Only nested forever | Fails charter + owner adoption |
| Replace nested immediately | Breaks CI/claim ergonomics |
| GPU-only first paint | Violates software-first |
| Electron / webview shell | Charter non-goal |
