# Prismattyc 0.3.0-rc.2 release notes

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.2` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.2`.

## Since 0.3.0-rc.1

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
