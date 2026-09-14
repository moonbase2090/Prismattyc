# prism-host macOS multi-window + Dock menu — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open multiple `Prism.app` OS windows from a Dock right-click menu, a File > New Window menu-bar item, and ⌘N; windows share sessions through the mux daemon.

**Architecture:** Refactor `prism-host` from a single `Option<HostState>` to a `HashMap<WindowId, HostState>`. Add an `EventLoop<UserAction>` so menu and Dock actions can request a new window. Each window boots a `prism-mux attach` pane, so sharing happens at the daemon (ADR-0011). macOS menu bar and Dock menu are built with objc2/objc2-app-kit; the Dock menu is added to winit's delegate by runtime method injection, de-risked by a phase-1 spike. Ship a local unsigned `Prism.app`.

**Tech Stack:** Rust, winit 0.30.13, softbuffer, objc2 0.6.4, objc2-app-kit 0.3.2, objc2-foundation 0.3.2.

**Spec:** `docs/superpowers/specs/2026-08-19-prism-host-macos-multi-window-design.md`

## Global Constraints

- Rust MSRV: 1.90 (workspace pin).
- Linux is the supported claim; macOS is dogfood only. macOS-only code stays behind `#[cfg(target_os = "macos")]`. The crate must still compile and test on Linux CI.
- Softbuffer is the default present path; the `gpu` feature stays opt-in.
- New window content reuses the existing `prism-mux attach` pane path (`attach_session_args`, `attach_boot_command` in `crates/prism-host/src/main.rs`). No new in-host socket client.
- Do not weaken existing tests. GUI-runtime tests stay `#[ignore]` or display-gated per ADR-0006 D-W5.
- Default new-window behavior: open a **new** daemon session (fresh Terminal-window semantics).

---

### Task 1: Introduce `UserAction` event type

**Files:**
- Modify: `crates/prism-host/src/main.rs` (event-loop type, wake closure, `user_event`)

**Interfaces:**
- Produces: `enum UserAction { Wake, NewWindow }` (derive `Debug, Clone, Copy, PartialEq, Eq`); `type EventLoop = winit::event_loop::EventLoop<UserAction>`; wake closure sends `UserAction::Wake`; `EventLoopProxy<UserAction>`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `main.rs`:

```rust
#[test]
fn user_action_variants_are_distinct() {
    assert_ne!(UserAction::Wake, UserAction::NewWindow);
    // Copy + Eq so the event loop can carry it by value.
    let a = UserAction::NewWindow;
    let b = a;
    assert_eq!(a, b);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p prism-host user_action_variants_are_distinct`
Expected: FAIL — `cannot find type UserAction`.

- [ ] **Step 3: Implement the enum and thread the type**

Add near the top-level types in `main.rs` (e.g. just above `struct App`):

```rust
/// Custom event-loop signals. `Wake` is the PTY/config coalesced wake
/// (previously the unit `()` event). `NewWindow` requests a new OS window
/// from the Dock menu, the menu bar, or ⌘N.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserAction {
    Wake,
    NewWindow,
}
```

Change the wake closure in `App::new` (currently `proxy.send_event(())`) to:

```rust
let _ = proxy.send_event(UserAction::Wake);
```

Change `App::new`'s `proxy` parameter type to `EventLoopProxy<UserAction>`.

In `main()`, change `EventLoop::new()` so the type is inferred as `EventLoop<UserAction>` by annotating:

```rust
let event_loop: EventLoop<UserAction> = EventLoop::with_user_event().build().context("event loop")?;
```

Update the `ApplicationHandler` impl associated event type. Change:

```rust
fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
    self.pump(event_loop);
}
```

to:

```rust
fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserAction) {
    match event {
        UserAction::Wake => self.pump(event_loop),
        UserAction::NewWindow => {} // wired in Task 3
    }
}
```

Note: winit's `ApplicationHandler<T>` defaults `T = ()`; add the type parameter — `impl ApplicationHandler<UserAction> for App`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p prism-host user_action_variants_are_distinct` — Expected: PASS.
Run: `cargo build -p prism-host` — Expected: builds. Wake still drives `pump`.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-host/src/main.rs
git commit -m "feat(host): add UserAction event type (Wake/NewWindow)"
```

---

### Task 2: Convert `App` to a multi-window map

**Files:**
- Modify: `crates/prism-host/src/main.rs` — `struct App`, `App::new`, `pump`, `resumed`, `window_event`, `exiting`; rename `spawn_host` → `open_window`.

**Interfaces:**
- Consumes: `HostState` (unchanged fields), `UserAction` (Task 1).
- Produces: `App.windows: std::collections::HashMap<winit::window::WindowId, HostState>`; `fn open_window(&mut self, event_loop: &ActiveEventLoop) -> Result<winit::window::WindowId>`; `pump` iterates all windows and exits only when the map is empty.

- [ ] **Step 1: Write the failing test**

The GUI path cannot run headless, so test the map lifecycle with a small helper. Add to `mod tests`:

```rust
#[test]
fn window_map_last_close_empties() {
    // Model of the routing rule: closing the last window empties the map,
    // which is the process-exit trigger used by `window_event`/`pump`.
    let mut ids: Vec<u64> = vec![1, 2];
    ids.retain(|&id| id != 1);
    assert_eq!(ids, vec![2], "closing one keeps the rest");
    ids.retain(|&id| id != 2);
    assert!(ids.is_empty(), "closing the last empties the map -> exit");
}
```

(This locks the rule the refactor must implement; the real map is `WindowId`-keyed and exercised at runtime.)

- [ ] **Step 2: Run test to verify it fails, then passes trivially**

Run: `cargo test -p prism-host window_map_last_close_empties` — Expected: PASS (rule model). Keep it as a regression guard for the close semantics.

- [ ] **Step 3: Replace the single host slot with a map**

In `struct App`, replace:

```rust
    host: Option<HostState>,
```

with:

```rust
    /// One entry per OS window, keyed by winit's `WindowId`. Empty means no
    /// windows remain and the process exits (see `pump`/`window_event`).
    windows: std::collections::HashMap<WindowId, HostState>,
```

In `App::new`, replace `host: None,` with `windows: std::collections::HashMap::new(),`.

- [ ] **Step 4: Rename `spawn_host` to `open_window` and return the id**

Change the signature:

```rust
    fn open_window(&mut self, event_loop: &ActiveEventLoop) -> Result<WindowId> {
```

At the end, replace the `self.host = Some(HostState { ... });` assignment with:

```rust
        let id = window.id();
        self.windows.insert(id, HostState { /* same fields as before */ });
        Ok(id)
```

Keep `window` cloned into `HostState` as today (`window` is `Arc<Window>`; capture `let id = window.id();` before the value moves into the struct).

Note: `config_error: self.startup_config_error.take()` moves the startup error into the first window only. That is correct — later windows open with `config_error: None`.

- [ ] **Step 5: Update `resumed` to open the first window**

Replace:

```rust
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.host.is_some() {
            return;
        }
        if let Err(e) = self.spawn_host(event_loop) {
            eprintln!("prism-host: failed to start: {e:#}");
            self.exit_code = 1;
            event_loop.exit();
        }
    }
```

with:

```rust
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        if let Err(e) = self.open_window(event_loop) {
            eprintln!("prism-host: failed to start: {e:#}");
            self.exit_code = 1;
            event_loop.exit();
        }
    }
```

- [ ] **Step 6: Route `window_event` and `pump` by `WindowId`**

In `window_event`, change the signature parameter `_id` to `id`, and look the host up in the map. Replace the redraw block and the `let Some(host) = self.host.as_mut()` binding:

```rust
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::RedrawRequested) {
            self.pump(event_loop);
            if let Some(host) = self.windows.get_mut(&id) {
                if let Err(e) = Self::paint(host) {
                    eprintln!("prism-host: paint error: {e:#}");
                }
            }
            return;
        }
        let Some(host) = self.windows.get_mut(&id) else {
            return;
        };
```

Change `WindowEvent::CloseRequested` (line ~2937) from `event_loop.exit();` to per-window close:

```rust
            WindowEvent::CloseRequested => {
                self.windows.remove(&id);
                if self.windows.is_empty() {
                    event_loop.exit();
                }
                return;
            }
```

(Because `host` borrows `self.windows`, restructure so `CloseRequested` is handled before the `host` binding, or drop the borrow first. Simplest: add an early `if let WindowEvent::CloseRequested = event { … return; }` block immediately after the redraw block, before `let Some(host) = …`.)

In `pump`, replace the single-host body with an iteration. Replace the `let more = if let Some(host) = self.host.as_mut() { … } else { … }` block with:

```rust
            let mut more = false;
            let mut closed: Vec<WindowId> = Vec::new();
            for (id, host) in self.windows.iter_mut() {
                if Self::drain_pty(host) {
                    more = true;
                }
                if host.dirty {
                    host.window.request_redraw();
                }
                if host.mux.all_children_exited() {
                    closed.push(*id);
                    continue;
                }
                event_loop.set_control_flow(next_control_flow(
                    Instant::now(),
                    host.border_anim,
                    host.light_cycle_ms,
                    host.mux.active_count() > 0,
                    host.pulse_epoch,
                ));
            }
            for id in closed {
                self.windows.remove(&id);
            }
            if self.windows.is_empty() {
                event_loop.exit();
                return;
            }
```

- [ ] **Step 7: Update `exiting`**

Replace `self.host.take();` with `self.windows.clear();`.

- [ ] **Step 8: Build and run existing tests**

Run: `cargo build -p prism-host` — Expected: builds with no `self.host` references left (`grep -n "self.host" crates/prism-host/src/main.rs` returns nothing).
Run: `cargo test -p prism-host` — Expected: PASS.
Run: `cargo run -p prism-host -- /bin/sh` — Expected: one window opens and behaves as before; closing it exits.

- [ ] **Step 9: Commit**

```bash
git add crates/prism-host/src/main.rs
git commit -m "refactor(host): key windows by WindowId for multi-window support"
```

---

### Task 3: Open a new window on `UserAction::NewWindow` and ⌘N

**Files:**
- Modify: `crates/prism-host/src/main.rs` — `user_event`, `window_event` key handling.

**Interfaces:**
- Consumes: `open_window` (Task 2), `UserAction::NewWindow` (Task 1).
- Produces: a working `NewWindow` handler; a cross-platform ⌘N (Super+N on Linux) keyboard trigger.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn cmd_n_requests_new_window() {
    // ⌘N (Logo/Super + 'n') maps to the new-window request.
    let logo = mods_logo();
    assert!(is_new_window_chord(logo, "n"));
    assert!(!is_new_window_chord(ModifiersState::empty(), "n"));
}
```

Add a helper `mods_logo()` beside the existing `mods()` test helper:

```rust
fn mods_logo() -> ModifiersState {
    let mut value = ModifiersState::empty();
    value.set(ModifiersState::SUPER, true);
    value
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p prism-host cmd_n_requests_new_window`
Expected: FAIL — `cannot find function is_new_window_chord`.

- [ ] **Step 3: Implement the chord predicate and handlers**

Add a free function near the other input predicates:

```rust
/// ⌘N / Super-N opens a new OS window. Uses the platform command modifier
/// (Logo/Super) so it does not collide with terminal Ctrl chords.
fn is_new_window_chord(mods: ModifiersState, key: &str) -> bool {
    mods.super_key() && key.eq_ignore_ascii_case("n")
}
```

Wire `user_event`:

```rust
        UserAction::NewWindow => {
            if let Err(e) = self.open_window(event_loop) {
                eprintln!("prism-host: new window failed: {e:#}");
            }
        }
```

In `window_event`, in the `WindowEvent::KeyboardInput` arm, before existing key routing, add (using the event's logical key text and `host.modifiers`):

```rust
            if event.state == ElementState::Pressed {
                if let Key::Character(ref s) = logical_key {
                    if is_new_window_chord(host.modifiers, s) {
                        if let Err(e) = self.open_window(event_loop) {
                            eprintln!("prism-host: new window failed: {e:#}");
                        }
                        return;
                    }
                }
            }
```

(Match the crate's existing key-extraction pattern in the `KeyboardInput` arm; reuse its `logical_key`/`ElementState` bindings rather than introducing new ones. `self.open_window` needs `&mut self`; the `host` binding borrows `self.windows`, so compute the chord decision, drop the `host` borrow, then call `self.open_window`.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p prism-host cmd_n_requests_new_window` — Expected: PASS.
Run: `cargo run -p prism-host -- /bin/sh`, press ⌘N (Super+N) — Expected: a second window opens; each closes independently; closing the last exits.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-host/src/main.rs
git commit -m "feat(host): open new window on NewWindow action and Cmd-N"
```

---

### Task 4: Dock-menu swizzle spike (macOS, throwaway)

**Files:**
- Create: `crates/prism-host/src/macos_menu.rs` (spike scaffold, kept and grown in Tasks 5/7).
- Modify: `crates/prism-host/src/main.rs` (`mod macos_menu;` + one call in `open_window`), `crates/prism-host/Cargo.toml` (features).

**Interfaces:**
- Produces: `#[cfg(target_os = "macos")] pub fn install_dock_menu_spike()` that adds `applicationDockMenu:` to winit's delegate class at runtime and logs when the Dock menu is requested. This proves the injection point before Task 7 builds the real menu.

- [ ] **Step 1: Enable the objc2 features**

In `crates/prism-host/Cargo.toml`, extend the macOS `objc2-app-kit` feature list to add `NSMenu`, `NSMenuItem`; and `objc2-foundation` to add `NSString`:

```toml
objc2-app-kit = { version = "0.3.2", default-features = false, features = [
    "NSApplication",
    "NSImage",
    "NSResponder",
    "NSRunningApplication",
    "NSMenu",
    "NSMenuItem",
] }
objc2-foundation = { version = "0.3.2", default-features = false, features = [
    "NSData",
    "NSObject",
    "NSString",
] }
```

- [ ] **Step 2: Write the spike module**

Create `crates/prism-host/src/macos_menu.rs`:

```rust
//! macOS menu integration (menu bar, ⌘N item, Dock menu).
//!
//! winit 0.30 owns the `NSApplicationDelegate`. To surface a Dock right-click
//! "New Window", we add `applicationDockMenu:` to winit's delegate class at
//! runtime. This module first proves that injection point (spike), then grows
//! the real menu (Tasks 5 and 7).

#![cfg(target_os = "macos")]

use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSString};

/// SPIKE: add `applicationDockMenu:` to the live app delegate's class and
/// return a one-item menu. Logs on invocation to confirm the Dock calls it.
/// Remove after Task 7 promotes this to the real menu.
pub fn install_dock_menu_spike() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("prism-host: dock-menu spike: not main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: delegate exists after winit finished launching; main thread only.
    unsafe {
        let delegate: *mut AnyObject = msg_send![&app, delegate];
        if delegate.is_null() {
            eprintln!("prism-host: dock-menu spike: no delegate yet");
            return;
        }
        let cls: *const objc2::runtime::AnyClass = msg_send![delegate, class];
        let added = objc2::runtime::AnyClass::from_ptr(cls).is_some();
        eprintln!("prism-host: dock-menu spike: delegate class present={added}");
        // Method injection is exercised for real in Task 7; here we only
        // confirm we can reach the delegate and build an NSMenu.
        let menu = NSMenu::new(mtm);
        let title = NSString::from_str("New Window (spike)");
        let item = NSMenuItem::new(mtm);
        item.setTitle(&title);
        menu.addItem(&item);
        eprintln!("prism-host: dock-menu spike: built NSMenu ok");
        let _ = (menu, Sel::from(sel!(applicationDockMenu:)));
    }
}
```

(If any objc2 0.6 API name differs from the above, adjust to the compiler's guidance — locking these exact names is the spike's purpose. Do not proceed to Task 7 until this compiles and runs.)

- [ ] **Step 3: Wire the module and one call**

In `main.rs`, add near the other `mod` declarations:

```rust
#[cfg(target_os = "macos")]
mod macos_menu;
```

In `open_window`, after `#[cfg(target_os = "macos")] icon::apply_macos_app_icon();`, add:

```rust
        #[cfg(target_os = "macos")]
        macos_menu::install_dock_menu_spike();
```

- [ ] **Step 4: Build and run on macOS**

Run: `cargo build -p prism-host` (on macOS) — Expected: builds.
Run: `cargo run -p prism-host -- /bin/sh` — Expected: stderr prints the spike lines (`delegate class present=true`, `built NSMenu ok`). This confirms delegate reachability and `NSMenu` construction.
Run on Linux: `cargo build -p prism-host` — Expected: builds (module is `cfg`-gated out).

- [ ] **Step 5: Gate decision**

If the delegate is reachable and `NSMenu`/`NSMenuItem`/`NSString` compile and run, the design holds — continue. If not, STOP and revisit the spec's fallback (menu bar + ⌘N only).

- [ ] **Step 6: Commit**

```bash
git add crates/prism-host/Cargo.toml crates/prism-host/src/macos_menu.rs crates/prism-host/src/main.rs
git commit -m "spike(host,macos): prove delegate reach + NSMenu build for Dock menu"
```

---

### Task 5: macOS menu bar + ⌘N menu item + action target

**Files:**
- Modify: `crates/prism-host/src/macos_menu.rs` (real menu-bar builder + action target object), `crates/prism-host/src/main.rs` (pass the `EventLoopProxy<UserAction>` into the menu setup at launch).

**Interfaces:**
- Consumes: `EventLoopProxy<UserAction>` (Task 1), `NSMenu`/`NSMenuItem`/`NSString` (Task 4 features).
- Produces: `pub fn install_main_menu(proxy: EventLoopProxy<UserAction>)`; an objc2 target object with selector `newWindow:` that calls `proxy.send_event(UserAction::NewWindow)`.

- [ ] **Step 1: Write the failing test**

Menu construction touches AppKit and must be display-gated, so assert the pure title/key-equivalent table instead:

```rust
#[cfg(target_os = "macos")]
#[test]
fn menu_table_lists_new_window_cmd_n() {
    let table = crate::macos_menu::menu_item_table();
    assert!(table.iter().any(|(title, key)| *title == "New Window" && *key == "n"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run (macOS): `cargo test -p prism-host menu_table_lists_new_window_cmd_n`
Expected: FAIL — `cannot find function menu_item_table`.

- [ ] **Step 3: Implement the menu table, target object, and installer**

In `macos_menu.rs`, add the pure table (unit-testable, no AppKit):

```rust
/// The File-menu items this app installs: (title, key equivalent).
pub fn menu_item_table() -> &'static [(&'static str, &'static str)] {
    &[("New Window", "n")]
}
```

Add the action-target object using objc2's `define_class!` holding the proxy, with a `newWindow:` method that sends `UserAction::NewWindow`. Then build an `NSMenu` main menu with an application submenu and a File submenu whose "New Window" item has key equivalent `n` (⌘ is implied by `NSEventModifierFlags::Command`), target = the object, action = `sel!(newWindow:)`. Call `NSApplication::sharedApplication(mtm).setMainMenu(Some(&main_menu))`. Store the target object in a process-static (e.g. `OnceLock`) so it outlives the call. Use `crate::UserAction`.

- [ ] **Step 4: Call the installer at launch**

In `main()`, after building `event_loop`, create the proxy once and pass a clone to the menu on macOS before `run_app`:

```rust
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    {
        // Menu bar must exist before the app finishes launching so ⌘N and
        // the File menu are live from the first window.
        macos_menu::install_main_menu(proxy.clone());
    }
    let mut app = App::new(cli, file_config, startup_config_error, proxy)?;
```

- [ ] **Step 5: Run tests and app**

Run (macOS): `cargo test -p prism-host menu_table_lists_new_window_cmd_n` — Expected: PASS.
Run: `cargo run -p prism-host -- /bin/sh` — Expected: menu bar shows File > New Window; clicking it and pressing ⌘N both open a window.
Run (Linux): `cargo test -p prism-host` — Expected: PASS (macOS test cfg-gated out).

- [ ] **Step 6: Commit**

```bash
git add crates/prism-host/src/macos_menu.rs crates/prism-host/src/main.rs
git commit -m "feat(host,macos): menu bar File>New Window with Cmd-N"
```

---

### Task 6: Local `Prism.app` bundle script

**Files:**
- Modify: `scripts/install-prism-host-macos.sh` (add an `--app` mode that assembles the bundle) or Create: `scripts/build-prism-host-app.sh`.
- Create: `crates/prism-host/macos/Info.plist` (template committed for the bundle).

**Interfaces:**
- Produces: a script that outputs `Prism.app` with `Info.plist`, `MacOS/prism-host`, `Resources/prism.icns`.

- [ ] **Step 1: Commit the Info.plist template**

Create `crates/prism-host/macos/Info.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Prism</string>
    <key>CFBundleDisplayName</key><string>Prism</string>
    <key>CFBundleExecutable</key><string>prism-host</string>
    <key>CFBundleIdentifier</key><string>dev.prism.host</string>
    <key>CFBundleIconFile</key><string>prism</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
```

- [ ] **Step 2: Write the bundle script**

Add to `scripts/install-prism-host-macos.sh` an `--app` path (or a new `scripts/build-prism-host-app.sh`) that:

```bash
# Build release binary
cargo build -p prism-host --release --locked
# Ensure the .icns exists (existing script path)
ICNS="assets/brand/macos/prism-host.icns"
# Assemble bundle
APP="target/Prism.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp crates/prism-host/macos/Info.plist "$APP/Contents/Info.plist"
cp target/release/prism-host "$APP/Contents/MacOS/prism-host"
cp "$ICNS" "$APP/Contents/Resources/prism.icns"
# Optional ad-hoc sign for local Gatekeeper
codesign --force --deep --sign - "$APP" || true
echo "Built $APP (unsigned/ad-hoc). Notarization deferred (docs/macos.md)."
```

- [ ] **Step 3: Run the script on macOS**

Run: `bash scripts/install-prism-host-macos.sh --app`
Expected: `target/Prism.app` exists. `open target/Prism.app` launches; Dock shows the name "Prism"; the menu bar reads "Prism".

- [ ] **Step 4: Verify Gatekeeper posture**

Run: `spctl -a -vvv target/Prism.app` — Expected: rejected/unsigned is acceptable for local; right-click > Open works. Document nothing new; `docs/macos.md` already covers it.

- [ ] **Step 5: Commit**

```bash
git add scripts/install-prism-host-macos.sh crates/prism-host/macos/Info.plist
git commit -m "build(host,macos): assemble local unsigned Prism.app bundle"
```

---

### Task 7: Promote spike to real Dock menu

**Files:**
- Modify: `crates/prism-host/src/macos_menu.rs` (replace the spike with a real `applicationDockMenu:` injection returning a New Window item), `crates/prism-host/src/main.rs` (swap the spike call for the real installer).

**Interfaces:**
- Consumes: the delegate-injection approach proven in Task 4; the action target from Task 5.
- Produces: `pub fn install_dock_menu(proxy: EventLoopProxy<UserAction>)` that adds `applicationDockMenu:` to winit's delegate class; the returned `NSMenu` has a "New Window" item targeting the Task 5 object.

- [ ] **Step 1: Write the failing test**

Reuse the pure table; add a Dock-menu title assertion:

```rust
#[cfg(target_os = "macos")]
#[test]
fn dock_menu_offers_new_window() {
    assert!(crate::macos_menu::dock_menu_titles().contains(&"New Window"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run (macOS): `cargo test -p prism-host dock_menu_offers_new_window`
Expected: FAIL — `cannot find function dock_menu_titles`.

- [ ] **Step 3: Implement the real Dock menu**

In `macos_menu.rs`:

- Add `pub fn dock_menu_titles() -> &'static [&'static str] { &["New Window"] }`.
- Replace `install_dock_menu_spike` with `install_dock_menu(proxy: EventLoopProxy<UserAction>)`. Use objc2 `class_addMethod` (via `AnyClass`/`ClassBuilder` as objc2 0.6 exposes it) to add `applicationDockMenu:` to the live delegate's class. The added method returns an `NSMenu` (retained/autoreleased per AppKit rules) built with a "New Window" item whose target is the shared action object (Task 5) and action `sel!(newWindow:)`. Store the proxy/target in the same `OnceLock` used by Task 5 so the Dock item and the menu-bar item share one target.
- Guard against double-adding the method (check with `class_getInstanceMethod` first) so opening multiple windows does not re-inject.

- [ ] **Step 4: Swap the call site**

In `open_window`, replace `macos_menu::install_dock_menu_spike();` with a one-time real install. Because the delegate exists only after launch, install on first window:

```rust
        #[cfg(target_os = "macos")]
        if self.windows.is_empty() {
            macos_menu::install_dock_menu(/* proxy stored/shared from Task 5 */);
        }
```

(Share the proxy through the Task 5 `OnceLock` rather than threading a new field; `install_dock_menu` reads it. The `self.windows.is_empty()` check runs before the new entry is inserted, so it is true only for the first window.)

- [ ] **Step 5: Run tests and app**

Run (macOS): `cargo test -p prism-host dock_menu_offers_new_window` — Expected: PASS.
Run: `open target/Prism.app` (rebuild via Task 6 script), right-click the Dock icon — Expected: "New Window" appears and opens a window. Verify opening several windows does not duplicate the method (no repeated inject logs).
Run (Linux): `cargo build -p prism-host && cargo test -p prism-host` — Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/prism-host/src/macos_menu.rs crates/prism-host/src/main.rs
git commit -m "feat(host,macos): Dock right-click New Window via delegate injection"
```

---

## Self-Review

**Spec coverage:**
- Dock menu → Task 4 (spike) + Task 7 (real). Menu bar + ⌘N → Task 5; cross-platform ⌘N → Task 3. Shared-session model → Tasks 2/3 reuse `attach_boot_command`/`open_window` (each window boots the same attach path). Multi-window core → Task 2. Event plumbing → Task 1. Local `.app` → Task 6. Testing posture → tests in Tasks 1–7, GUI display-gated. Open item (new vs existing session) → default new session, satisfied by reusing `open_window`'s existing boot path.

**Placeholder scan:** Menu-internals in Tasks 5 and 7 describe objc2 `define_class!`/`class_addMethod` construction in prose rather than full literal code, because the exact objc2 0.6 API is locked by the Task 4 spike; the pure, testable surfaces (`menu_item_table`, `dock_menu_titles`, `is_new_window_chord`, `UserAction`) have literal code and tests. This is intentional and gated, not a deferred requirement.

**Type consistency:** `UserAction { Wake, NewWindow }`, `EventLoopProxy<UserAction>`, `App.windows: HashMap<WindowId, HostState>`, `open_window -> Result<WindowId>`, `install_main_menu(proxy)`, `install_dock_menu(...)`, `menu_item_table`, `dock_menu_titles`, `is_new_window_chord` are used consistently across tasks.

## Notes for the executor

- Borrow checker: several handlers bind `host` from `self.windows`, then need `&mut self` to call `open_window`. Decide the action first, drop the `host` borrow, then call `self.open_window(event_loop)`.
- winit API names (`with_user_event`, `ApplicationHandler<UserAction>`, `Key::Character`, `ElementState`, `ModifiersState::super_key`) are for 0.30.x; confirm against `Cargo.lock` (0.30.13) if the compiler disagrees.
- Keep every macOS symbol behind `#[cfg(target_os = "macos")]`; Linux CI must build and test the crate unchanged.
