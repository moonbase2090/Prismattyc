// SPDX-License-Identifier: MIT
//! Shared child-launch and descriptor acquisition boundary.
//!
//! A poisoned Darwin gate fails closed for every launch through this crate,
//! including application update and restart. Recover by restarting the host
//! process rather than continuing after an uncertain descriptor window.

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod fd;

#[cfg(target_os = "macos")]
static LAUNCH_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
fn acquire() -> io::Result<std::sync::MutexGuard<'static, ()>> {
    #[cfg(test)]
    if let Some(result) = test_hooks::observe_acquire() {
        return result;
    }
    LAUNCH_GATE
        .lock()
        .map_err(|_| io::Error::other("process launch gate poisoned"))
}

/// Launches a configured command, excluding Darwin FD acquisition windows.
#[expect(clippy::disallowed_methods, reason = "sole process spawn boundary")]
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    #[cfg(target_os = "macos")]
    let _guard = acquire()?;
    command.spawn()
}

/// Waits after releasing the launch gate, preserving configured standard streams.
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    spawn(command)?.wait()
}

/// Sets output defaults before callers apply their own stream configuration.
/// Collect with `spawn(&mut command)?.wait_with_output()`.
pub fn capture_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Replaces this process while excluding Darwin FD acquisition windows.
#[cfg(unix)]
#[expect(clippy::disallowed_methods, reason = "sole process exec boundary")]
pub fn exec(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "macos")]
    let _guard = match acquire() {
        Ok(guard) => guard,
        Err(error) => return error,
    };
    command.exec()
}

/// Runs a Windows replacement command and exits with its status, as the CLI expects.
#[cfg(windows)]
pub fn exec(command: &mut Command) -> io::Error {
    match status(command) {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "macos"))]
mod test_hooks {
    use super::*;
    use std::cell::RefCell;
    use std::os::fd::RawFd;
    use std::sync::{MutexGuard, TryLockError};

    type AcquireObserver = Box<dyn FnOnce(bool)>;
    type WindowObserver = Box<dyn FnOnce(&[RawFd])>;
    thread_local! {
        static ACQUIRE: RefCell<Option<AcquireObserver>> = const { RefCell::new(None) };
        static WINDOW: RefCell<Option<WindowObserver>> = const { RefCell::new(None) };
    }

    pub(super) fn on_acquire(observer: impl FnOnce(bool) + 'static) {
        ACQUIRE.with(|slot| assert!(slot.borrow_mut().replace(Box::new(observer)).is_none()));
    }

    pub(super) fn on_window(observer: impl FnOnce(&[RawFd]) + 'static) {
        WINDOW.with(|slot| assert!(slot.borrow_mut().replace(Box::new(observer)).is_none()));
    }

    pub(super) fn observe_acquire() -> Option<io::Result<MutexGuard<'static, ()>>> {
        let observer = ACQUIRE.with(|slot| slot.borrow_mut().take())?;
        match LAUNCH_GATE.try_lock() {
            Ok(guard) => {
                observer(false);
                Some(Ok(guard))
            }
            Err(TryLockError::WouldBlock) => {
                observer(true);
                None
            }
            Err(TryLockError::Poisoned(_)) => {
                Some(Err(io::Error::other("process launch gate poisoned")))
            }
        }
    }

    pub(super) fn window(descriptors: &[RawFd]) {
        if let Some(observer) = WINDOW.with(|slot| slot.borrow_mut().take()) {
            observer(descriptors);
        }
    }
}
