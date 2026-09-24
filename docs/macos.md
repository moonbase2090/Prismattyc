# macOS port

Linux remains the supported claim ([fidelity-matrix](fidelity-matrix-v1.md)).
This page is the inventory for a compile-and-dogfood macOS path. It is **not**
a product-support claim and not a §5.6 gate.

## Child terminfo

Spawn materializes two compiled layouts under `TERMINFO`:

| Layout | Format | Who reads it |
|--------|--------|----------------|
| `p/prismattyc-kitty` (aliases `prismattyc-direct`, `prism-kitty`, `prism-direct`) | 32-bit extended (`0x021E`) with `fullkbd`, `Tc`, `setrgbf`/`setrgbb`, paste, focus | Homebrew ncurses, Linux |
| `70/prismattyc-kitty` | 16-bit legacy (`0x011A`), no user caps | Apple `/usr/lib` ncurses 6.0.x (`tmux`, `vim`) |

Do not point `PRISMATTYC_TERMINFO` at the repo `terminfo/` tree on macOS system
curses: that tree has `p/` only. The XDG materialized dir (`~/.local/share/prism/terminfo`)
has both. After a terminfo change, restart `prismattyc-host` so children respawn.

## What already works (unix, not Linux-specific)

- PTY spawn via `portable-pty`
- Unix-domain control socket + `chmod 0600`
- rustix termios / poll / signals
- winit window with alpha-capable Core Animation presentation (Cocoa)
- Nested `prismattyc` VT host
- Mux `stop`/`up` argv match via `procinfo` (`KERN_PROCARGS2` on Darwin). Inode proof stays Linux-only; 0600 socket remains the gate
- Mux attach scan (`ls`/`doctor`/`kick`) via `procinfo::pids` + `cmdline`
- InjectMail agent detect via `procinfo::cmdline` / `children_of`
- Host split cwd via `procinfo::cwd_of` (`proc_pidinfo` on Darwin). OSC 7 still preferred
- Host child-exit wait via `kill(pid, 0)` on non-Linux
- Host clipboard via default `arboard` (Cocoa pasteboard)

## Window transparency and blur

The default macOS presenter keeps per-pixel alpha. `window_opacity` controls
the window ground and default cell backgrounds. Text and explicit cell
backgrounds remain opaque. `chrome_opacity` controls the tab strip and footer.
It defaults to `window_opacity`.

Set `window_opacity` below `1.0` to see the desktop through the window.
Set `window_blur = true` to blur that backdrop with AppKit. An opaque ground
covers the blur. These settings apply through config hot reload, including
when the window starts fully opaque. They do not use `NSWindow.alphaValue`
to fade text or window decorations.

```toml
window_opacity = 0.75
chrome_opacity = 0.9
window_blur = true
```

The optional GPU presenter does not carry per-pixel alpha. Use the default
presenter for these settings.

### Check transparency changes

Run these checks in a logged-in macOS desktop session.

1. Build the host and mux binaries.

   ```bash
   cargo build --locked -p prismattyc-host -p prismattyc-mux
   ```

2. Check tile positions, retained images, window opacity, blur, and resize.

   ```bash
   cargo run -p prismattyc-host --example macos_present_probe --locked
   ```

   The probe maps each tile back into view coordinates. It checks that a
   partial update replaces only the damaged tile image. To check that the
   probe detects the old coordinate error, run this negative control. It
   must fail a tile position assertion.

   ```bash
   PRISMATTYC_PROBE_OLD_TILE_FRAMES=1 cargo run -p prismattyc-host --example macos_present_probe --locked
   ```

3. Check config hot reload and terminal colors in a private session.
   Use a new output directory for each run.

   ```bash
   python3 tests/native/macos-alpha-e2e.py --bins target/debug --out build/macos-alpha-check
   ```

The private-session fixture saves CPU framebuffer captures. It checks that
text and explicit backgrounds retain their colors as opacity changes.
These captures do not show the composited desktop blur. Check the backdrop
in the native window, or use a desktop capture with Screen Recording access.
Also check a cold start with a saved Space in a large window. The complete
"Restore last space?" dialog must appear in order. Check ordinary terminal
output and resize for displaced bands or clipped rows.

## Linux-only today

| Area | Mechanism | macOS plan |
|------|-----------|------------|
| Mux socket inode proof | `/proc/net/unix` + `/proc/<pid>/fd` | skipped; 0600 socket remains the gate |
| Control peer uid | `SO_PEERCRED` | already skipped; 0600 + private runtime dir |
| Desktop icon | `install-prismattyc-host-desktop.sh` | Linux: full-bleed dark squircle [`assets/brand/prismattyc-icon-tile.svg`](../assets/brand/prismattyc-icon-tile.svg). macOS: SVG master [`assets/brand/macos/prismattyc.svg`](../assets/brand/macos/prismattyc.svg); Dock tile from `png/prismattyc-tile-1024.png` at runtime; `scripts/install-prismattyc-host-macos.sh` builds `.icns` for `Prismattyc.app` |
| Hive supervisor environ | hived reads `/proc/pid/environ` | Hive-side; not a Prismattyc blocker |

## Out of this first slice

- GitHub Actions `macos-latest` job (billing + Apple SDK). Release signing
  runs on a Mac through `scripts/release/package-macos.sh`.
- Intel and universal binaries. The release zip is Apple silicon only.
- Sparkle
- Windows
- Claiming `prismattyc-classic/*` on Darwin

## Keybindings that macOS steals

macOS binds **Control-F2…F8** to “move focus to menu bar / Dock / window / …”.
Those events often never reach `prismattyc-host`, so **Ctrl+Shift+F2…F8** even-layout
chords look dead.

Use one of these instead (same layouts as Linux Ctrl+Shift+Fn):

| Layout | Linux / Windows | macOS |
|--------|-----------------|-------|
| 2 columns | Ctrl+Shift+F2 | **Cmd+Shift+F2** or **Ctrl+Alt+2** |
| 3 columns | Ctrl+Shift+F3 | **Cmd+Shift+F3** or **Ctrl+Alt+3** |
| 2×2 quadrants | Ctrl+Shift+F4 | **Cmd+Shift+F4** or **Ctrl+Alt+4** |
| n columns | Ctrl+Shift+F5…F9 | **Cmd+Shift+F5…F9** or **Ctrl+Alt+5…9** |

**Ctrl+Shift+1…9** still select tabs. Do not reuse those digits for layout.

To keep the Linux chords, turn off the system shortcuts in
**System Settings → Keyboard → Keyboard Shortcuts → Keyboard**.

## App bundle and notarization

A local ad-hoc `Prismattyc.app` is the macOS dogfood host. `prismattyc update --host`
runs [`scripts/install-prismattyc-host-macos.sh`](../scripts/install-prismattyc-host-macos.sh),
which writes `~/Applications/Prismattyc.app`. The bundle contains
`prismattyc-host`, `pmux`, `pmuxd`, and `pmux-attach` under `Contents/MacOS`,
so Finder and Dock launches do not depend on the shell `PATH`. The installer
builds the mux helpers from the same checkout as the app release. It refreshes
all four executables in other existing copies (`/Applications`,
`target/Prismattyc.app`, Spotlight). `cargo install` updates the separate
executables under `~/.cargo/bin` only.
Quit Prismattyc.app (Cmd+Q) and reopen it from the Dock after an update, or the
old image stays in memory.

Local dogfood builds stay ad-hoc signed. A GitHub release uses
[`scripts/release/package-macos.sh`](../scripts/release/package-macos.sh),
documented in [Publish release artifacts](release-process.md). That script
produces `Prismattyc-vX.Y.Z-macos-arm64.zip` for Apple silicon only.

### Signing vs. notarization

- **Code signing** proves author identity. It uses a Developer ID Application
  certificate from the Apple Developer Program.
- **Notarization** is Apple's automated malware scan of a signed app. Apple
  returns a ticket. Gatekeeper trusts that ticket.

The two are separate steps. Signing alone still triggers a Gatekeeper warning
on other Macs. Notarization removes that warning.

### Local dogfood (no membership, no notarization)

An unsigned or ad-hoc-signed `.app` runs on the build machine. On another Mac
it needs one manual approval:

```bash
# ad-hoc sign (no certificate)
codesign --force --deep --sign - Prismattyc.app
# clear the quarantine flag on a downloaded copy
xattr -dr com.apple.quarantine Prismattyc.app
```

Right-click > Open also bypasses the first-launch block once.

### Release signing and notarization

The release app is Apple silicon only. There is no universal or Intel
`lipo` step.

Prerequisites:

1. Apple Developer Program membership.
2. A **Developer ID Application** certificate in the login keychain.
   The packager's default identity is
   `Developer ID Application: Moonbase 2090 LLC (S24C53PD3Y)`.
   Override it with `PRISMATTYC_CODESIGN_IDENTITY`.
3. A `notarytool` keychain profile. The default name is `moonbase-notary`.
   Override it with `PRISMATTYC_NOTARY_KEYCHAIN_PROFILE`.
   Store the profile once. `notarytool` prompts for the app-specific
   password and keeps it in the keychain:

   ```bash
   xcrun notarytool store-credentials moonbase-notary \
     --apple-id "$APPLE_ID" \
     --team-id "$APPLE_TEAM_ID"
   ```

   Do not put the password or an API key in the repo, the environment, or
   the `notarytool submit` command.

Package the same version you package for Linux, then upload the zip to the
draft before that release is published:

```bash
scripts/release/package-macos.sh --version 0.2.8 \
  --out build/release-macos-arm64
```

The script does the following:

1. Assemble `Prismattyc.app` with `prismattyc-host`, `pmux`, `pmuxd`, and
   `pmux-attach` under `Contents/MacOS`. It builds those binaries from this
   checkout unless `--bin-dir` points at an existing arm64 release build.
2. Require each binary to be arm64 and to report the release version.
3. Sign each nested Mach-O, then the `.app`, with the hardened runtime and
   a secure timestamp. It does not use `codesign --deep`.
4. Zip with `ditto`, remove AppleDouble `._*` and `__MACOSX` members, and
   check that the extracted app still verifies.
5. Submit that zip with `xcrun notarytool submit --keychain-profile … --wait`.
   The first team notarization can take hours. The default wait is 6 hours
   (`PRISMATTYC_NOTARY_TIMEOUT`).
6. Staple the ticket onto the app, check it with `stapler validate` and
   `spctl --assess --type execute`, then zip the stapled app as
   `Prismattyc-v0.2.8-macos-arm64.zip`.

The shipped zip is the stapled app, not the archive that was submitted.
`SHA256SUMS` for the draft must include that zip. See
[Publish release artifacts](release-process.md).

Entitlements: the hardened runtime does not block `fork`/`exec` or PTY use, so
the packager does not pass an entitlements file. Add entitlements only
for JIT or unsigned executable memory, which Prismattyc does not use.

## Validate a source build

On a Mac with Rust 1.90 or newer, run:

```bash
cargo test --workspace --locked -- --test-threads=1
cargo run -p prismattyc-host --locked
```

Use the native checks in [the native test guide](../tests/native/README.md) to verify
window rendering and restart behavior.
