# Spike Baseline v0

**Status:** Published for Phase 0A exit / Phase 0B entry.

**Kind:** Internal subset gate — **not** a product release, modern-terminal
compatibility claim, or full VT fidelity matrix.

**Frozen implementation head:** `e56d868` (on main after
rebase onto PRD freeze `8fefdc6`).

Related: [PRD.md](PRD.md) §2.6 (Phase 0A exit), §5 (testing),
[capability-protocol.md](capability-protocol.md), [decisions-v0.md](decisions-v0.md).

## Purpose

Phase 0A exits only when this baseline has **no open P0** in the enumerated
fixture set and every fixture passes at the frozen head. Phase 0B may then
proceed as an **experimental** validation spike; it must not weaken these
classic invariants.

## Scope covered

| Area | Baseline requirement | Evidence at frozen head |
|------|----------------------|-------------------------|
| Real PTY | Spawn, child output, child exit | `real_pty_captures_child_output`; `prism` binary path |
| Input | Raw stdin → child PTY | Host loop + `take_input_writer` (manual/PTY smoke) |
| Host restore | Raw mode + alternate screen RAII restore; Unix SIGINT/SIGTERM/SIGHUP flag→exit | `TerminalGuard` Drop + signal handlers in `crates/prismattyc` |
| Printable text | Single-column ASCII print | `wraps_only_when_the_next_character_arrives`; VT parse tests |
| CR / LF / BS / TAB | Documented C0 handling | Emulator `execute` + grid unit tests |
| Cursor / erase | CUP/HVP, CUU/CUD/CUF/CUB, CHA, EL, ED subset | `parses_text_cursor_motion_and_erasure`; core cursor tests |
| SGR | Bold/italic/underline/inverse + basic 16-color | `parses_basic_sgr_attributes_and_colors` |
| Delayed autowrap | Wrap only when next character arrives | `wraps_only_when_the_next_character_arrives` |
| Scrollback | Bounded; 10k-line host default | `scrolling_is_bounded`; `MAX_SCROLLBACK_LINES = 10_000` |
| Deterministic render | Stable plain-text grid shape | `plain_text_preserves_grid_shape` |
| ANSI render | Style emission + cursor restore | `ansi_renderer_emits_style_and_restores_cursor` |
| Containment | Unsupported OSC must not leak as text | `unsupported_sequences_do_not_leak_payload_text` |
| Zero-size host | Fallback 80×24 | `zero_sized_synthetic_terminal_gets_a_usable_fallback` |
| CI merge gate | fmt / check / test / clippy locked | `.github/workflows/ci.yml`; live GHA green run `30417135305` @ `63dbac8` |

## Explicit missing-feature allowlist

A spike application **must not** depend on these. Absence is **not** a Phase 0A
P0:

- child alternate-screen buffer model
- mouse reporting
- live SIGWINCH / host resize loop
- wide Unicode / grapheme cluster correctness
- bracketed paste
- extended keyboard protocols
- APC capability **decoder/encoder** on the host (freezes types + design only)
- selection/copy UI (Phase 1)
- full xterm / charter fidelity matrix (Phase 1 product gate)

## Fixture inventory (exact-head gate)

Re-run on every claimed SHA:

```bash
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --bin prism --locked
```

### Unit / integration fixtures (16 at freeze)

| Crate | Test | Baseline role |
|-------|------|---------------|
| prismattyc-core | `wraps_only_when_the_next_character_arrives` | Delayed autowrap |
| prismattyc-core | `scrolling_is_bounded` | Scrollback bound |
| prismattyc-core | `erase_line_uses_current_style` | Erase + style |
| prismattyc-core | `cursor_movement_is_clamped_to_the_grid` | Cursor clamp |
| prismattyc-emulator | `parses_text_cursor_motion_and_erasure` | VT cursor/erase |
| prismattyc-emulator | `parses_basic_sgr_attributes_and_colors` | 16-color SGR subset |
| prismattyc-emulator | `unsupported_sequences_do_not_leak_payload_text` | Containment |
| prismattyc-emulator | `real_pty_captures_child_output` | Real PTY smoke |
| prismattyc-render | `plain_text_preserves_grid_shape` | Deterministic render |
| prismattyc-render | `ansi_renderer_emits_style_and_restores_cursor` | ANSI render |
| prism | `zero_sized_synthetic_terminal_gets_a_usable_fallback` | 80×24 fallback |
| prismattyc-protocol | 5 semantic type tests | Capability first-cut types (not wire decoder) |

### Manual / process smoke (documented, not CI-asserted yet)

- Interactive: `prism` (or `prism /bin/sh`) in a real TTY; exit restores host.
- Child print smoke: spawn shell that prints a marker and exits.

## Non-claims (hard)

Do **not** describe this baseline as:

- modern-terminal compatible
- charter-complete classic fidelity
- production protocol freeze for rich APC traffic
- validated adoption / competitive differentiator (that is Phase 0B+)

## Phase 0A exit checklist

- [x] CI workflow on main
- [x] Capability design + semantic types on main; no rich emit enabled
- [x] Classic PTY vertical slice on main
- [x] This document published
- [x] Exact-head gates green at `e56d868` (local re-verify by operator-a)
- [x] Live GitHub Actions green on `origin/main` (run `30417135305` @ `63dbac8`)
- [x] No open P0 filed against the fixtures above
- [x] Hybrid composition freeze closed after mutual review @ `63dbac8`

## Adversarial review notes (non-blocking / not P0)

Recorded at merge review so they do not silently become claims:

1. **Full-grid re-render** on every PTY read chunk — acceptable for MVP; not a latency SLO.
2. **Stdin forwarder thread** is not joined on child exit — process teardown cleans up; no hang observed in unit path.
3. **No host SIGWINCH** — allowlisted; `PtySession::resize` exists but is unused by the bin loop.
4. **APC bodies are ignored** (empty `hook`/`put`/`unhook`) — correct for classic path; decoder is Phase 0B.
5. **Live GHA** proven green at run `30417135305` on tip `63dbac8` (earlier failures were account billing, not workflow defects).
6. **Erase uses current SGR style** for blanks — intentional first-cut; matrix may refine later.

## How to re-arm after head change

If main moves past the frozen head, either:

1. re-run the full gate on the new SHA and amend this document's frozen head, or
2. keep `e56d868` as the last known green baseline until re-validation.

Do not claim Phase 0A exit on an unverified tip.
