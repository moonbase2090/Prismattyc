# Changelog

## [0.3.3] - 2026-10-07

### Since 0.3.0

When an attached session exits successfully, Prismattyc closes its pane,
removes the session from its saved Space, and tears down the live session.
Direct attachments without a saved Space are stopped by stable session ID, and
failed cleanup is retried. Failed attached clients remain available as
reopenable placeholders.

A plain click opens a link. This applies to OSC 8 links and detected
HTTP(S) URLs. The link opens on release only when the press and release stay
on the same link. Drags still select text, double- and triple-clicks still
select a word or line, and programs using mouse reporting still receive
unmodified clicks. `link_click = "modifier"` brings back Ctrl-click on Linux
and Command-click on macOS. The setting is hot-reloaded and appears in
Settings.

Right-click a space, session, or pane row in the sidebar or a vertical space
rail, or a space icon on the collapsed strip, to open a context menu. It
offers the existing open, save, rename, stop, move, and delete actions.
Destructive actions ask for confirmation. Close space… appears only for the
current space. Menu or Shift+F10 opens the menu from a focused rail, and the
unbound `space_rail_context_menu` action does the same. The sidebar and the
collapsed strip no longer show a pane row for a session with one pane. The
session row stands for that pane, and its menu adds Focus, Rename, Close
pane…, Stop…, and Move to space….

The Graphite sidebar marks the space, tab, and pane rows that need you. A
space row shows how many panes are waiting. Clicking a marker focuses the
next pane that needs attention, and opens its space first when that space is
not current. `jump_needs_you` (default `Ctrl+Shift+U`) and a command-palette
entry cycle through those panes. Attention clears when the pane prints more
output without a new attention request, so a badge no longer sticks after an
agent rings once and keeps working. Focusing the pane still clears it.

Pressing Enter on the placeholder for a saved Space session replays that
session's saved command instead of opening a bare login shell. The saved
layout and per-pane working directories come back too.
`pmux session reopen NAME --space SPACE` replays saved commands under the
same `space_open_runs_commands` policy as `pmux space open`. A seat that is
already live in an owned Space is not replayed. `--no-run` skips the
commands, and `--no-claim` reopens the session without changing the Space
file or the session's owner.

pmux mail reaches Muse panes. Doorbells used to wait because pmux did not
recognize the `muse-bin-<version>` foreground process. pmux now matches
`muse` and `muse-*` and submits with one carriage return.

Pixel alpha premultiplication is exact and faster. Each channel keeps
floor(channel × alpha / 255) for every 8-bit value, and fully opaque runs are
skipped. On a 3,528 × 1,764 frame on Apple silicon, the median opaque
conversion dropped from 2.45 ms to 0.67 ms, and alpha 128 from 2.74 ms to
2.27 ms. These times cover the conversion loop, not end-to-end presentation.

`pmux render-status --json` reports the main-thread pump phases and the count
and time of synchronous socket and subprocess waits. With `render_timer` on,
the log and the on-screen display show the pump duration and its slowest
phase, PTY parse time, and on macOS the presentation write and commit times,
tile counts, and bytes. Changed-pixel comparison follows the existing
`render_timer` modes, which default to off.

The nightly mutants in-diff job runs as 16 shards instead of one, so it
finishes inside the step timeout. Tests that could not fail now assert real
behavior. CI installs Noto CJK fonts and checks GPU adapter enumeration on
wgpu's Noop backend.

## [0.3.3-rc.1] - 2026-10-07

This build is a prerelease. A tag such as `v0.3.3-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.3`.

### Since 0.3.0

A plain click opens a link. This applies to OSC 8 links and detected
HTTP(S) URLs. The link opens on release only when the press and release stay
on the same link. Drags still select text, double- and triple-clicks still
select a word or line, and programs using mouse reporting still receive
unmodified clicks. `link_click = "modifier"` brings back Ctrl-click on Linux
and Command-click on macOS. The setting is hot-reloaded and appears in
Settings.

Right-click a space, session, or pane row in the sidebar or a vertical space
rail, or a space icon on the collapsed strip, to open a context menu. It
offers the existing open, save, rename, stop, move, and delete actions.
Destructive actions ask for confirmation. Close space… appears only for the
current space. Menu or Shift+F10 opens the menu from a focused rail, and the
unbound `space_rail_context_menu` action does the same. The sidebar and the
collapsed strip no longer show a pane row for a session with one pane. The
session row stands for that pane, and its menu adds Focus, Rename, Close
pane…, Stop…, and Move to space….

The Graphite sidebar marks the space, tab, and pane rows that need you. A
space row shows how many panes are waiting. Clicking a marker focuses the
next pane that needs attention, and opens its space first when that space is
not current. `jump_needs_you` (default `Ctrl+Shift+U`) and a command-palette
entry cycle through those panes. Attention clears when the pane prints more
output without a new attention request, so a badge no longer sticks after an
agent rings once and keeps working. Focusing the pane still clears it.

Pressing Enter on the placeholder for a saved Space session replays that
session's saved command instead of opening a bare login shell. The saved
layout and per-pane working directories come back too.
`pmux session reopen NAME --space SPACE` replays saved commands under the
same `space_open_runs_commands` policy as `pmux space open`. A seat that is
already live in an owned Space is not replayed. `--no-run` skips the
commands, and `--no-claim` reopens the session without changing the Space
file or the session's owner.

pmux mail reaches Muse panes. Doorbells used to wait because pmux did not
recognize the `muse-bin-<version>` foreground process. pmux now matches
`muse` and `muse-*` and submits with one carriage return.

Pixel alpha premultiplication is exact and faster. Each channel keeps
floor(channel × alpha / 255) for every 8-bit value, and fully opaque runs are
skipped. On a 3,528 × 1,764 frame on Apple silicon, the median opaque
conversion dropped from 2.45 ms to 0.67 ms, and alpha 128 from 2.74 ms to
2.27 ms. These times cover the conversion loop, not end-to-end presentation.

`pmux render-status --json` reports the main-thread pump phases and the count
and time of synchronous socket and subprocess waits. With `render_timer` on,
the log and the on-screen display show the pump duration and its slowest
phase, PTY parse time, and on macOS the presentation write and commit times,
tile counts, and bytes. Changed-pixel comparison follows the existing
`render_timer` modes, which default to off.

The nightly mutants in-diff job runs as 16 shards instead of one, so it
finishes inside the step timeout. Tests that could not fail now assert real
behavior. CI installs Noto CJK fonts and checks GPU adapter enumeration on
wgpu's Noop backend.

## [0.3.0] - 2026-10-05

Graphite is the default chrome. To keep the previous look, add this line to
`~/.config/prismattyc/config.toml`. It applies live, without a restart:

```toml
chrome_style = "classic"
```

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This is a stable release. The tag `v0.3.0` contains no hyphen, so the publish
job does not add `--prerelease` or `--latest=false`. GitHub marks the release
Latest. `pmux update` and the app menu offer it. The binaries report `0.3.0`.

### Since 0.2.30

Unset `chrome_style` selects the Graphite tabs bar, pane title rows, spaces
bar, light-cycle focus ring, bar colors, pane-header handles, arrangement
actions, transparency dialog, and sidebar layout. The command palette, theme
picker, and other overlays use the Graphite surfaces.

Graphite chrome follows the selected theme. The tabs bar, spaces bar, side
rail, sidebar, pane title rows, chips, and overlays take their colors from
that theme. The Prismattyc themes keep their existing chrome. An explicit
`bar_color` still repaints the bars. Prismattyc Dark and Prismattyc Light
paint the chrome and the terminal from one palette. A `prismattyc` theme
follows the system appearance. `prismattyc-dark` and `prismattyc-light` pin
one side. The fresh-install template comments `theme` instead of writing
`theme = "prismattyc"`, so rolling back to 0.2.30 still loads the file.

The Space dropdown in the tabs rail opens the Space picker. Saved spaces can
be dragged into a new order on every rail, including the Graphite sidebar.
Shift+arrow moves the focused space the same way. Reordering stays off until
`space_reorder = true`. That setting defaults to false.

Saving a Space writes each tab's pane arrangement. Opening that Space puts
the splits back, including a grid, an even split, or a free-form ratio.
Spaces also autosave that arrangement when it changes. The write waits for
a short idle, then saves the live tree, including splits and ratios. Autosave
is on unless `[spaces] autosave` or the legacy `space_autosave` key turns it
off.

Graphite settings and the command palette include a Layout choice, Bars or
Sidebar. The choice applies live and is saved in the config. `layout` stays
`bars` by default.

The Graphite sidebar Arrange control shows icons for Single, Split, and Grid.
Hovering an icon shows its name. The selected icon follows the current pane
arrangement.

The Graphite transparency dialog scales its type with the window, so the
labels stay readable on a retina display. The frame stays the 760×460 design
size and scrolls inside.

Home, End, and the arrow keys follow DECCKM. With application cursor mode on,
unmodified keys use SS3. Otherwise they use CSI. On macOS, Option+Left and
Option+Right jump by word, and horizontal focus defaults to Ctrl+Option+Left
and Ctrl+Option+Right when `[keys]` does not set `focus_left` or
`focus_right`. With `macos_shortcuts = true`, Cmd+Left and Cmd+Right move to
the start and end of the line, and Cmd+Backspace sends `^U`. A config
generated by an older build that still has live `focus_left` and
`focus_right` lines keeps those chords until the lines are commented out.

The Jev shadow job labels missed mutants, scores a miss from the selected
test package, and reports a calibrated score. That score does not choose
mutants or tests. The nightly mutants in-diff job passes `--shard 0/1`,
which cargo-mutants requires whenever `--sharding` is set.

### Since 0.3.0-rc.3

The Spaces menu works with the mouse in Graphite and classic, including
while the window is translucent. Hovering a row uses the same highlight as
keyboard selection and the pointing-hand cursor. The search field uses the
text cursor. One click on a row opens that space and closes the menu. A
second click confirms a delete only on the armed row. A click outside the
panel closes the menu. The wheel and trackpad scroll the list inside the
fixed panel. A click in the search field focuses it.

With `layout = "sidebar"`, drag the grip between the sidebar and the panes
to resize it. The width is saved. It is clamped between 200 px and the room
the panes need: at least 320 px stay with the panes, and the stored width
never goes above 2000. Long names ellipsize. Double-click the grip to return
to 256 px. A chevron in the sidebar header, `sidebar_collapse`
(`Ctrl+Alt+Shift+S`), or a drag below 200 px collapses the sidebar to a
52 px icon strip. Expanding restores the last dragged width. The strip shows
an icon for each space and each session, and it scrolls inside that fixed
width. `space_rail = "right"` docks the sidebar on the right. `layout` stays
`bars` by default, so the sidebar stays off until it is selected.

Clicking a session row in the current space selects its tab and focuses that
pane. The sidebar handles the press, so the click does not type into the
pane. A session icon on the collapsed strip focuses that pane the same way.
A session row in another saved space opens that space and focuses the pane
once a live title matches. If zoom is hiding the pane, the click leaves zoom
and brings the saved arrangement back without rewriting layout ratios. A
click on the pane that is already zoomed leaves the zoom in place.

Status toasts can be turned down or off. `toasts` is `all` (the default),
`errors`, or `off`. `all` keeps the previous behavior. `errors` shows only
failures and refused requests. `off` shows no status toasts. Every status
message, shown or hidden, stays in Recent messages. Spaces settings lists
the three levels, and the unbound `recent_messages` action opens that list.
The gate runs before any painting, so Graphite and classic behave the same.

## [0.3.0-rc.3] - 2026-10-05

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.3` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.3`.

### Since 0.3.0-rc.2

Graphite chrome follows the selected theme. The tabs bar, spaces bar, side
rail, sidebar, pane title rows, chips, and overlays take their colors from
that theme. The Prismattyc themes keep their existing chrome. An explicit
`bar_color` still repaints the bars.

Home, End, and the arrow keys follow DECCKM. With application cursor mode on,
unmodified keys use SS3. Otherwise they use CSI. On macOS, Option+Left and
Option+Right jump by word, and horizontal focus defaults to Ctrl+Option+Left
and Ctrl+Option+Right when `[keys]` does not set `focus_left` or
`focus_right`. With `macos_shortcuts = true`, Cmd+Left and Cmd+Right move to
the start and end of the line, and Cmd+Backspace sends `^U`. A config
generated by an older build that still has live `focus_left` and
`focus_right` lines keeps those chords until the lines are commented out.

Spaces autosave their pane arrangement when it changes. The write waits for
a short idle, then saves the live tree, including splits and ratios. Autosave
is on unless `[spaces] autosave` or the legacy `space_autosave` key turns it
off.

The Graphite sidebar Arrange control shows icons for Single, Split, and Grid.
Hovering an icon shows its name. The selected icon follows the current pane
arrangement.

The Jev shadow job labels missed mutants, scores a miss from the selected
test package, and reports a calibrated score. That score does not choose
mutants or tests.

## [0.3.0-rc.2] - 2026-10-05

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.2` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.2`.

### Since 0.3.0-rc.1

The Space dropdown in the tabs rail opens the Space picker.

Saved spaces can be dragged into a new order on every rail, including the
Graphite sidebar. Shift+arrow moves the focused space the same way. The
behavior stays off until `space_reorder = true`. That setting defaults to
false.

Prismattyc Dark and Prismattyc Light paint the chrome and the terminal from
one palette. A new `prismattyc` theme follows the system appearance.
`prismattyc-dark` and `prismattyc-light` pin one side. The fresh-install
template comments `theme` instead of writing `theme = "prismattyc"`, so
rolling back to 0.2.30 still loads the file.

The Graphite transparency dialog scales its type with the window, so the
labels stay readable on a retina display. The frame stays the 760×460 design
size and scrolls inside.

Saving a Space writes each tab's pane arrangement. Opening that Space puts
the splits back, including a grid, an even split, or a free-form ratio.

Graphite settings and the command palette include a Layout choice, Bars or
Sidebar. The choice applies live and is saved in the config.

The nightly mutants in-diff job passes `--shard 0/1`, which cargo-mutants
requires whenever `--sharding` is set. The sharded jobs are unchanged.

## [0.3.0-rc.1] - 2026-10-04

Graphite is the default chrome. To keep the previous look, add this line to
`~/.config/prismattyc/config.toml`. It applies live, without a restart:

```toml
chrome_style = "classic"
```

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, and `layout` commented out, so a later default change reaches
those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.1`.

### Graphite by default

Unset `chrome_style` now selects the Graphite tabs bar, pane title rows,
spaces bar, light-cycle focus ring, bar colors, pane-header handles,
arrangement actions, transparency dialog, and sidebar layout. The command
palette, theme picker, and other overlays use the Graphite surfaces. Screenshots
of that chrome are in `docs/design/`.

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
chrome. `layout = "sidebar"` is Graphite only. It replaces the bars with one
sidebar tree: space rows collapse, live rows select their tab, saved rows open
a Space, and the footer runs the same new-tab, palette, and arrange actions.
The pointer changes on those rows, and the panel scrolls.

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
