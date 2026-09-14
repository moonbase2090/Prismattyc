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

/// Set the native macOS window opacity through `NSWindow.alphaValue`.
///
/// The AppKit window is obtained from winit's `NSView` handle. This is a
/// whole-window opacity, so it also affects text and decorations. The
/// renderer keeps its normal opaque framebuffer on macOS.
pub fn set_window_opacity(window: &Window, opacity: f32) -> bool {
    use objc2::{msg_send, runtime::AnyObject, MainThreadMarker};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if MainThreadMarker::new().is_none() {
        eprintln!("prismattyc-host: macOS window opacity: not on the main thread");
        return false;
    }
    let Ok(handle) = window.window_handle() else {
        eprintln!("prismattyc-host: macOS window opacity: no native window handle");
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        eprintln!("prismattyc-host: macOS window opacity: unexpected window handle");
        return false;
    };

    // winit's AppKit handle is an NSView pointer. AppKit owns the view and its
    // window, so borrow both objects only for the duration of these messages.
    let view: &AnyObject = unsafe { &*handle.ns_view.as_ptr().cast() };
    // SAFETY: `view` is the live NSView supplied by winit, and this call runs
    // on the AppKit main thread. The returned NSWindow is not retained here.
    let native_window: *mut AnyObject = unsafe { msg_send![view, window] };
    if native_window.is_null() {
        eprintln!("prismattyc-host: macOS window opacity: NSView has no NSWindow");
        return false;
    }
    let native_window: &AnyObject = unsafe { &*native_window };
    let opacity = opacity.clamp(0.0, 1.0) as f64;
    // SAFETY: `native_window` is an NSWindow and `setAlphaValue:` takes a
    // CGFloat. CGFloat is f64 on supported macOS targets.
    unsafe {
        let _: () = msg_send![native_window, setAlphaValue: opacity];
    }
    true
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
