//! The `activity_monitor` action: open, or focus, the Activity Monitor Space.
//!
//! `pmux space monitor` creates the Space the first time, with one session
//! running the monitor, and starts the monitor again when its session's shell
//! is idle. It runs in the background; when it finishes, the host switches to
//! the Space the same way the Space rail does.

use std::io::Read;
use std::process::{Child, Stdio};

use crate::HostState;

pub(crate) const NAME: &str = "Activity Monitor";

/// The helper in flight, if any.
#[derive(Default)]
pub(crate) struct Pending(Option<Child>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn open(host: &mut HostState) {
    if host.activity_monitor.0.is_some() {
        crate::rail_toast(host, &format!(" opening {NAME} "));
        return;
    }
    match crate::pmux_command()
        .args(["space", "monitor"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => {
            host.activity_monitor.0 = Some(child);
            crate::rail_toast(host, &format!(" opening {NAME} "));
        }
        Err(error) => failed(host, &format!("could not run pmux: {error}")),
    }
}

/// Called from the host loop. Switches to the Space once the helper is done.
pub(crate) fn poll(host: &mut HostState) {
    let Some(child) = host.activity_monitor.0.as_mut() else {
        return;
    };
    let status = match child.try_wait() {
        Ok(None) => return,
        Ok(Some(status)) => Ok(status),
        Err(error) => Err(error),
    };
    let mut child = host.activity_monitor.0.take().expect("pending helper");
    match status {
        Ok(status) if status.success() => {
            crate::open_space_from_host(host, NAME, crate::SpaceOpenMode::Switch);
            host.dirty = true;
        }
        Ok(_) => {
            let mut message = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.by_ref().take(4096).read_to_string(&mut message);
            }
            let message = message.trim();
            let message = message.strip_prefix("Error: ").unwrap_or(message);
            failed(
                host,
                if message.is_empty() {
                    "pmux space monitor failed"
                } else {
                    message
                },
            );
        }
        Err(error) => failed(host, &format!("pmux space monitor: {error}")),
    }
    // Already exited unless try_wait failed; this reaps it either way.
    let _ = child.kill();
    let _ = child.wait();
}

fn failed(host: &mut HostState, detail: &str) {
    crate::rail_error_toast(host, &format!(" {NAME} did not open: {detail} "));
}
