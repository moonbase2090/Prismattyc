# Prismattyc 0.3.0-rc.1 release notes

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

## Graphite by default

Unset `chrome_style` now selects the Graphite tabs bar, pane title rows,
spaces bar, light-cycle focus ring, bar colors, pane-header handles,
arrangement actions, transparency dialog, and sidebar layout. The command
palette, theme picker, and other overlays use the Graphite surfaces. Screenshots
of that chrome are in `docs/design/`.
