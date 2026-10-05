//! Status toast visibility and history (#171).
//!
//! Every status message is recorded here whether it shows or not, so
//! `toasts = "errors"` or `"off"` never loses one: Recent messages lists
//! them. Bell, paste, write-fail, and drag chips are not status toasts.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::ToastLevel;

/// `Info` confirms or reports progress. `Error` means a requested action did
/// not happen or something broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

/// Whether `level` shows a toast of `kind`.
pub fn shows(level: ToastLevel, kind: ToastKind) -> bool {
    match level {
        ToastLevel::All => true,
        ToastLevel::Errors => kind == ToastKind::Error,
        ToastLevel::Off => false,
    }
}

/// Whether a live chip survives a switch to `level`. `None` is a chip that
/// is not a status toast (bell, paste, write-fail) and always stays.
pub fn keeps_chip(level: ToastLevel, kind: Option<ToastKind>) -> bool {
    kind.is_none_or(|kind| shows(level, kind))
}

/// Messages kept per window; older ones drop off.
pub const HISTORY_CAP: usize = 100;

#[derive(Debug, Clone)]
pub struct Entry {
    pub at: Instant,
    pub kind: ToastKind,
    pub text: String,
    pub shown: bool,
}

#[derive(Debug, Default)]
pub struct History {
    entries: VecDeque<Entry>,
}

impl History {
    /// Keep `text` (chip padding trimmed) and return whether `level` shows
    /// it. A blank message is neither kept nor shown.
    pub fn record(&mut self, level: ToastLevel, kind: ToastKind, text: &str, at: Instant) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let shown = shows(level, kind);
        if self.entries.len() == HISTORY_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            at,
            kind,
            text: text.to_string(),
            shown,
        });
        shown
    }

    pub fn newest_first(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }

    /// Messages the level kept off screen.
    #[cfg(test)]
    pub fn hidden(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.shown).count()
    }
}

/// Recent messages rows, newest first: the message, then its age, `error`
/// for failures, and `hidden` when no toast showed.
pub fn rows(history: &History, now: Instant) -> Vec<(String, String)> {
    history
        .newest_first()
        .map(|entry| {
            let mut detail = age(now.saturating_duration_since(entry.at));
            if entry.kind == ToastKind::Error {
                detail.push_str(" · error");
            }
            if !entry.shown {
                detail.push_str(" · hidden");
            }
            (entry.text.clone(), detail)
        })
        .collect()
}

fn age(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=9 => "just now".into(),
        10..=59 => format!("{secs}s ago"),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToastLevel;
    use std::time::{Duration, Instant};

    #[test]
    fn each_level_shows_the_right_kinds() {
        assert!(shows(ToastLevel::All, ToastKind::Info));
        assert!(shows(ToastLevel::All, ToastKind::Error));
        assert!(!shows(ToastLevel::Errors, ToastKind::Info));
        assert!(shows(ToastLevel::Errors, ToastKind::Error));
        assert!(!shows(ToastLevel::Off, ToastKind::Info));
        assert!(!shows(ToastLevel::Off, ToastKind::Error));
    }

    #[test]
    fn every_message_is_recorded_whether_or_not_it_shows() {
        let now = Instant::now();
        let mut history = History::default();
        let info = " cairn: view applied; 3 reused sessions (live layouts retained) ";
        assert!(history.record(ToastLevel::All, ToastKind::Info, info, now));
        assert!(!history.record(ToastLevel::Errors, ToastKind::Info, " Saved ", now));
        assert!(history.record(
            ToastLevel::Errors,
            ToastKind::Error,
            " move failed: gone ",
            now
        ));
        assert!(!history.record(ToastLevel::Off, ToastKind::Error, "Could not undo", now));
        let texts: Vec<_> = history
            .newest_first()
            .map(|entry| (entry.text.as_str(), entry.kind, entry.shown))
            .collect();
        assert_eq!(
            texts,
            [
                ("Could not undo", ToastKind::Error, false),
                ("move failed: gone", ToastKind::Error, true),
                ("Saved", ToastKind::Info, false),
                (
                    "cairn: view applied; 3 reused sessions (live layouts retained)",
                    ToastKind::Info,
                    true
                ),
            ],
            "chip padding is trimmed; newest first"
        );
        assert_eq!(history.hidden(), 2);
    }

    #[test]
    fn history_keeps_the_newest_entries_up_to_the_cap() {
        let now = Instant::now();
        let mut history = History::default();
        for n in 0..HISTORY_CAP + 5 {
            history.record(ToastLevel::Off, ToastKind::Info, &format!("m{n}"), now);
        }
        assert_eq!(history.newest_first().count(), HISTORY_CAP);
        assert_eq!(
            history.newest_first().next().unwrap().text,
            format!("m{}", HISTORY_CAP + 4)
        );
        assert_eq!(history.newest_first().last().unwrap().text, "m5");
    }

    #[test]
    fn blank_messages_are_not_recorded_or_shown() {
        let mut history = History::default();
        assert!(!history.record(ToastLevel::All, ToastKind::Info, "   ", Instant::now()));
        assert_eq!(history.newest_first().count(), 0);
    }

    #[test]
    fn rows_say_how_long_ago_and_whether_the_toast_was_hidden() {
        let start = Instant::now();
        let mut history = History::default();
        history.record(
            ToastLevel::All,
            ToastKind::Info,
            "opening space cairn",
            start,
        );
        history.record(
            ToastLevel::Errors,
            ToastKind::Info,
            "cairn: view applied",
            start + Duration::from_secs(30),
        );
        history.record(
            ToastLevel::Off,
            ToastKind::Error,
            "Save failed",
            start + Duration::from_secs(60),
        );
        let now = start + Duration::from_secs(3 * 3600 + 65);
        assert_eq!(
            rows(&history, now),
            [
                (
                    "Save failed".to_string(),
                    "3h ago · error · hidden".to_string()
                ),
                (
                    "cairn: view applied".to_string(),
                    "3h ago · hidden".to_string()
                ),
                ("opening space cairn".to_string(), "3h ago".to_string()),
            ]
        );
        assert_eq!(age(Duration::from_secs(4)), "just now");
        assert_eq!(age(Duration::from_secs(45)), "45s ago");
        assert_eq!(age(Duration::from_secs(150)), "2m ago");
        assert_eq!(age(Duration::from_secs(7200)), "2h ago");
        assert_eq!(age(Duration::from_secs(3 * 86_400)), "3d ago");
    }

    /// Picking a stricter level drops the status chips it would hide; bell,
    /// paste, and write-fail chips (no kind) stay.
    #[test]
    fn settle_keeps_only_chips_the_new_level_shows() {
        let kinds = [None, Some(ToastKind::Info), Some(ToastKind::Error)];
        let kept = |level| {
            kinds
                .iter()
                .filter(|kind| keeps_chip(level, **kind))
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(kept(ToastLevel::All), kinds);
        assert_eq!(kept(ToastLevel::Errors), [None, Some(ToastKind::Error)]);
        assert_eq!(kept(ToastLevel::Off), [None]);
    }
}
