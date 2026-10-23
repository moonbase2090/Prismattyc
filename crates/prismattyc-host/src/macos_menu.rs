//! macOS menu integration (menu bar and Dock menu).
//!
//! winit 0.30 owns the `NSApplicationDelegate`. To surface a Dock right-click
//! "New Window", we add `applicationDockMenu:` to winit's delegate class at
//! runtime (Task 4 spiked delegate reachability; Task 7 promotes it to a real
//! injection wired to the same action target as the File menu, Task 5).

use std::ptr;
use std::sync::OnceLock;

use objc2::ffi as objc2_ffi;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueOff,
    NSControlStateValueOn, NSEventModifierFlags, NSMenu, NSMenuItem,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
use winit::event_loop::EventLoopProxy;

use crate::keybind::{Action, Chord, KeyMap, KeySpec};
use crate::update_notes::prepare_update_release_notes;
use crate::UserAction;
use winit::keyboard::NamedKey;

/// The Dock right-click menu items this app installs. Pure and AppKit-free
/// so it is unit-testable without a display (Task 7).
pub fn dock_menu_titles() -> &'static [&'static str] {
    &["New Window"]
}

/// A menu item that sends an AppKit standard action through the responder
/// chain (no explicit target), so the system handles it.
fn standard_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    modifiers: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setKeyEquivalent(&NSString::from_str(key));
    item.setKeyEquivalentModifierMask(modifiers);
    // SAFETY: each `action` is a standard `NSApplication`/`NSWindow` selector
    // with the one-argument action signature; no target means the responder
    // chain delivers it.
    unsafe {
        item.setAction(Some(action));
    }
    item
}

/// Adds the standard Window menu (Minimize, Zoom) to `main_menu`. Both items
/// have no explicit target, so AppKit routes them to the key window through
/// the responder chain.
fn add_window_menu(mtm: MainThreadMarker, main_menu: &NSMenu) -> Retained<NSMenu> {
    let window_menu_item = NSMenuItem::new(mtm);
    window_menu_item.setTitle(&NSString::from_str("Window"));
    let window_menu = NSMenu::new(mtm);
    window_menu.setTitle(&NSString::from_str("Window"));

    let minimize_item = NSMenuItem::new(mtm);
    minimize_item.setTitle(&NSString::from_str("Minimize"));
    minimize_item.setKeyEquivalent(&NSString::from_str("m"));
    minimize_item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    // SAFETY: `performMiniaturize:` is a standard `NSWindow` action; leaving
    // the target unset routes it through the responder chain.
    unsafe {
        minimize_item.setAction(Some(sel!(performMiniaturize:)));
    }
    window_menu.addItem(&minimize_item);

    let zoom_item = NSMenuItem::new(mtm);
    zoom_item.setTitle(&NSString::from_str("Zoom"));
    // SAFETY: `performZoom:` is a standard `NSWindow` action; leaving the
    // target unset routes it through the responder chain.
    unsafe {
        zoom_item.setAction(Some(sel!(performZoom:)));
    }
    window_menu.addItem(&zoom_item);

    window_menu_item.setSubmenu(Some(&window_menu));
    main_menu.addItem(&window_menu_item);
    window_menu
}

/// Backing storage for `NewWindowTarget`'s selector-callable state.
struct NewWindowTargetIvars {
    proxy: EventLoopProxy<UserAction>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `NewWindowTarget` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = NewWindowTargetIvars]
    struct NewWindowTarget;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for NewWindowTarget {}

    impl NewWindowTarget {
        /// Action target for the File > New Window menu item. Forwards to the
        /// winit event loop so window creation
        /// happens on `ApplicationHandler::user_event`, same path as the
        /// (later) Dock menu (Task 7).
        #[unsafe(method(newWindow:))]
        fn new_window(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().proxy.send_event(UserAction::NewWindow);
        }

        #[unsafe(method(openHelp:))]
        fn open_help(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let url = match -tag - 1 {
                0 => "https://github.com/moonbase2090/Prismattyc/wiki",
                1 => "https://github.com/moonbase2090/Prismattyc/issues/new",
                2 => "https://github.com/moonbase2090/Prismattyc/releases",
                _ => return,
            };
            let _ = crate::open_url(url);
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().proxy.send_event(UserAction::CheckForUpdates);
        }

        #[unsafe(method(toggleAutomaticUpdateChecks:))]
        fn toggle_automatic_update_checks(&self, _sender: Option<&AnyObject>) {
            let _ = self
                .ivars()
                .proxy
                .send_event(UserAction::ToggleAutomaticUpdateChecks);
        }

        #[unsafe(method(rollbackUpdate:))]
        fn rollback_update(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().proxy.send_event(UserAction::RollbackUpdate);
        }

        /// Runs the host action at `MENU_HOST_ACTIONS[tag]` on the focused
        /// window, the same dispatch the keymap uses.
        #[unsafe(method(runAction:))]
        fn run_action(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            // SAFETY: `sender` is the NSMenuItem that invoked this action, and
            // `tag` is a plain NSInteger getter on NSMenuItem.
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((_, action)) = usize::try_from(tag - 1)
                .ok()
                .and_then(|index| MENU_HOST_ACTIONS.get(index))
            else {
                return;
            };
            let _ = self.ivars().proxy.send_event(UserAction::RunAction(*action));
        }
    }
);

/// Menu tags are one-based indices, leaving zero for native AppKit items.
const MENU_HOST_ACTIONS: &[(&str, Action)] = &[
    ("Copy", Action::Copy),
    ("Paste", Action::Paste),
    ("Select All", Action::SelectAll),
    ("Find…", Action::Find),
    ("Zoom In", Action::IncreaseFontSize),
    ("Zoom Out", Action::DecreaseFontSize),
    ("Actual Size", Action::ResetFontSize),
    ("Toggle Sidebar", Action::SidebarCollapse),
    ("Zoom Pane", Action::ZoomPane),
    ("Theme…", Action::ThemePicker),
    ("New Window", Action::NewWindow),
    ("New Tab", Action::NewTab),
    ("New Pane", Action::SplitRight),
    ("Close Pane", Action::ClosePane),
    ("Close Tab", Action::CloseTab),
    ("New Session…", Action::NewSessionTab),
    ("Open Space…", Action::OpenSpace),
    ("Settings…", Action::OpenConfig),
    ("Quit Prismattyc", Action::Quit),
];

fn key_equivalent(chord: Chord) -> Option<(String, NSEventModifierFlags)> {
    let key = match chord.key {
        KeySpec::Char(ch) => ch.to_string(),
        KeySpec::Named(named) => match named {
            NamedKey::Enter => "\r".into(),
            NamedKey::Tab => "\t".into(),
            NamedKey::Space => " ".into(),
            NamedKey::Escape => "\u{1b}".into(),
            NamedKey::Backspace => "\u{8}".into(),
            NamedKey::Delete => "\u{7f}".into(),
            NamedKey::ArrowUp => "\u{f700}".into(),
            NamedKey::ArrowDown => "\u{f701}".into(),
            NamedKey::ArrowLeft => "\u{f702}".into(),
            NamedKey::ArrowRight => "\u{f703}".into(),
            NamedKey::Home => "\u{f729}".into(),
            NamedKey::End => "\u{f72b}".into(),
            NamedKey::PageUp => "\u{f72c}".into(),
            NamedKey::PageDown => "\u{f72d}".into(),
            _ => return None,
        },
    };
    let mut modifiers = NSEventModifierFlags::empty();
    modifiers.set(NSEventModifierFlags::Command, chord.super_key);
    modifiers.set(NSEventModifierFlags::Control, chord.ctrl);
    modifiers.set(NSEventModifierFlags::Option, chord.alt);
    modifiers.set(NSEventModifierFlags::Shift, chord.shift);
    Some((key, modifiers))
}

fn set_action_shortcut(item: &NSMenuItem, keymap: &KeyMap, action: Action) {
    let mut chords = keymap.chords(action);
    // Prefer the platform's Command binding when there are portable aliases.
    chords.sort_by_key(|chord| !chord.super_key);
    let (key, modifiers) = chords
        .into_iter()
        .find_map(key_equivalent)
        .unwrap_or_else(|| (String::new(), NSEventModifierFlags::empty()));
    item.setKeyEquivalent(&NSString::from_str(&key));
    item.setKeyEquivalentModifierMask(modifiers);
}

/// Refresh visible equivalents when the authoritative keymap reloads.
pub fn refresh_shortcuts(keymap: &KeyMap) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() else {
        return;
    };
    refresh_menu_shortcuts(&menu, keymap);
}

fn refresh_menu_shortcuts(menu: &NSMenu, keymap: &KeyMap) {
    for item in menu.itemArray() {
        if let Some(submenu) = item.submenu() {
            refresh_menu_shortcuts(&submenu, keymap);
        }
        if let Some((_, action)) = item
            .tag()
            .checked_sub(1)
            .and_then(|tag| usize::try_from(tag).ok())
            .and_then(|index| MENU_HOST_ACTIONS.get(index))
        {
            set_action_shortcut(&item, keymap, *action);
        }
    }
}

/// Builds a menu item for `MENU_HOST_ACTIONS[index]`, wired to `target`.
fn host_action_item(
    mtm: MainThreadMarker,
    target: &NewWindowTarget,
    index: usize,
    keymap: &KeyMap,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    let (title, action) = MENU_HOST_ACTIONS[index];
    item.setTitle(&NSString::from_str(title));
    item.setTag(index as isize + 1);
    set_action_shortcut(&item, keymap, action);
    // SAFETY: `runAction:` is defined on `NewWindowTarget` above and takes the
    // one-argument action signature AppKit expects; `target` outlives the menu.
    unsafe {
        item.setTarget(Some(target));
        item.setAction(Some(sel!(runAction:)));
    }
    item
}

/// Adds the Edit menu and the host-backed View items to `main_menu`. Copy,
/// Paste and the zoom items run through the keymap; Enter Full Screen uses
/// AppKit's own action.
fn add_edit_and_view_menus(
    mtm: MainThreadMarker,
    target: &NewWindowTarget,
    main_menu: &NSMenu,
    keymap: &KeyMap,
) {
    let edit_menu_item = NSMenuItem::new(mtm);
    edit_menu_item.setTitle(&NSString::from_str("Edit"));
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));
    for (title, action, key, modifiers) in [
        ("Undo", sel!(undo:), "z", NSEventModifierFlags::Command),
        (
            "Redo",
            sel!(redo:),
            "z",
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        ),
        ("Cut", sel!(cut:), "x", NSEventModifierFlags::Command),
    ] {
        edit_menu.addItem(&standard_item(mtm, title, action, key, modifiers));
    }
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    for index in 0..=3 {
        edit_menu.addItem(&host_action_item(mtm, target, index, keymap));
    }
    edit_menu_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_menu_item);

    let view_menu_item = NSMenuItem::new(mtm);
    view_menu_item.setTitle(&NSString::from_str("View"));
    let view_menu = NSMenu::new(mtm);
    view_menu.setTitle(&NSString::from_str("View"));
    for index in 4..10 {
        view_menu.addItem(&host_action_item(mtm, target, index, keymap));
    }
    view_menu.addItem(&NSMenuItem::separatorItem(mtm));
    view_menu.addItem(&standard_item(
        mtm,
        "Enter Full Screen",
        sel!(toggleFullScreen:),
        "",
        NSEventModifierFlags::empty(),
    ));
    view_menu_item.setSubmenu(Some(&view_menu));
    main_menu.addItem(&view_menu_item);
}

impl NewWindowTarget {
    fn new(mtm: MainThreadMarker, proxy: EventLoopProxy<UserAction>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NewWindowTargetIvars { proxy });
        // SAFETY: The signature of `NSObject`'s `init` method is correct.
        unsafe { msg_send![super(this), init] }
    }
}

/// `Retained<NewWindowTarget>` is `!Send + !Sync` (main-thread-only AppKit
/// object), but a `static` needs `Sync` to be legal. Wrap it: every access
/// this module makes is gated behind a fresh `MainThreadMarker::new()`
/// check, so it never actually crosses threads in practice.
struct MainThreadOnlyBox(#[allow(dead_code)] Retained<NewWindowTarget>);
// SAFETY: only ever constructed and read from the main thread (guarded by
// `MainThreadMarker` at both call sites in this module).
unsafe impl Send for MainThreadOnlyBox {}
// SAFETY: see above.
unsafe impl Sync for MainThreadOnlyBox {}

/// Keeps the action-target object alive for the process lifetime; AppKit
/// only holds a weak-ish reference via `setTarget`, and menu items do not
/// retain arbitrary targets the way they retain submenus.
static NEW_WINDOW_TARGET: OnceLock<MainThreadOnlyBox> = OnceLock::new();

struct MainThreadOnlyMenuItemBox(#[allow(dead_code)] Retained<NSMenuItem>);
// SAFETY: the item is created and read only while a MainThreadMarker exists.
unsafe impl Send for MainThreadOnlyMenuItemBox {}
// SAFETY: see above.
unsafe impl Sync for MainThreadOnlyMenuItemBox {}

static AUTOMATIC_UPDATE_ITEM: OnceLock<MainThreadOnlyMenuItemBox> = OnceLock::new();

/// Installs the real macOS menu bar: an application submenu (required by
/// AppKit for `NSApp.mainMenu` to behave), File, Edit, View, Window, and Help.
/// Menu shortcuts come from the configurable host keymap. Must run
/// on the main thread, before or shortly after launch so the
/// menu and key equivalent are live from the first window (Task 5).
pub fn install_main_menu(
    proxy: EventLoopProxy<UserAction>,
    automatic_checks: bool,
    keymap: &KeyMap,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("prismattyc-host: menu bar: not main thread; skipping install");
        return;
    };

    let target = NewWindowTarget::new(mtm, proxy);
    let _ = NEW_WINDOW_TARGET.set(MainThreadOnlyBox(target.clone()));

    let main_menu = NSMenu::new(mtm);

    // Application submenu: AppKit requires the main menu's first item to
    // carry a submenu for the app menu (bold app name) to render correctly.
    let app_menu_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&standard_item(
        mtm,
        "About Prismattyc",
        sel!(orderFrontStandardAboutPanel:),
        "",
        NSEventModifierFlags::empty(),
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let check_item = NSMenuItem::new(mtm);
    check_item.setTitle(&NSString::from_str("Check for Updates…"));
    // SAFETY: `target` is retained for the app lifetime and implements this
    // selector with AppKit's one-argument action signature.
    unsafe {
        check_item.setTarget(Some(&target));
        check_item.setAction(Some(sel!(checkForUpdates:)));
    }
    app_menu.addItem(&check_item);

    let automatic_item = NSMenuItem::new(mtm);
    automatic_item.setTitle(&NSString::from_str("Automatically Check for Updates"));
    automatic_item.setState(if automatic_checks {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    unsafe {
        automatic_item.setTarget(Some(&target));
        automatic_item.setAction(Some(sel!(toggleAutomaticUpdateChecks:)));
    }
    let _ = AUTOMATIC_UPDATE_ITEM.set(MainThreadOnlyMenuItemBox(automatic_item.clone()));
    app_menu.addItem(&automatic_item);

    let rollback_item = NSMenuItem::new(mtm);
    rollback_item.setTitle(&NSString::from_str("Roll Back Last Update…"));
    unsafe {
        rollback_item.setTarget(Some(&target));
        rollback_item.setAction(Some(sel!(rollbackUpdate:)));
    }
    app_menu.addItem(&rollback_item);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&host_action_item(mtm, &target, 17, keymap));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let services_menu = NSMenu::new(mtm);
    let services_item = NSMenuItem::new(mtm);
    services_item.setTitle(&NSString::from_str("Services"));
    services_item.setSubmenu(Some(&services_menu));
    app_menu.addItem(&services_item);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&standard_item(
        mtm,
        "Hide Prismattyc",
        sel!(hide:),
        "h",
        NSEventModifierFlags::Command,
    ));
    app_menu.addItem(&standard_item(
        mtm,
        "Hide Others",
        sel!(hideOtherApplications:),
        "h",
        NSEventModifierFlags::Command | NSEventModifierFlags::Option,
    ));
    app_menu.addItem(&standard_item(
        mtm,
        "Show All",
        sel!(unhideAllApplications:),
        "",
        NSEventModifierFlags::empty(),
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));

    app_menu.addItem(&host_action_item(mtm, &target, 18, keymap));
    app_menu_item.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_menu_item);

    let file_menu_item = NSMenuItem::new(mtm);
    file_menu_item.setTitle(&NSString::from_str("File"));
    let file_menu = NSMenu::new(mtm);
    file_menu.setTitle(&NSString::from_str("File"));
    for index in 10..17 {
        file_menu.addItem(&host_action_item(mtm, &target, index, keymap));
    }
    file_menu_item.setSubmenu(Some(&file_menu));
    main_menu.addItem(&file_menu_item);

    add_edit_and_view_menus(mtm, &target, &main_menu, keymap);
    let window_menu = add_window_menu(mtm, &main_menu);

    let help_menu = NSMenu::new(mtm);
    help_menu.setTitle(&NSString::from_str("Help"));
    for (index, title) in ["Prismattyc Help", "Report an Issue…", "Release Notes"]
        .iter()
        .enumerate()
    {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        // Negative tags distinguish Help items from host-action tags.
        item.setTag(-(index as isize) - 1);
        unsafe {
            item.setTarget(Some(&target));
            item.setAction(Some(sel!(openHelp:)));
        }
        help_menu.addItem(&item);
    }
    let help_item = NSMenuItem::new(mtm);
    help_item.setTitle(&NSString::from_str("Help"));
    help_item.setSubmenu(Some(&help_menu));
    main_menu.addItem(&help_item);
    let app = NSApplication::sharedApplication(mtm);
    app.setHelpMenu(Some(&help_menu));
    app.setMainMenu(Some(&main_menu));
    // AppKit fills this menu with the open windows and keeps it updated.
    app.setWindowsMenu(Some(&window_menu));
    app.setServicesMenu(Some(&services_menu));
    eprintln!("prismattyc-host: menu bar installed (Check for Updates)");
}

pub fn set_automatic_checks_enabled(enabled: bool) {
    let Some(_mtm) = MainThreadMarker::new() else {
        return;
    };
    if let Some(item) = AUTOMATIC_UPDATE_ITEM.get() {
        item.0.setState(if enabled {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
}

pub fn prompt_update(version: &str, release_notes: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!(
        "Prismattyc {version} is available"
    )));
    let notes = prepare_update_release_notes(version, release_notes);
    let notes = if notes.is_empty() {
        "No release notes were published.".to_string()
    } else {
        notes
    };
    alert.setInformativeText(&NSString::from_str(&notes));
    alert.addButtonWithTitle(&NSString::from_str("Review Update"));
    alert.addButtonWithTitle(&NSString::from_str("Later"));
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn confirm_rollback() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Roll Back Prismattyc?"));
    alert.setInformativeText(&NSString::from_str(
        "Restore the previous verified release. The app will restart.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Roll Back"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn show_update_message(title: &str, message: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    let _ = alert.runModal();
}

/// The `applicationDockMenu:` implementation added to winit's delegate
/// class. Builds a fresh one-item NSMenu ("New Window") targeting the
/// shared `NEW_WINDOW_TARGET` (installed by `install_main_menu`, Task 5) so
/// the Dock item and the File-menu item drive the same action object.
///
/// # Safety / FFI contract
/// This is called directly by the Objective-C runtime on every Dock
/// right-click, so it must never unwind across the FFI boundary (unwinding
/// into ObjC frames is UB). The body is wrapped in `catch_unwind` as a
/// defensive backstop; the empty-`OnceLock` and non-main-thread cases are
/// handled by returning null/an empty menu rather than panicking.
extern "C-unwind" fn application_dock_menu_imp(
    _this: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> *mut NSMenu {
    std::panic::catch_unwind(|| -> *mut NSMenu {
        let Some(mtm) = MainThreadMarker::new() else {
            return ptr::null_mut();
        };
        let menu = NSMenu::new(mtm);
        if let Some(boxed) = NEW_WINDOW_TARGET.get() {
            let target = &boxed.0;
            for title in dock_menu_titles() {
                let item = NSMenuItem::new(mtm);
                item.setTitle(&NSString::from_str(title));
                // SAFETY: `target` is kept alive for the process lifetime in
                // `NEW_WINDOW_TARGET`; `newWindow:` matches the one-argument
                // action-method signature AppKit expects (same as Task 5).
                unsafe {
                    item.setTarget(Some(target));
                    item.setAction(Some(sel!(newWindow:)));
                }
                menu.addItem(&item);
            }
        }
        // AppKit expects an autoreleased return from `applicationDockMenu:`;
        // `autorelease_return` hands off the retain count without an extra
        // retain/release round trip and without leaking on every Dock click.
        Retained::autorelease_return(menu)
    })
    .unwrap_or(ptr::null_mut())
}

/// Injects `applicationDockMenu:` into winit's live `NSApplicationDelegate`
/// class so right-clicking the Dock tile shows "New Window" wired to the
/// same action target as File > New Window (Task 5's `NEW_WINDOW_TARGET`).
///
/// Idempotent: checks `instance_method` first so calling this more than
/// once (e.g. across several `open_window` calls, defensively) does not
/// re-add the method. Callers should still only invoke this once, on the
/// first window (see `open_window`'s `self.windows.is_empty()` guard),
/// since the delegate only exists once winit has finished launching.
pub fn install_dock_menu() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("prismattyc-host: dock menu: not main thread; skipping install");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: delegate exists after winit finished launching; main thread only.
    unsafe {
        let delegate: *mut AnyObject = msg_send![&app, delegate];
        if delegate.is_null() {
            eprintln!("prismattyc-host: dock menu: no delegate yet; skipping install");
            return;
        }
        let class = (*delegate).class();
        let sel = sel!(applicationDockMenu:);
        if class.instance_method(sel).is_some() {
            // Already injected; do not add it a second time.
            return;
        }

        // Type encoding for `applicationDockMenu:`: returns `id` (`@`),
        // implicit `self` (`@`) and `_cmd` (`:`), one `id` argument (`@`).
        let types = c"@@:@";
        let imp: objc2::runtime::Imp = core::mem::transmute::<
            extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> *mut NSMenu,
            objc2::runtime::Imp,
        >(application_dock_menu_imp);
        let added = objc2_ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            sel,
            imp,
            types.as_ptr(),
        );
        if added.as_bool() {
            eprintln!("prismattyc-host: dock menu installed");
        } else {
            eprintln!("prismattyc-host: dock menu: class_addMethod failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn configured_chord_becomes_native_equivalent() {
        use super::*;
        let chord = Chord::parse("ctrl+alt+shift+x").unwrap();
        let (key, modifiers) = key_equivalent(chord).unwrap();
        assert_eq!(key, "x");
        assert_eq!(
            modifiers,
            NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Shift
        );
        let (key, modifiers) = key_equivalent(Chord::parse("super+shift+=").unwrap()).unwrap();
        assert_eq!(key, "=");
        assert_eq!(
            modifiers,
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift
        );
    }

    #[test]
    fn dock_menu_offers_new_window() {
        assert!(crate::macos_menu::dock_menu_titles().contains(&"New Window"));
    }
}
