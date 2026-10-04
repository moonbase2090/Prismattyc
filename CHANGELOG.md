# Changelog

## [0.2.30] - 2026-10-04

### In-app updates

On macOS, the Prismattyc menu checks for a signed release, shows its version
and notes, and can install and relaunch. Automatic checks run on launch and
daily. Turn them off with `automatic_update_checks = false` or the menu item.
**Roll Back Last Update…** restores the previous verified app. `pmux update`
remains available on every platform.

### Graphite preview

The Graphite UI is available as an opt-in preview with
`chrome_style = "graphite"`. The default remains `classic`. A fresh install
writes `chrome_style = "classic"`. Graphite adds the redesigned tabs bar, pane
title rows, a scrolling spaces column in every rail position, the light-cycle
focus ring, Ctrl+Shift+B bar colors, pane-header drag handles, arrangement
actions, the transparency dialog, and a pointing-hand cursor on clickable
chrome. `layout = "sidebar"` opts into the combined sidebar tree.

### Command palette and dialogs

The command palette takes mouse clicks and keeps its place while the list
scrolls. The theme picker, Space pickers, session prompt, and restore prompt
use a fixed frame and scroll inside it.

### Terminal

Keyboard selection works in alternate-screen applications. A `[keys]` override
that hides a macOS Command shortcut prints a warning. Short-line scrolling,
resize reflow, and narrow-cell writes do less work.

### Jev

Jev shadow requests send AI Gateway authentication and accept a double-wrapped
gateway response (`result.result.answers`). A manual smoke workflow can make
one live call. Pull-request triage is advisory: it may add `triage:*` labels
and one comment, it stays off until `JEV_PR_TRIAGE_ENABLED` is set, and it
does not block a merge.
