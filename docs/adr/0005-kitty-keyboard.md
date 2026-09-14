# ADR-0005: Kitty keyboard protocol (CSI-u progressive enhancement)

- **Status:** Accepted (first claim slice)
- **Date:** 2026-08-01
- **Related:** [fidelity-matrix-v1.md](../fidelity-matrix-v1.md) extended keyboard, [ADR-0001](0001-host-selection-clipboard.md) host key ownership
- **Spec:** [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) (clean-room implementation)

## Context

Apps (neovim, helix, fish, …) opt into unambiguous key reporting via progressive
enhancement flags. Prismattyc previously only emitted legacy / modifyOtherKeys (`CSI 27`)
encodings, so those apps could not detect or use Kitty CSI-u.

## Decision

### D-K1 — Track progressive enhancement per screen

| Sequence | Behavior |
|----------|----------|
| `CSI = flags ; mode u` | Set flags (`mode` 1 replace / 2 or / 3 clear-bits) |
| `CSI > flags u` | Push current flags; set `flags` (default 0) |
| `CSI < n u` | Pop `n` stack entries (default 1); empty → flags 0 |
| `CSI ? u` | Reply `CSI ? flags u` (detection vs DA1) |
| Stack depth | Cap 16; main and **alternate** screens have independent stacks |
| RIS `ESC c` | Clear both stacks |

Bare `CSI u` remains **SCORC** (restore cursor) — not a keyboard control.

### D-K2 — Encode keys from active flags

When flags are non-zero, the host encodes keys for the child using Kitty rules:

| Flag | Effect (Prismattyc slice) |
|------|----------------------|
| Disambiguate (1) | Esc / Ctrl / Alt / Super → `CSI … u`; Enter/Tab/BS stay legacy C0 when unmodified |
| Event types (2) | Repeat/release reported (`mods:type`); host delivers Release events |
| Alternate keys (4) | Shifted codepoint subfield when Shift present (ASCII alpha) |
| Report all (8) | All keys as escape codes (incl. plain text keys, Enter as `CSI 13 u`) |
| Report text (16) | With report-all: associated text codepoint field |

Legacy encoding unchanged when flags are 0.

### D-K3 — Host chords still win

Find, selection, scrollback, and copy chords are handled before Kitty encoding
([ADR-0001](0001-host-selection-clipboard.md)). Alt-screen still refuses host
selection; keys forward with the alt-screen keyboard flags.

### D-K4 — Explicit non-goals (this slice)

- Full base-layout (PC-101) alternate key reporting for all layouts
- Caps/Num lock bits in the modifier field
- Keypad vs main disambiguation table completeness
- Outer-host negotiation of Kitty protocol (Prismattyc as *app* under Kitty)

## Consequences

**Positive:** Modern TUIs can enable CSI-u under Prismattyc; detection query works.

**Negative:** Incomplete alternate-key / lock coverage; apps needing every flag
edge case may still differ from kitty itself.

## Mapping to code

| Behavior | Location |
|----------|----------|
| Flag stacks / parse | `prismattyc-emulator` `KeyboardModeStack`, `csi_dispatch` |
| Encode | `prismattyc` `encode_key_to_pty` / `encode_key_kitty*` |
| Release delivery | host event loop when `keyboard_reports_event_types` |

## References

- https://sw.kovidgoyal.net/kitty/keyboard-protocol/
