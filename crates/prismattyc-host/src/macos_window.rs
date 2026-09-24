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

/// Keep native chrome readable independently of content opacity and blur.
/// Called once per window; AppKit owns, resizes, and redraws the backdrop.
/// No terminal damage, presentation work, or animation timer is introduced.
pub fn install_titlebar_background(window: &Window) {
    use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSBoxType, NSColor, NSTitlePosition, NSWindowButton};
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
    let Some(button) = native.standardWindowButton(NSWindowButton::CloseButton) else {
        return; // Borderless windows have no native title bar to fill.
    };
    // Walk public NSView relationships, without relying on private AppKit class
    // names. Stop before a container holding terminal content, and choose a
    // full-width title-bar ancestor rather than a traffic-light-only cluster.
    let mut parent = unsafe { button.superview() };
    for _ in 0..16 {
        let Some(view) = parent else { break };
        if content.isDescendantOf(&view) {
            break;
        }
        let bounds = view.bounds();
        if bounds.size.width >= content.bounds().size.width && bounds.size.height > 0.0 {
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
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            // SAFETY: documented NSView ordering API; -1 is NSWindowBelow.
            unsafe {
                let _: () = msg_send![&*view, addSubview: &*background,
                    positioned: -1isize, relativeTo: std::ptr::null::<NSView>()];
            }
            return;
        }
        // SAFETY: traversal remains on the main thread with each view retained.
        parent = unsafe { view.superview() };
    }
    eprintln!("prismattyc-host: could not locate native title-bar background container");
}
