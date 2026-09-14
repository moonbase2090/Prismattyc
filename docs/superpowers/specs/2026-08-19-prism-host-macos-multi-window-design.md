# prism-host macOS multi-window and Dock menu — design

Status: draft for review.
Owner: Brandan Majeske.
Date: 2026-08-19.
Scope tag: macOS dogfood (see [macos.md](../../macos.md)). Not a §5.6 gate.
Linux remains the supported claim.

## Goal

Open multiple `Prism.app` OS windows, like Terminal.app. Trigger from:

1. Dock right-click menu item "New Window".
2. Menu bar: File > New Window.
3. Keyboard: ⌘N.

Windows share sessions through the long-lived mux server (ADR-0011). Ship a
local, unsigned `Prism.app` bundle. Defer signing and notarization
(see [macos.md](../../macos.md) "App bundle and notarization").

## Current state (evidence)

- `prism-host` embeds an in-process `prism_mux::Domain` per process.
  `Domain::bootstrap("default")` at `crates/prism-host/src/mux.rs:789`. No
  socket client in the host.
- The host already reaches the daemon by spawning `prism-mux attach
  --session-id <id>` as a **pane child process**: `attach_session_args`
  (`crates/prism-host/src/main.rs:264`), boot command (`main.rs:939`),
  `find_mux_bin` (`main.rs:249`).
- Mux "windows" are **tabs** inside one OS window. There is no OS-level
  multi-window concept.
- `App` holds a single `host: Option<HostState>` (`main.rs:651-667`).
  `resumed` early-returns if a host exists (`main.rs:2895`). `window_event`
  ignores the incoming `WindowId` (`main.rs:2906`).
- Event-loop user-event type is `()` and used only as a wake
  (`main.rs:3391`, wake closure `main.rs:674-683`).
- AppKit interop is one function: `apply_macos_app_icon` in
  `crates/prism-host/src/icon.rs:71-95`. Pattern: `MainThreadMarker::new()` +
  `NSApplication::sharedApplication(mtm)` + `unsafe` setter.
- `objc2-app-kit` features enabled: `NSApplication`, `NSImage`, `NSResponder`,
  `NSRunningApplication`. Missing: `NSMenu`, `NSMenuItem`,
  `NSApplicationDelegate`. `objc2-foundation` missing `NSString`.
- **winit 0.30.13 owns the `NSApplicationDelegate`.** No menu bar is set.
- No `.app`/`Info.plist` committed. `scripts/install-prism-host-macos.sh`
  builds only an `.icns`.

## Architecture

### Sharing model

Each OS window boots attached to the daemon through the existing
`prism-mux attach` pane path. Windows share sessions at the daemon layer.
No new socket client in the host. Reuse `attach_session_args` and the boot
command.

Each OS window keeps its own in-process `MuxRuntime`/`Domain`; that domain
hosts the attach-client pane. Sharing happens in the daemon, not in host
memory. This matches the reused mechanism and needs no cross-window domain
sharing.

### Multi-window core (cross-platform)

Replace the single host with a window map.

- `App.windows: HashMap<winit::window::WindowId, HostState>` replaces
  `host: Option<HostState>`.
- `window_event` routes by the `WindowId` it already receives.
- Add `App::open_window(&mut self, event_loop)` — the current `spawn_host`
  body, generalized to insert into the map instead of a single slot.
- `resumed` opens the first window.
- Window close removes the entry. Empty map exits the process.
- Per-window state stays in `HostState` (window, present backend, mux runtime,
  font, theme, focus). No field is global today, so the split is mechanical.

### Event plumbing

- Change `EventLoop<()>` to `EventLoop<UserAction>`.
- `enum UserAction { Wake, NewWindow }`.
- The wake closure sends `UserAction::Wake` (current behavior).
- `user_event` matches: `Wake` runs `pump`; `NewWindow` calls `open_window`.
- Menu and Dock actions send `UserAction::NewWindow` through
  `EventLoopProxy<UserAction>`.

### macOS triggers

1. **Menu bar + ⌘N.** After launch, set `NSApp.mainMenu` with an App menu and
   a File menu. The File menu holds "New Window" with key equivalent ⌘N.
   Extend the `icon.rs` objc2 pattern on the main thread. Enable features
   `NSMenu`, `NSMenuItem` (objc2-app-kit) and `NSString` (objc2-foundation).
2. **Menu action target.** A small objc2 object holds an
   `EventLoopProxy<UserAction>`. Its selector `newWindow:` sends
   `UserAction::NewWindow`. Menu items target this object.
3. **Dock right-click "New Window".** `applicationDockMenu:` lives on the
   `NSApplicationDelegate`, which winit owns. Plan: add the method to winit's
   delegate class at runtime with objc2 `class_addMethod`, returning our
   `NSMenu`. **Risk area — proven by a phase-1 spike (see Phasing).**

### Local .app bundle

Extend `scripts/install-prism-host-macos.sh` (or add a sibling) to assemble
`Prism.app`:

- `Contents/Info.plist` — `CFBundleName=Prism`,
  `CFBundleExecutable=prism-host`, `CFBundleIconFile=prism`,
  `CFBundleIdentifier`, `LSMinimumSystemVersion`.
- `Contents/MacOS/prism-host` — the release binary.
- `Contents/Resources/prism.icns` — the existing `.icns` output.

The bundle gives the correct Dock name and menu-bar title. Unsigned; runs
locally after one right-click-Open or `xattr -dr com.apple.quarantine`.

## Phasing

1. **Multi-window core + ⌘N via winit keyboard.** Cross-platform. Window map,
   `UserAction`, open/close, last-close exit. ⌘N handled in `window_event`
   as an interim trigger that works on Linux too. **Include the Dock-menu
   swizzle spike here** — a throwaway `applicationDockMenu:` via
   `class_addMethod` that returns a one-item menu and logs on invoke. Confirm
   it survives winit 0.30.13 launch. If the spike fails, stop and revisit
   before phase 3.
2. **macOS menu bar + ⌘N menu item + `.app` bundle.** `NSApp.mainMenu`, the
   action-target object, and the bundle script.
3. **macOS Dock menu.** Promote the phase-1 spike to the real
   `applicationDockMenu:` returning the "New Window" item wired to
   `UserAction::NewWindow`.

## Testing

- Multi-window state is cross-platform. Unit-test open, close, `WindowId`
  routing, and last-close exit on Linux CI.
- `UserAction` handling: test that a `NewWindow` event grows the window map.
- Menu construction (macOS): assert item titles and key equivalents.
- AppKit runtime stays GUI-gated per ADR-0006 D-W5 (`#[ignore]` or
  display-gated).
- Linux CI compiles the crate. macOS-only code stays behind
  `cfg(target_os = "macos")`.

## Risks and open items

- **Dock-menu swizzle stability.** Adding `applicationDockMenu:` to winit's
  delegate class depends on winit internals. Phase-1 spike gates the design.
  Fallback: menu bar + ⌘N only, Dock item as a later item.
- **Focus and input routing across windows.** `window_event` already carries
  `WindowId`; confirm modifier and paste state is per-window, not global.
- **Daemon session selection per window.** Decide whether a new window opens a
  new daemon session or attaches to an existing one. Default: new session,
  matching a fresh Terminal window.

## Out of scope

- Signing, notarization, Sparkle, distribution.
- Windows and Linux multi-window Dock/menu-bar equivalents (⌘N-style keyboard
  trigger still works cross-platform from phase 1).
- Claiming `prismattyc-classic/*` on Darwin.
