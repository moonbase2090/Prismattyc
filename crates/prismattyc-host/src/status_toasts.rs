//! Status toast visibility and history (#171).

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
        assert!(history.record(ToastLevel::Errors, ToastKind::Error, " move failed: gone ", now));
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
        history.record(ToastLevel::All, ToastKind::Info, "opening space cairn", start);
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
                ("Save failed".to_string(), "3h ago · error · hidden".to_string()),
                ("cairn: view applied".to_string(), "3h ago · hidden".to_string()),
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
