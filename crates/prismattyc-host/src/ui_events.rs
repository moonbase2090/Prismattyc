// SPDX-License-Identifier: MIT
//! Opt-in change lines for native drivers.
//!
//! With `PRISMATTYC_UI_EVENTS=1`, the app prints one stderr line after a
//! presented frame whenever the update panel or the Restart to update dialog
//! is in a different state from the last line. A line names only states and
//! positions (which page, which row index is selected, the dialog's state and
//! step number), never row text or session names. A driver reads the app's
//! stderr and waits for these lines instead of polling or sleeping, then reads
//! the rendered rows from render status for its checks: before a line is
//! printed, the app publishes render status and waits for that write, so a
//! read after the line sees at least that frame's state. The line carries the
//! frame's dump sequence, so a driver can match it to a numbered frame
//! (`PRISMATTYC_DUMP_PRESENT` with `{seq}`). Nothing is printed by default.
use super::*;
use std::sync::OnceLock;

pub(super) const PREFIX: &str = "prismattyc-host: ui: ";

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PRISMATTYC_UI_EVENTS").is_some_and(|value| value == "1"))
}

/// What a driver can wait for: the update panel's page and selected row
/// index, and the dialog's state, selection and step.
pub(super) fn snapshot(host: &HostState) -> serde_json::Value {
    serde_json::json!({
        "panel": space_panel::update_state(host),
        "dialog": host.restart_modal.as_ref().map(restart_modal::RestartModal::state),
    })
}

/// The line for `snapshot` when it differs from `last`.
pub(super) fn line(
    last: &mut Option<serde_json::Value>,
    now: serde_json::Value,
    seq: u64,
) -> Option<String> {
    if last.as_ref() == Some(&now) {
        return None;
    }
    let text = format!(
        "{PREFIX}{}",
        serde_json::json!({"seq": seq, "unix_ms": prismattyc_mux::host_render_status::unix_ms(), "ui": now})
    );
    *last = Some(now);
    Some(text)
}

/// After a presented frame: keep the change line, if any, for `flush`.
pub(super) fn observe(host: &mut HostState) {
    if !enabled() {
        return;
    }
    let now = snapshot(host);
    if let Some(text) = line(&mut host.ui_events_last, now, host.dump_present_seq) {
        host.ui_events_pending.push(text);
    }
}

/// Publish render status, wait for it to be written, then print the kept
/// change lines, so a driver that reads render status after a line sees the
/// state the line announced.
pub(super) fn flush(app: &mut App) {
    if !app
        .windows
        .values()
        .any(|host| !host.ui_events_pending.is_empty())
    {
        return;
    }
    app.last_render_status = None;
    app.publish_render_status();
    if let Some((pid_path, _)) = app.registered_host.as_ref() {
        app.file_writer.handle().wait_for_render_status(pid_path);
    }
    for host in app.windows.values_mut() {
        for text in host.ui_events_pending.drain(..) {
            eprintln!("{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_printed_once_per_change() {
        let mut last = None;
        let first = serde_json::json!({"panel": {"selected": 1}, "dialog": null});
        let text = line(&mut last, first.clone(), 7).expect("first state is new");
        assert!(text.starts_with(PREFIX), "{text}");
        let parsed: serde_json::Value =
            serde_json::from_str(text.strip_prefix(PREFIX).unwrap()).unwrap();
        assert_eq!(parsed["seq"], 7);
        assert_eq!(parsed["ui"], first);
        assert_eq!(
            line(&mut last, first, 8),
            None,
            "unchanged state prints nothing"
        );
        let moved = serde_json::json!({"panel": {"selected": 2}, "dialog": null});
        assert!(
            line(&mut last, moved, 9).is_some(),
            "a new selection prints again"
        );
    }
}
