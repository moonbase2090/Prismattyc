//! Small AppKit integration for window properties that winit does not expose.

use std::cell::RefCell;
use std::collections::HashMap;

use winit::window::Window;

use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView,
};

thread_local! {
    /// AppKit retains subviews after insertion. Keep only their addresses so
    /// hot reload can find and remove the view without touching winit's view.
    static BLUR_VIEWS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
    /// Same pattern for the optional opaque title-bar fill used when the
    /// window content is translucent or blurred (see #22 / #23).
    static TITLEBAR_FILLS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
}

/// Install or remove the AppKit backdrop used by `window_blur`.
///
/// Returns `true` only when the effect view is installed. AppKit retains the
/// view after it is added to the winit view, so the thread-local map stores
/// only its address for later removal during hot reload.
pub fn set_window_blur(window: &Window, enabled: bool) -> bool {
    use objc2::{msg_send, runtime::AnyObject, MainThreadMarker, MainThreadOnly};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("prismattyc-host: macOS window blur: not on the main thread");
        return false;
    };
    let Ok(handle) = window.window_handle() else {
        eprintln!("prismattyc-host: macOS window blur: no native window handle");
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        eprintln!("prismattyc-host: macOS window blur: unexpected window handle");
        return false;
    };

    // winit's AppKit handle is an NSView pointer. AppKit owns this view and
    // retains subviews added to it.
    let view: &NSView = unsafe { &*handle.ns_view.as_ptr().cast() };
    // Confirm that winit has attached the view to a live AppKit window before
    // changing its subview tree.
    let view_as_object: &AnyObject = unsafe { &*(view as *const NSView).cast() };
    let native_window: *mut AnyObject = unsafe { msg_send![view_as_object, window] };
    if native_window.is_null() {
        eprintln!("prismattyc-host: macOS window blur: NSView has no NSWindow");
        return false;
    }

    let key = view as *const NSView as usize;
    if !enabled {
        let effect = BLUR_VIEWS.with(|views| views.borrow_mut().remove(&key));
        if let Some(effect) = effect {
            // SAFETY: the address came from the retained NSVisualEffectView
            // inserted by this module and remains live until removal.
            let effect: &NSVisualEffectView = unsafe { &*(effect as *const NSVisualEffectView) };
            effect.removeFromSuperview();
        }
        return false;
    }

    if let Some(effect) = BLUR_VIEWS.with(|views| views.borrow().get(&key).copied()) {
        // SAFETY: the address came from the retained NSVisualEffectView
        // inserted by this module.
        let effect: &NSVisualEffectView = unsafe { &*(effect as *const NSVisualEffectView) };
        effect.setFrame(view.bounds());
        return true;
    }

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), view.bounds());
    effect.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    // NSWindowBelow places the backdrop behind winit's terminal content view.
    // SAFETY: `view` and `effect` are live AppKit views, the selector is the
    // documented NSView subview-ordering method, and this runs on AppKit's
    // main thread. `-1` is NSWindowBelow; nil means below all current siblings.
    unsafe {
        let _: () = msg_send![
            view,
            addSubview: &*effect,
            positioned: -1i64,
            relativeTo: std::ptr::null::<NSView>()
        ];
    }
    BLUR_VIEWS.with(|views| {
        views
            .borrow_mut()
            .insert(key, (&*effect) as *const NSVisualEffectView as usize);
    });
    true
}

/// Whether the title bar needs an opaque fill behind the traffic lights.
///
/// Default opaque windows rely on AppKit's system chrome (including glass on
/// recent macOS). When `window_opacity < 1` or `window_blur` is on, content
/// can show through the title bar unless an opaque fill is present (#22/#23).
pub fn titlebar_needs_fill(window_opacity: f32, window_blur: bool) -> bool {
    window_blur || window_opacity < 1.0
}

/// What `sync_titlebar_background` should do for the current fill state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitlebarFillAction {
    /// Drop any installed fill (opaque window).
    Remove,
    /// Fill already present; leave it alone.
    Keep,
    /// Walk the title-bar ancestry and install a fill.
    Install,
}

fn titlebar_fill_action(needs_fill: bool, already_installed: bool) -> TitlebarFillAction {
    match (needs_fill, already_installed) {
        (false, _) => TitlebarFillAction::Remove,
        (true, true) => TitlebarFillAction::Keep,
        (true, false) => TitlebarFillAction::Install,
    }
}

/// Stop walking once the candidate contains the terminal content view.
fn titlebar_walk_hits_content(content_is_descendant: bool) -> bool {
    content_is_descendant
}

/// A full-width, non-empty ancestor is a title-bar container candidate.
fn is_titlebar_fill_container(view_width: f64, view_height: f64, content_width: f64) -> bool {
    view_width >= content_width && view_height > 0.0
}

/// Pick the first title-bar ancestor that can hold the opaque fill.
///
/// `candidates` is ordered from the close-button superview upward. Each entry
/// is `(width, height, content_is_descendant)`. Walking stops without a match
/// once a view that contains the terminal content is reached.
fn select_titlebar_fill_container(
    content_width: f64,
    candidates: &[(f64, f64, bool)],
) -> Option<usize> {
    for (index, &(width, height, hits_content)) in candidates.iter().enumerate() {
        if titlebar_walk_hits_content(hits_content) {
            return None;
        }
        if is_titlebar_fill_container(width, height, content_width) {
            return Some(index);
        }
    }
    None
}

const TITLEBAR_WALK_MAX: usize = 16;

// An AppKit-owned background behind the native title and traffic lights. Using
// NSBox lets AppKit resolve the semantic fill color when appearance changes.
// Hit testing passes through, preserving native dragging and window controls.
objc2::define_class!(
    // SAFETY: NSBox has no additional subclassing requirements. This class has
    // no ivars or Drop implementation and all access is on AppKit's main thread.
    #[unsafe(super = objc2_app_kit::NSBox)]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "PrismattycTitlebarBackground"]
    struct TitlebarBackground;

    impl TitlebarBackground {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: objc2_foundation::NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

/// Sync the native title bar with the current opacity / blur config.
///
/// Always asks AppKit for a non-transparent system title bar so current SDKs
/// can draw glass chrome in the default opaque case. Installs the opaque
/// `windowBackgroundColor` fill only when [`titlebar_needs_fill`] is true,
/// and removes it again when the window returns to opaque. Safe to call on
/// config hot reload. No terminal damage, presentation work, or animation
/// timer is introduced.
pub fn sync_titlebar_background(window: &Window, needs_fill: bool) {
    use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSBox, NSBoxType, NSColor, NSTitlePosition, NSWindowButton};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit supplies a live NSView, accessed on the main thread while
    // the Window owner remains alive.
    let content: &NSView = unsafe { &*handle.ns_view.as_ptr().cast() };
    let Some(native) = content.window() else {
        return;
    };
    native.setTitlebarAppearsTransparent(false);

    let key = content as *const NSView as usize;
    let already = TITLEBAR_FILLS.with(|views| views.borrow().contains_key(&key));
    match titlebar_fill_action(needs_fill, already) {
        TitlebarFillAction::Keep => return,
        TitlebarFillAction::Remove => {
            let fill = TITLEBAR_FILLS.with(|views| views.borrow_mut().remove(&key));
            if let Some(fill) = fill {
                // SAFETY: address came from a retained TitlebarBackground inserted
                // by this module and remains live until removal.
                let fill: &NSBox = unsafe { &*(fill as *const NSBox) };
                fill.removeFromSuperview();
            }
            return;
        }
        TitlebarFillAction::Install => {}
    }

    let Some(button) = native.standardWindowButton(NSWindowButton::CloseButton) else {
        return; // Borderless windows have no native title bar to fill.
    };
    // Collect public NSView ancestry (no private AppKit class names), then let
    // the pure selector pick the full-width title-bar container.
    let content_width = content.bounds().size.width;
    let mut ancestors: Vec<objc2::rc::Retained<NSView>> = Vec::new();
    let mut dims: Vec<(f64, f64, bool)> = Vec::new();
    let mut parent = unsafe { button.superview() };
    while ancestors.len() < TITLEBAR_WALK_MAX {
        let Some(view) = parent else { break };
        dims.push((
            view.bounds().size.width,
            view.bounds().size.height,
            content.isDescendantOf(&view),
        ));
        // SAFETY: traversal remains on the main thread with each view retained.
        parent = unsafe { view.superview() };
        ancestors.push(view);
    }
    let Some(index) = select_titlebar_fill_container(content_width, &dims) else {
        eprintln!("prismattyc-host: could not locate native title-bar background container");
        return;
    };
    let view = &ancestors[index];
    let bounds = view.bounds();
    // SAFETY: NSBox's designated initializer accepts an NSRect and
    // returns the initialized subclass; AppKit retains it on insertion.
    let background: objc2::rc::Retained<TitlebarBackground> = unsafe {
        msg_send![super(TitlebarBackground::alloc(mtm).set_ivars(())), initWithFrame: bounds]
    };
    background.setBoxType(NSBoxType::Custom);
    background.setTitlePosition(NSTitlePosition::NoTitle);
    background.setBorderWidth(0.0);
    background.setCornerRadius(0.0);
    background.setTransparent(false);
    background.setFillColor(&NSColor::windowBackgroundColor());
    background.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // SAFETY: documented NSView ordering API; -1 is NSWindowBelow.
    unsafe {
        let _: () = msg_send![view, addSubview: &*background,
            positioned: -1isize, relativeTo: std::ptr::null::<NSView>()];
    }
    TITLEBAR_FILLS.with(|views| {
        views
            .borrow_mut()
            .insert(key, (&*background) as *const TitlebarBackground as usize);
    });
}

/// Initial title-bar setup for a new window. See [`sync_titlebar_background`].
pub fn install_titlebar_background(window: &Window, needs_fill: bool) {
    sync_titlebar_background(window, needs_fill);
}

#[cfg(test)]
mod titlebar_fill_tests {
    use super::{
        is_titlebar_fill_container, select_titlebar_fill_container, titlebar_fill_action,
        titlebar_needs_fill, titlebar_walk_hits_content, TitlebarFillAction,
    };

    #[test]
    fn opaque_default_skips_fill() {
        assert!(!titlebar_needs_fill(1.0, false));
    }

    #[test]
    fn translucent_or_blur_needs_fill() {
        assert!(titlebar_needs_fill(0.8, false));
        assert!(titlebar_needs_fill(1.0, true));
        assert!(titlebar_needs_fill(0.5, true));
    }

    #[test]
    fn fill_action_covers_all_states() {
        assert_eq!(
            titlebar_fill_action(false, false),
            TitlebarFillAction::Remove
        );
        assert_eq!(
            titlebar_fill_action(false, true),
            TitlebarFillAction::Remove
        );
        assert_eq!(titlebar_fill_action(true, true), TitlebarFillAction::Keep);
        assert_eq!(
            titlebar_fill_action(true, false),
            TitlebarFillAction::Install
        );
    }

    #[test]
    fn walk_stops_on_content_descendant() {
        assert!(titlebar_walk_hits_content(true));
        assert!(!titlebar_walk_hits_content(false));
    }

    #[test]
    fn fill_container_requires_full_width_and_height() {
        assert!(is_titlebar_fill_container(800.0, 28.0, 800.0));
        assert!(is_titlebar_fill_container(900.0, 28.0, 800.0));
        assert!(!is_titlebar_fill_container(100.0, 28.0, 800.0));
        assert!(!is_titlebar_fill_container(800.0, 0.0, 800.0));
        assert!(!is_titlebar_fill_container(800.0, -1.0, 800.0));
    }

    #[test]
    fn select_container_picks_first_full_width() {
        let candidates = [
            (100.0, 28.0, false),
            (800.0, 28.0, false),
            (800.0, 600.0, true),
        ];
        assert_eq!(select_titlebar_fill_container(800.0, &candidates), Some(1));
    }

    #[test]
    fn select_container_stops_at_content() {
        let candidates = [
            (100.0, 28.0, false),
            (800.0, 600.0, true),
            (800.0, 28.0, false),
        ];
        assert_eq!(select_titlebar_fill_container(800.0, &candidates), None);
    }

    #[test]
    fn select_container_empty_is_none() {
        assert_eq!(select_titlebar_fill_container(800.0, &[]), None);
    }
}
