// SPDX-License-Identifier: MIT
//! The progress meter shown while Update and restart runs.
//!
//! The release download reports bytes of a known total, so its bar is exact.
//! Verify and restart report "n of m" items, and the bar counts finished
//! items. Installing has no measurable fraction, so its bar moves without a
//! percentage instead of showing a made-up one.
use crate::palette::MeterBar;
use prismattyc_mux::release_update::progress::Progress;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Download,
    Verify,
    Install,
    Restart,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Download => "Download",
            Step::Verify => "Verify",
            Step::Install => "Install",
            Step::Restart => "Restart",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Amount {
    Bytes { received: u64, total: u64 },
    Items { index: usize, count: usize },
    Unknown,
}

#[derive(Clone, Debug)]
pub(super) struct Meter {
    steps: Vec<Step>,
    step: usize,
    detail: String,
    amount: Amount,
    failed: bool,
    /// Moves the bar of a step without a fraction.
    frame: usize,
}

const MB: f64 = 1024.0 * 1024.0;

impl Meter {
    /// A meter for a full update, or for a restart on the installed version.
    pub(super) fn new(update_available: bool) -> Self {
        let steps = if update_available {
            vec![Step::Download, Step::Verify, Step::Install, Step::Restart]
        } else {
            vec![Step::Restart]
        };
        let detail = if update_available {
            "Starting the download"
        } else {
            "Preparing to restart"
        };
        Meter {
            steps,
            step: 0,
            detail: detail.into(),
            amount: Amount::Unknown,
            failed: false,
            frame: 0,
        }
    }

    fn enter(&mut self, step: Step) {
        if let Some(index) = self.steps.iter().position(|s| *s == step) {
            self.step = index;
        }
    }

    pub(super) fn current(&self) -> Step {
        self.steps[self.step]
    }

    pub(super) fn apply(&mut self, progress: &Progress) {
        if self.failed {
            return;
        }
        match progress {
            Progress::Download {
                item,
                received,
                total,
            } => {
                self.enter(Step::Download);
                self.detail = format!("Downloading {item}");
                self.amount = Amount::Bytes {
                    received: *received,
                    total: *total,
                };
            }
            Progress::Verify { item, index, count } => {
                self.enter(Step::Verify);
                self.detail = format!("Verifying {index} of {count}: {item}");
                self.amount = Amount::Items {
                    index: *index,
                    count: *count,
                };
            }
            Progress::Install { item } => {
                self.enter(Step::Install);
                self.detail = format!("Installing {item}");
                self.amount = Amount::Unknown;
            }
        }
    }

    /// A restart of one component, reported by the app itself.
    pub(super) fn restart(&mut self, index: usize, count: usize, item: &str) {
        if self.failed {
            return;
        }
        self.enter(Step::Restart);
        self.detail = format!("Restarting {index} of {count}: {item}");
        self.amount = Amount::Items { index, count };
    }

    /// Keep the bar at the step that failed.
    pub(super) fn fail(&mut self) {
        self.failed = true;
    }

    /// Advance the moving bar; true when the meter has one to move.
    pub(super) fn tick(&mut self) -> bool {
        if self.failed || self.amount != Amount::Unknown {
            return false;
        }
        self.frame = self.frame.wrapping_add(1);
        true
    }

    /// What happens to the user's sessions at this point.
    pub(super) fn note(&self) -> &'static str {
        if self.current() == Step::Restart {
            "Stopping the running sessions and restarting each component."
        } else {
            "Your sessions keep running until the restart step."
        }
    }

    /// The heading and the step's item, without byte counts.
    pub(super) fn step_line(&self) -> String {
        format!("{} | {}", self.lines()[0], self.detail)
    }

    /// The heading and the current step's detail, as panel lines.
    pub(super) fn lines(&self) -> Vec<String> {
        let heading = format!(
            "{} {} of {}: {}",
            if self.failed {
                "Failed at step"
            } else {
                "Step"
            },
            self.step + 1,
            self.steps.len(),
            self.current().name()
        );
        let detail = match self.amount {
            Amount::Bytes { received, total } => format!(
                "{}: {:.1} of {:.1} MB",
                self.detail,
                received as f64 / MB,
                total as f64 / MB
            ),
            _ => self.detail.clone(),
        };
        vec![heading, detail]
    }

    /// The bar and the text beside it: a percentage when the step has a
    /// fraction, nothing when it does not.
    pub(super) fn bar(&self) -> (MeterBar, String) {
        let fraction = match self.amount {
            Amount::Bytes { received, total } if total > 0 => {
                Some(received.min(total) as f64 / total as f64)
            }
            Amount::Items { index, count } if count > 0 => {
                // Items before this one are done; this one is under way.
                Some(index.saturating_sub(1).min(count) as f64 / count as f64)
            }
            _ => None,
        };
        match fraction {
            Some(fraction) => (
                MeterBar::Fraction((fraction * 1000.0).floor() as u16),
                format!("{}%", (fraction * 100.0).floor() as usize),
            ),
            None => (MeterBar::Moving(self.frame), String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_shows_bytes_of_the_total_and_a_matching_bar() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Download {
            item: "pmuxd".into(),
            received: 5 * 1024 * 1024,
            total: 20 * 1024 * 1024,
        });
        assert_eq!(
            meter.lines(),
            ["Step 1 of 4: Download", "Downloading pmuxd: 5.0 of 20.0 MB"]
        );
        assert_eq!(meter.bar(), (MeterBar::Fraction(250), "25%".into()));
    }

    #[test]
    fn verify_counts_finished_items_and_names_the_current_one() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Verify {
            item: "pmux-attach".into(),
            index: 3,
            count: 4,
        });
        assert_eq!(
            meter.lines(),
            ["Step 2 of 4: Verify", "Verifying 3 of 4: pmux-attach"]
        );
        assert_eq!(meter.bar(), (MeterBar::Fraction(500), "50%".into()));
    }

    #[test]
    fn install_moves_without_a_percentage() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Install {
            item: "core binaries".into(),
        });
        assert_eq!(
            meter.lines(),
            ["Step 3 of 4: Install", "Installing core binaries"]
        );
        let (first, label) = meter.bar();
        assert_eq!(label, "", "no made-up percentage");
        assert!(matches!(first, MeterBar::Moving(_)));
        assert!(meter.tick());
        assert_ne!(meter.bar().0, first, "the bar moves on each tick");
    }

    #[test]
    fn restart_only_has_a_single_step() {
        let mut meter = Meter::new(false);
        meter.restart(2, 3, "MCP adapter");
        assert_eq!(
            meter.lines(),
            ["Step 1 of 1: Restart", "Restarting 2 of 3: MCP adapter"]
        );
        assert_eq!(meter.bar(), (MeterBar::Fraction(333), "33%".into()));
    }

    #[test]
    fn failure_holds_the_failed_step_and_ignores_later_events() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Verify {
            item: "pmuxd".into(),
            index: 2,
            count: 6,
        });
        meter.fail();
        meter.apply(&Progress::Install { item: "x".into() });
        meter.restart(1, 3, "daemon");
        assert!(!meter.tick());
        assert_eq!(
            meter.lines(),
            ["Failed at step 2 of 4: Verify", "Verifying 2 of 6: pmuxd"]
        );
        assert_eq!(meter.bar(), (MeterBar::Fraction(166), "16%".into()));
    }

    #[test]
    fn the_note_warns_when_sessions_stop() {
        let mut meter = Meter::new(true);
        assert_eq!(
            meter.note(),
            "Your sessions keep running until the restart step."
        );
        meter.restart(1, 3, "daemon and sessions");
        assert_eq!(
            meter.note(),
            "Stopping the running sessions and restarting each component."
        );
    }

    #[test]
    fn the_step_line_changes_with_the_item_but_not_the_bytes() {
        let mut meter = Meter::new(true);
        let download = |received| Progress::Download {
            item: "pmuxd".into(),
            received,
            total: 100,
        };
        meter.apply(&download(10));
        let first = meter.step_line();
        assert_eq!(first, "Step 1 of 4: Download | Downloading pmuxd");
        meter.apply(&download(20));
        assert_eq!(meter.step_line(), first);
        meter.apply(&Progress::Verify {
            item: "pmuxd".into(),
            index: 1,
            count: 6,
        });
        assert_eq!(
            meter.step_line(),
            "Step 2 of 4: Verify | Verifying 1 of 6: pmuxd"
        );
    }

    #[test]
    fn a_determinate_bar_does_not_tick() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Download {
            item: "pmux".into(),
            received: 1,
            total: 2,
        });
        assert!(!meter.tick());
    }

    #[test]
    fn a_full_download_reads_one_hundred_percent() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Download {
            item: "pmux".into(),
            received: 9,
            total: 2,
        });
        assert_eq!(meter.bar(), (MeterBar::Fraction(1000), "100%".into()));
    }
}
