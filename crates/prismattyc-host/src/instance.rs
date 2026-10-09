//! One bare Windows GUI launch owns the host. A second bare launch focuses
//! that window. An attach or an explicit program is another window.

#[cfg(windows)]
use std::sync::Mutex;

#[cfg(windows)]
const MUTEX_NAME: &str = "Local\\dev.prismattyc.host\0";
#[cfg(windows)]
const EVENT_NAME: &str = "Local\\dev.prismattyc.host.focus\0";

/// Handles kept for the process lifetime. Dropping them would release the
/// single-instance mutex while the window is still open.
#[cfg(windows)]
static OWNER: Mutex<Option<isize>> = Mutex::new(None);
#[cfg(windows)]
static FOCUS: Mutex<Option<isize>> = Mutex::new(None);

/// A bare GUI launch is the single instance. Attach and an explicit program
/// open another window.
#[must_use]
pub fn claims_single_instance(explicit_program: bool, attach_sessions: usize) -> bool {
    !explicit_program && attach_sessions == 0
}

/// True when this process should exit because another host owns the window.
///
/// `wait_for_owner` is the restart handoff. The parent releases the mutex
/// before spawning the replacement. A timeout means the parent is gone, so
/// this process continues instead of focusing a window that no longer exists.
pub fn claim_or_focus(single: bool, wait_for_owner: bool) -> bool {
    if !single {
        return false;
    }
    #[cfg(windows)]
    {
        claim_windows(wait_for_owner)
    }
    #[cfg(not(windows))]
    {
        let _ = wait_for_owner;
        false
    }
}

#[must_use]
pub fn owns_instance() -> bool {
    #[cfg(windows)]
    {
        OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Drop the mutex so a replacement host can claim it. Called before the
/// restart spawn. If the child starts while this process still holds the
/// mutex, the child focuses this window and exits, and the app disappears.
#[cfg(windows)]
pub fn release() {
    close_slot(&OWNER);
    close_slot(&FOCUS);
}

#[cfg(windows)]
fn close_slot(slot: &Mutex<Option<isize>>) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    let handle = slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(handle) = handle {
        unsafe {
            CloseHandle(handle as HANDLE);
        }
    }
}

#[cfg(windows)]
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

#[cfg(windows)]
fn claim_windows(wait_for_owner: bool) -> bool {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let attempts = if wait_for_owner { 50 } else { 1 };
    for attempt in 0..attempts {
        // The event exists before the mutex is visible, so a second launch
        // cannot signal a name that disappears when its own handle closes.
        let event = create_focus_event();
        let mutex_name = wide(MUTEX_NAME);
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr()) };
        if mutex.is_null() {
            close_handle(event);
            return false;
        }
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            *OWNER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mutex as isize);
            if !event.is_null() {
                *FOCUS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event as isize);
            }
            return false;
        }
        unsafe {
            CloseHandle(mutex as HANDLE);
        }
        if !wait_for_owner {
            signal_focus(event);
            close_handle(event);
            // Stay alive long enough for the owner to take the foreground.
            // Exiting immediately drops the grant from AllowSetForegroundWindow.
            std::thread::sleep(Duration::from_millis(400));
            return true;
        }
        close_handle(event);
        if attempt + 1 == attempts {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(windows)]
fn create_focus_event() -> windows_sys::Win32::Foundation::HANDLE {
    use windows_sys::Win32::System::Threading::CreateEventW;
    let name = wide(EVENT_NAME);
    unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) }
}

#[cfg(windows)]
fn close_handle(handle: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::Foundation::CloseHandle;
    if !handle.is_null() {
        unsafe {
            CloseHandle(handle);
        }
    }
}

#[cfg(windows)]
fn signal_focus(event: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::System::Threading::SetEvent;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    if event.is_null() {
        return;
    }
    unsafe {
        // This process was just launched, so it may grant foreground to the
        // host that already owns the window.
        AllowSetForegroundWindow(u32::MAX);
        SetEvent(event);
    }
}

/// Wait for a second bare launch and run `on_focus` on the host thread's proxy.
#[cfg(windows)]
pub fn watch(on_focus: impl Fn() + Send + 'static) {
    let handle = *FOCUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(handle) = handle else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("prismattyc-host-focus".into())
        .spawn(move || {
            use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::{ResetEvent, WaitForSingleObject};
            let event = handle as HANDLE;
            loop {
                if unsafe { WaitForSingleObject(event, u32::MAX) } != WAIT_OBJECT_0 {
                    break;
                }
                unsafe {
                    ResetEvent(event);
                }
                on_focus();
            }
        });
}

/// Restore and foreground the host window for a second bare launch.
#[cfg(windows)]
pub fn foreground(window: &winit::window::Window) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    window.set_minimized(false);
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(win32) = handle.as_raw() {
            let hwnd = win32.hwnd.get() as HWND;
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
        }
    }
    window.focus_window();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_launch_claims_the_single_instance() {
        assert!(claims_single_instance(false, 0));
        assert!(!claims_single_instance(true, 0));
        assert!(!claims_single_instance(false, 1));
        assert!(!claim_or_focus(false, true));
        assert!(!claim_or_focus(false, false));
        #[cfg(not(windows))]
        {
            assert!(!claim_or_focus(true, true));
            assert!(!owns_instance());
        }
    }
}
