# ADR-0010 — Windowed mux chrome and direct pane keys

**Status:** Accepted; **tab out-of-scope superseded by
[ADR-0012](0012-host-tabs.md)**
**Date:** 2026-08-11
**Depends on:** ADR-0006

## Context

The windowed host could compose multiple live panes, but focus was visible
only through the terminal caret and panes could be created only at startup with
`--panes N`. Operators need persistent, discoverable mux affordances without a
mandatory prefix mode, and background output must be findable without changing
client-local focus.

## Decision

`prismattyc-host` keeps the terminal grid full-window by default (**no permanent menu
bar**). Direct mux keys remain always active. A compact **bottom chord strip**
is painted as a pixel overlay only while **Ctrl+Shift** is held (does not
resize the PTY / SIGWINCH the child). Unseen-output badges stay on pane chrome.

The overlay identifies pane count and exposes the direct keys:

| Key | Action |
|---|---|
| `Ctrl+Shift+\` or `Ctrl+Shift+E` | split the focused pane to the right |
| `Ctrl+Shift+-` or `Ctrl+Shift+D` | split the focused pane downward |
| `Ctrl+Shift+W` | close the focused pane when another pane remains |
| `Ctrl+Shift+X` | detach this session view; last tab exits the host |
| `Alt+Arrow` | move focus spatially to the nearest pane in that direction |

Primary matching uses **physical scancodes** (layout-independent). `E`/`D` are
layout-friendly alternatives when `\`/`-` are awkward or stolen by the compositor.
These are the **defaults**; ADR-0015 lets `[keys]` in config.toml rebind them.

There is no leader or hidden mode. Spatial focus prefers candidates overlapping
the current pane on the perpendicular axis, then the nearest center, with stable
pane ID as the final tie-breaker.

When more than one pane is present, each pane receives a **thin (1px)** structural
outline after its terminal cells are rasterized. Unfocused outlines use neutral
chrome border; the focused outline uses a **brand spectrum** color (default
blue). Single-pane layouts draw **no** outline.

Focus border color is user-selectable at runtime:

| Mechanism | Example |
|---|---|
| Env | `PRISMATTYC_FOCUS_BORDER=violet` (or `coral` / `amber` / `yellow` / `green` / `blue` / `ink` / `0`–`6`) |
| CLI | `prismattyc-host --focus-border amber` |
| Chord | `Ctrl+Shift+]` cycles the spectrum; title shows the active name |

A background pane whose screen content epoch changes receives an amber square
`!` badge. Focusing the pane clears its badge. The OS title exposes pane and
unseen counts; chord help is a bottom overlay while Ctrl+Shift is held.

`--panes N` remains available and the default remains one pane. Full
multi-window tabs shipped later; chrome and chords are in
[ADR-0012](0012-host-tabs.md).

## Proof

- Host unit/PTY tests cover direct chord mapping, spatial navigation, split and
  close reflow, unseen-output set/clear, and preserved 80×24 default geometry.
- Raster tests assert bottom footer overlay, three-pixel focus, and shaped unseen
  badge pixels without mutating terminal state.
- Nested Termwright remains the shared-VT regression gate; it does not substitute
  for the real OS-window proof.

## Consequences

- Pane state and actions are visible without scraping child output.
- Terminal applications retain every negotiated cell; chrome does not consume a
  child row or leak a shortcut into the PTY.
- Output badges are client-local view state and do not mutate mux topology.

## Split working directory

New panes inherit the **focused pane’s cwd** when splitting:

1. **OSC 7** from the child (`file://…`) when the shell reports it
2. Else Linux **`/proc/<pid>/cwd`** of the focused pane’s process

If neither is available, the child inherits the host process cwd (prior behavior).

## Out of scope

- Full multi-window/tab creation and switching — **superseded by
  [ADR-0012](0012-host-tabs.md)**
- Command palette (PT-42). Remappable chords are **superseded by
  [ADR-0015](0015-user-keybindings.md)**: the table above lists defaults; `[keys]` in
  config.toml overrides them. Leader keys and modal profiles stay out of scope.
- Controller-lease/read-only chrome for external clients
- Detach/reattach lifetime
