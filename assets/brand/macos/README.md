# macOS host icon

Master vector: [`prismattyc.svg`](prismattyc.svg) (same mark as
[`../prismattyc-icon.svg`](../prismattyc-icon.svg)).

macOS does not use FreeDesktop `hicolor` paths. Dock and Finder read
`prismattyc.icns` inside `Prismattyc.app`.

- `prismattyc-host` applies `../png/prismattyc-tile-1024.png` as the Dock
  tile at runtime (`winit` ignores `window_icon` on Cocoa).
- `scripts/install-prismattyc-host-macos.sh` builds `prismattyc.icns` from that
  tile (`sips` downscales, `iconutil` packs; Darwin only). A packed
  `prismattyc.icns` is checked in so Linux checkouts still assemble the
  bundle resources.

Do not edit this SVG independently of `prismattyc-icon.svg`.

Linux desktop / window icons use a full-bleed squircle
(`../prismattyc-icon-tile.svg`).
