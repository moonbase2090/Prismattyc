# ADR-0012 — Host tabs (window-as-tab chrome)

**Status:** Accepted (amended for strip click-to-select, inline rename,
per-tab close glyph, middle-click close, child-exit cascade, rail insets,
title-icon scaling, and session-ended placeholders on attach panes)
**Date:** 2026-08-13
**Depends on:** ADR-0007; ADR-0010;
ADR-0008 (`RenameWindow`)
**Supersedes:** ADR-0010 “Out of scope — Full multi-window/tab creation and
switching”

## Context

ADR-0010 deferred tabs until a multi-window runtime existed. That runtime
landed: `Window` is the tab, active tab is client-local
(`ClientView.window`), and chords create/close/cycle/select/move panes
across tabs. Operators still have no persistent visual of the tab set —
only the OS title and per-pane badges.

## Decision

### Window is the tab

A `Window` is a tab, as defined in [ADR-0007](0007-phase2-mux-domain.md).

### Chrome

`prismattyc-host` still has **no permanent menu bar** on the single-tab path.
A compact **top tab strip** is reserved only when `tab_count > 1`:

- `HostGeom.top_chrome_px` is one cell row (`font.cell_h`) or zero.
- Pane slots are offset downward by that amount. The 80×24 default and
  single-tab pixels stay identical to ADR-0010.
- Each tab shows a compact title plus aggregated unseen (`!`) and active
  (`*`) badges for that window’s panes. Slots are inset from both window
  edges by `window_padding_px` (same as pane slots), while each slot's title,
  pane handles, badges, and close control use `pane_padding_px` so the tab
  content aligns with the terminal content below. Close-glyph air remains
  inside the slot, not a shrunken rail. Adjacent slots are separated by
  `pane_gap_px` of chrome (dead to clicks). 6×6 badges sit on the same
  horizontal centerline as the close glyph. The strip background still spans
  the full width.
- The **active** tab is marked by **shape and** the current
  `focus_border` spectrum color, never color alone. Inactive
  tabs stay neutral chrome.
- No idle animation on the strip.

The Ctrl+Shift **bottom** chord overlay from ADR-0010 is unchanged.

### Chords (indexes are presentation-only)

| Key | Action |
|---|---|
| `Ctrl+Shift+T` | new tab |
| `Ctrl+Shift+Q` | close the active tab (when another remains) |
| `Ctrl+Shift+W` | close focused pane; last pane of a multi-tab window closes the tab |
| `Ctrl+Shift+X` | detach this session view; last tab exits the host (server-owned sessions stay) |
| `Ctrl+Shift+PageUp` / `PageDown` | previous / next tab |
| `Ctrl+Shift+Digit1`…`9` | select tab by presentation index |
| `Ctrl+Shift+Alt+PageUp` / `PageDown` | move the focused pane to the previous / next tab |
| `Ctrl+Shift+R` | begin inline rename of the active tab |

**Amendment (PT-69):** drag a tab chip left or right to reorder. A tab
with more than one pane shows a one-cell handle per pane in the strip;
drop that handle on another tab to `MovePane`, or on the empty end of
the strip to open a new tab with that pane. The last pane leaving a tab
closes the tab. While a drag is live the strip is drawn even when
`tab_count == 1` so the empty end is a drop target. PTY mouse is not
used (ADR-0002). `move_tab_left` / `move_tab_right` are unbound palette
actions. The attach-tabs cache is rewritten after every reorder or move.

Child exit of a **local shell** uses the same cascade as `Ctrl+Shift+W`:
close the pane; an empty tab closes and selects the neighbor
(`Ctrl+Shift+Q` rule); last pane of the last tab exits the host.
Unfocused-tab exit does not steal focus.

**Amendment (PT-68):** an **attach** pane (`pmux attach` child) does not
collapse. The slot becomes a placeholder: session name, exit reason, and
`Enter to reopen`. Enter attaches again when the session is live. When it
is gone, the host recreates it from the last opened space (`PMUX_SPACE`,
else `default`) using the saved name, cwd, and agent. If that space has
no row, the placeholder prints `session NAME is gone` and stays.
`Ctrl+Shift+W` on an intermediate placeholder removes the slot; on the
last placeholder of a non-last tab it closes the tab; on the last
placeholder of the last tab it quits the host. `all_children_exited` is
false while any placeholder remains.

### Strip mouse (strip exists only when `tab_count > 1`)

Presentation index → `WindowId` is resolved at the click. Hits
in the `top_chrome_px` band never reach pane selection or app mouse tracking.

- Title glyphs whose ink is wider or taller than the slot cell (Nerd
  spinners, wide OSC titles) are rasterized smaller and
  shifted to stay inside the strip. Wide characters advance two cells.
- **Left click** a tab: select it (`select_tab`). Left click the close
  glyph on the right of a slot closes that tab without selecting it first.
- **Middle click** a tab body (or the close glyph) closes that tab.
  Same neighbor-selection rule as `Ctrl+Shift+Q` when the active
  tab is closed. Middle-click in pane content is unchanged.
- **Right click** a tab with one pane: open its session context menu.
  For a tab with multiple panes, begin inline rename in that strip cell.
  Typing replaces the title; Enter commits via `Domain::rename_window`; Esc or a
  click elsewhere cancels. While editing, keyboard input goes to the field,
  not the pane. The editing cell uses the `focus_border` color **and** a
  filled slot (shape + color; never color alone).
- Keyboard reachability: `Ctrl+Shift+R` opens the same editor on the
  active tab.

### Out of scope

- Drag-reorder of tabs
- Double-click rename (right-click / `Ctrl+Shift+R` is the rename surface)
- tmux-style re-fit of a stale inactive-tab layout on switch (logged
  error until the OS window grows)

## Consequences

- Multi-tab state is visible without scraping child output.
- Single-tab and nested Termwright geometry are unchanged.
- ADR-0010’s “no permanent menu bar” stance holds for the default path.

## Proof

- Host unit tests cover tab chords, strip present iff `tab_count > 1`,
  active marker shape vs color, badge glyphs, `top_chrome_px` resize math,
  strip hit-test, inline rename commit/cancel, editor paint, rail ends at
  `window_padding_px` 0 and 5, dead inter-tab gaps, and badge/close
  centerline equality.
- Nested Termwright is unchanged (it does not drive `prismattyc-host`).
