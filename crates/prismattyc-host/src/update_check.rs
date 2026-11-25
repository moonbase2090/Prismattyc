// SPDX-License-Identifier: MIT
//! Background release check shared by every platform.
//!
//! The host asks `pmux update --check --json` whether a newer signed release
//! exists. This module only reads that answer; installing stays with the
//! explicit update flow, so a check never stops a session.

use std::process::Stdio;
use std::thread;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use winit::event_loop::EventLoopProxy;

use crate::{find_mux_bin, update_notes, UserAction};

#[derive(Debug, Deserialize)]
pub(super) struct ReleaseCheck {
    pub available: String,
    pub update_available: bool,
    #[serde(default)]
    pub release_notes: String,
}

pub(super) fn start_check(proxy: EventLoopProxy<UserAction>) {
    thread::spawn(move || {
        let result = check_release().map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserAction::UpdateCheckFinished { result });
    });
}

fn check_release() -> Result<ReleaseCheck> {
    let pmux = find_mux_bin();
    let args = ["update", "--check", "--json"];
    let output = prismattyc_process::spawn(
        prismattyc_process::capture_command(&pmux)
            .args(args)
            .stdin(Stdio::null()),
    )
    .and_then(std::process::Child::wait_with_output)
    .with_context(|| format!("run {} {}", pmux.display(), args.join(" ")))?;
    ensure!(output.status.success(), "{}", command_error(&output));
    parse(&output.stdout)
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

fn parse(stdout: &[u8]) -> Result<ReleaseCheck> {
    serde_json::from_slice(stdout).context("parse pmux update check")
}

/// Remembers which release the user was already told about, so the daily
/// check does not repeat the same notice while the app stays open.
#[derive(Debug, Default)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) struct Announced(Option<String>);

#[cfg_attr(target_os = "macos", allow(dead_code))]
impl Announced {
    /// The notice to show for `check`, or `None` when no update exists or
    /// this version was already announced. A check that finishes after the
    /// user turned automatic checks off is dropped without being recorded, so
    /// turning them back on still announces the release.
    pub fn notice(&mut self, check: &ReleaseCheck, enabled: bool) -> Option<String> {
        if !enabled
            || !check.update_available
            || self.0.as_deref() == Some(check.available.as_str())
        {
            return None;
        }
        self.0 = Some(check.available.clone());
        Some(notice_text(&check.available, &check.release_notes))
    }
}

/// Characters of the release's top change a one-line notice keeps.
const NOTICE_HIGHLIGHT_CHARS: usize = 48;

/// One line: the version, the release's top change, and where the full
/// notes are. The click opens the review, which links the release notes.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn notice_text(version: &str, release_notes: &str) -> String {
    const TAIL: &str = "Click for the update and release notes.";
    let top = update_notes::highlights(release_notes, 1, NOTICE_HIGHLIGHT_CHARS);
    match top.items.first() {
        Some(change) if change.ends_with('…') => {
            format!("Prismattyc {version} is available: {change} {TAIL}")
        }
        Some(change) => format!("Prismattyc {version} is available: {change}. {TAIL}"),
        None => format!("Prismattyc {version} is available. {TAIL}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(json: &str) -> ReleaseCheck {
        parse(json.as_bytes()).expect("parse")
    }

    #[test]
    fn a_new_release_is_announced_once_per_version() {
        let mut seen = Announced::default();
        let first = check(r#"{"available":"0.4.0","update_available":true}"#);
        let notice = seen.notice(&first, true).expect("first announcement");
        assert!(notice.contains("0.4.0"), "{notice}");
        assert_eq!(
            seen.notice(&first, true),
            None,
            "the daily recheck stays quiet"
        );
        let newer = check(r#"{"available":"0.4.1","update_available":true}"#);
        assert!(seen.notice(&newer, true).expect("newer").contains("0.4.1"));
    }

    #[test]
    fn the_notice_names_the_top_change_and_where_the_notes_are() {
        let mut seen = Announced::default();
        let release = ReleaseCheck {
            available: "0.4.0".into(),
            update_available: true,
            release_notes: "## [0.4.0]\n\n### Added\n\n- **Faster** splits (all layouts). More detail.\n- Other.\n".into(),
        };
        assert_eq!(
            seen.notice(&release, true).as_deref(),
            Some("Prismattyc 0.4.0 is available: Faster splits. Click for the update and release notes.")
        );
        let bare = check(r#"{"available":"0.4.1","update_available":true}"#);
        assert_eq!(
            seen.notice(&bare, true).as_deref(),
            Some("Prismattyc 0.4.1 is available. Click for the update and release notes.")
        );
    }

    #[test]
    fn a_full_changelog_still_gives_a_one_line_notice() {
        let notes = include_str!("update_notes_0_3_73.md");
        let release = ReleaseCheck {
            available: "0.3.73".into(),
            update_available: true,
            release_notes: notes.into(),
        };
        let notice = Announced::default().notice(&release, true).expect("notice");
        assert_eq!(
            notice,
            "Prismattyc 0.3.73 is available: Experimental native terminal grids and uploaded… Click for the update and release notes."
        );
    }

    #[test]
    fn an_up_to_date_install_is_never_announced() {
        let mut seen = Announced::default();
        let current = check(r#"{"available":"0.4.0","update_available":false}"#);
        assert_eq!(seen.notice(&current, true), None);
        let later = check(r#"{"available":"0.4.0","update_available":true}"#);
        assert!(
            seen.notice(&later, true).is_some(),
            "a skipped quiet check must not hide a later release"
        );
    }

    #[test]
    fn a_check_finishing_after_checks_were_turned_off_stays_quiet() {
        let mut seen = Announced::default();
        let pending = check(r#"{"available":"0.4.0","update_available":true}"#);
        assert_eq!(seen.notice(&pending, false), None);
        assert!(
            seen.notice(&pending, true).is_some(),
            "a dropped result must not count as announced"
        );
    }

    #[test]
    fn the_check_reads_what_pmux_update_prints() {
        let parsed = check(
            r#"{"installed":"0.3.67","available":"0.4.0","update_available":true,"release_notes":"Fixes","extra":1}"#,
        );
        assert_eq!(parsed.available, "0.4.0");
        assert!(parsed.update_available);
        assert!(parse(b"not json").is_err());
    }
}
