# Prismattyc brand assets

Mark: "Continuous beam" (9d) — one white beam, split into the seven-hue
spectrum and recombined.

## Files
- prismattyc-icon.svg — master, 512 viewBox, transparent
- prismattyc-icon-mono.svg — single-color version for symbolic menus / stamps
- prismattyc-icon-tile.svg — full-bleed dark squircle (Linux desktop, window, taskbar)
- png/prismattyc-{16,24,32,48,64,128,256,512}.png — transparent renders
  (24px is a downscale of 32; the packet ships hinted 16 and 32)
- png/prismattyc-mono-512.png
- png/prismattyc-tile-{16,24,32,48,64,128,256,512,1024}.png — app tile on
  #121214, 24% corner radius (512 and 1024 from the packet; smaller sizes
  downscaled from 1024)
- macos/prismattyc.svg — same mark as the master (macOS vector)
- macos/prismattyc.icns — Dock/Finder icon, packed from the 1024 tile

## Spectrum
FF6E63 · FFB454 · FFE066 · 7BD88F · 62A8FF · 7B8CFA · 9B8CF5
Neutral beam: D0D0D0 · Surface: 121214

## Usage
- Keep the mark on dark surfaces; the beam needs the dark field.
- Below 24px prefer the supplied PNGs (hinted geometry).
- Mono only where color is unavailable; never recolor individual bands.
- Wordmark: Space Grotesk 600, letter-spacing -0.01em, set "Prismattyc" in EDEEF2.

## Host integration
- **`prismattyc-host`** embeds `png/prismattyc-tile-128.png` as the OS **window
  icon** (winit) at build time.
- **macOS:** vector master is [`macos/prismattyc.svg`](macos/prismattyc.svg).
  Dock tile uses `png/prismattyc-tile-1024.png` at runtime (`winit` ignores
  `window_icon` on Cocoa). Build `.icns` with
  `scripts/install-prismattyc-host-macos.sh` (Darwin `iconutil`); a packed
  `macos/prismattyc.icns` is checked in for Linux checkouts.
- FreeDesktop: `prismattyc-host.desktop` + hicolor **tile** PNG/SVG
  (`prismattyc-icon-tile.svg`, `png/prismattyc-tile-*.png`) via
  `scripts/install-prismattyc-host-desktop.sh` (also `cargo install`s the binary
  unless `SKIP_CARGO_INSTALL=1`).
