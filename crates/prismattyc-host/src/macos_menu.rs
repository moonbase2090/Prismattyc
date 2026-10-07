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

use crate::UserAction;

/// The File-menu items this app installs: (title, key equivalent). Keyboard
/// equivalents stay empty so the configurable host keymap owns shortcuts.
pub fn menu_item_table() -> &'static [(&'static str, &'static str)] {
    &[("New Window", "")]
}

/// The Dock right-click menu items this app installs. Pure and AppKit-free
/// so it is unit-testable without a display (Task 7).
pub fn dock_menu_titles() -> &'static [&'static str] {
    &["New Window"]
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
    }
);

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
/// AppKit for `NSApp.mainMenu` to behave) plus a File menu with "New Window".
/// Keyboard shortcuts are handled by the configurable host keymap. Must run
/// on the main thread, before or shortly after launch so the
/// menu and key equivalent are live from the first window (Task 5).
pub fn install_main_menu(proxy: EventLoopProxy<UserAction>, automatic_checks: bool) {
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

    let quit_item = NSMenuItem::new(mtm);
    quit_item.setTitle(&NSString::from_str("Quit Prismattyc"));
    quit_item.setKeyEquivalent(&NSString::from_str(""));
    // SAFETY: `terminate:` is a valid selector on the responder chain
    // (`NSApplication`); leaving target unset routes it there.
    unsafe {
        quit_item.setAction(Some(sel!(terminate:)));
    }
    app_menu.addItem(&quit_item);
    app_menu_item.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_menu_item);

    // File menu: "New Window" via the action-target object above. Its key
    // equivalent is empty; the host keymap handles the configurable chord.
    let file_menu_item = NSMenuItem::new(mtm);
    file_menu_item.setTitle(&NSString::from_str("File"));
    let file_menu = NSMenu::new(mtm);
    file_menu.setTitle(&NSString::from_str("File"));
    for (title, key) in menu_item_table() {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        item.setKeyEquivalent(&NSString::from_str(key));
        item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        // SAFETY: `target` is kept alive for the process lifetime in
        // `NEW_WINDOW_TARGET`; `newWindow:` is defined above and matches the
        // one-argument action-method signature AppKit expects.
        unsafe {
            item.setTarget(Some(&target));
            item.setAction(Some(sel!(newWindow:)));
        }
        file_menu.addItem(&item);
    }
    file_menu_item.setSubmenu(Some(&file_menu));
    main_menu.addItem(&file_menu_item);

    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main_menu));
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
    let notes = release_notes.trim();
    let notes = if notes.is_empty() {
        "No release notes were published.".to_string()
    } else if notes.chars().count() > 8_000 {
        format!("{}…", notes.chars().take(8_000).collect::<String>())
    } else {
        notes.to_string()
    };
    alert.setInformativeText(&NSString::from_str(&notes));
    alert.addButtonWithTitle(&NSString::from_str("Install and Relaunch"));
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
    fn menu_table_lists_new_window_cmd_n() {
        let table = crate::macos_menu::menu_item_table();
        assert!(table
            .iter()
            .any(|(title, key)| *title == "New Window" && key.is_empty()));
    }

    #[test]
    fn dock_menu_offers_new_window() {
        assert!(crate::macos_menu::dock_menu_titles().contains(&"New Window"));
    }
}
