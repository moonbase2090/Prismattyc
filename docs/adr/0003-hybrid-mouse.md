# ADR-0003: Hybrid mouse (app report + Shift host select)

- **Status:** Accepted
- **Date:** 2026-07-31
- **Charter:** classic path first; host chrome must not corrupt the child PTY model
- **Supersedes in part:** [ADR-0002](0002-host-mouse-policy.md) D-M1 host-selection-only claim
  (implements **Option B** from D-M5)
- **Related:** [ADR-0001](0001-host-selection-clipboard.md), fidelity matrix app-mouse row
- **Does not supersede:** ADR-0001 selection/clipboard ownership; alt-screen copy-chord
  rules for **unmodified** host gestures when app mouse is off

## Context

ADR-0002 froze classic 0.1.x as **host selection only**: DECSET mouse modes were
accepted as no-ops and Prismattyc never emitted SGR/X10 reports. That was honest and
shippable, but nested vim/htop/less-with-mouse still feel broken under a rich
`TERM` — clicks only select host text.

Users expect **Kitty/xterm-class hybrid** behavior:

1. When the child enables application mouse tracking, **plain** mouse events go
   to the child as protocol reports.
2. **Shift** (or host-only paths when tracking is off) keeps Prismattyc selection /
   scrollback pan.

This ADR selects that hybrid and defines the minimal mode table + report encoding
required to claim it without reopening dual-consume bugs.

## Decision

### D-H1 — Hybrid ownership rule

| Condition | Mouse event owner |
|-----------|-------------------|
| App mouse tracking **off** | Host (ADR-0002 path): selection, multi-click, scrollback wheel |
| Tracking **on** and **Shift held** | Host: selection (and Shift+wheel = page pan on primary) |
| Tracking **on** and Shift **not** held | Child: encode and queue report on host→child PTY write path |
| Tracking **off** + alt screen | Host refuses selection; wheel does not pan primary history |
| Tracking **on** + alt + plain | Child reports (vim/htop on alt work) |
| Tracking **on** + alt + Shift | Host selection allowed (explicit override; copy chords follow ADR-0001) |

**Invariant:** a single gesture is never both host-selected and app-reported.

### D-H2 — Mode table (stored, not ignored)

Prismattyc tracks child DECSET/DECRST for:

| Mode | Meaning | Prismattyc action |
|------|---------|--------------|
| **1000** | Click tracking (press/release) | Enable click-level tracking |
| **1002** | Cell motion (drag with button) | Enable drag-level tracking (includes clicks) |
| **1003** | Any motion | Enable any-motion tracking |
| **1006** | SGR mouse encoding | Prefer SGR reports when encoding reports |
| 1005 / 1015 / 1016 | Alternate encodings | Accept CSI; **do not** implement encoding variants in this claim |

**Level:** the highest enabled of 1000/1002/1003 wins (`Any` > `Drag` > `Click` >
`Off`). Enabling a level sets that flag; disabling clears that flag only.
Disabling all three → tracking off.

**RIS** (`ESC c`): clear mouse tracking flags and SGR flag (with other private modes).
**DECSTR** (`CSI ! p`): does **not** clear mouse modes (xterm-ish; keeps editor
state across soft reset).

### D-H3 — Report encoding (classic claim)

When tracking is on and the event is owned by the child:

1. If **1006** is set → **SGR** mouse: `CSI < Cb ; Cx ; Cy M` (press/motion) or
   `… m` (release). Coordinates are **1-based** cell positions.
2. If 1006 is **not** set → **legacy X10**: `ESC [ M` + `(Cb+32)` `(Cx+32)` `(Cy+32)`
   with 223-cell clamp (best-effort for rare non-SGR clients).

**Button / modifier bits** (SGR `Cb`, X10 same base):

| Event | Base |
|-------|------|
| Left / Middle / Right press | 0 / 1 / 2 |
| Release (SGR uses same base + final `m`) | 0 / 1 / 2 |
| Motion while button held (1002/1003) | base + 32 |
| Wheel up / down | 64 / 65 |
| +Shift / +Alt / +Ctrl | +4 / +8 / +16 |

**Filter by level:**

| Level | Report |
|-------|--------|
| Click (1000) | Down + Up only (no Drag/Moved) |
| Drag (1002) | Down, Up, Drag (not bare Moved) |
| Any (1003) | Down, Up, Drag, Moved |

Wheel reports whenever tracking ≠ Off (apps expect scroll).

### D-H4 — Host capture stays on

Prismattyc continues to enable host `EnableMouseCapture` so hybrid routing is possible.
Outer terminals that steal mouse before Prismattyc never reach this path — unchanged
limitation.

### D-H5 — Explicit non-goals (this ADR)

- Pixel / urxvt / UTF-8 mouse encodings (1016 / 1015 / 1005)
- Right-click context menus
- Focus-in/out (1004) as part of mouse claim
- Changing ADR-0001 copy-chord policy beyond “selection only exists if host path built it”
- Kitty full keyboard protocol interaction with mouse

### D-H6 — Product claim wording

After implementation:

- **Application mouse reporting** is **claimed** for 1000/1002/1003 + SGR 1006
  under the hybrid rule above.
- README: plain click → app when the child enables mouse; **Shift+drag** still
  host-selects.
- Matrix exclusion for “app mouse” is **removed**; hybrid is the documented policy.
- ADR-0002 remains historical policy for the pre-hybrid tip; its D-M1 claim is
  **superseded for tips that include this ADR**.

## Consequences

**Positive**

- vim/htop/less mouse work under Prismattyc without giving up host selection.
- Shift escape hatch matches common terminal muscle memory.
- Mode table is small and testable (emulator state + encode unit tests).

**Negative**

- More edge cases (mid-drag mode change, wheel ownership, alt+Shift select).
- Legacy X10 is lossy on large grids — mitigated by “apps use 1006” reality.

**Follow-ups**

- Optional: human smoke — click **does** move vim cursor when mouse on;
  Shift+drag still selects.
- Optional: 1004 focus events later under a separate ADR.

## Addendum — private DECSET 7700

xterm 1000/1002/1003 always claim buttons as well as wheel. Mux attach needs
wheel reports for scrollback without stealing host drag-select.

| Mode | Meaning | Prismattyc action |
|------|---------|--------------|
| **7700** | Wheel-only reporting (Prismattyc-private) | Encode wheel as SGR/X10; press/drag/release stay host-owned |

`mouse_tracking()` remains 1000/1002/1003 only. Host routes wheel to the app when
7700 **or** those levels are on. Attach sets `7700+1006` by default and mirrors
the child's real 1000-level modes onto the outer terminal when the child enables
mouse. Non-Prismattyc outer terminals ignore 7700; PageUp / `C-\ [` still enter
scrollback.

Attach also uses DECSET 1049 for its paint surface. That must not be treated as
a guest TUI (vim) alt screen: when 7700 is on and 1000/1002/1003 are off, the
host allows plain drag-select.

## Mapping to code

| Behavior | Location |
|----------|----------|
| Store 1000/1002/1003/1006 | `prismattyc-emulator` private mode + `MouseTracking` API |
| Encode SGR / X10 | `prismattyc` host `encode_mouse_report` |
| Route Shift vs plain | `handle_mouse` + `to_child` queue |
| RIS clear modes | emulator `esc_dispatch` RIS arm |
| Evidence tests | emulator mode tracking; host encode + hybrid routing |

## References

- ADR-0002 D-M5 Option B
- xterm control sequences (mouse tracking / SGR)
