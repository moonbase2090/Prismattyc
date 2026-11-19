// SPDX-License-Identifier: MIT
//! The progress meter shown while Update and restart runs.
//!
//! The release download reports bytes of a known total, so its bar is exact.
//! Verify and restart report "n of m" items, and the bar counts finished
//! items. Installing has no measurable fraction, so its bar moves without a
//! percentage instead of showing a made-up one.
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

    /// The meter as panel lines, each at most `columns` cells wide.
    pub(super) fn lines(&self, columns: usize) -> Vec<String> {
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
        vec![heading, detail, self.bar(columns)]
    }

    fn bar(&self, columns: usize) -> String {
        // "[" + cells + "]" + " 100%" fits in the panel width.
        let width = columns.saturating_sub(7).max(4);
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
            Some(fraction) => {
                let filled = ((fraction * width as f64).floor() as usize).min(width);
                format!(
                    "[{}{}] {:>3}%",
                    "█".repeat(filled),
                    "░".repeat(width - filled),
                    (fraction * 100.0).floor() as usize
                )
            }
            None => {
                let block = (width / 5).max(2).min(width);
                let span = width - block + 1;
                // Sweep right, then back, so the block never jumps.
                let position = self.frame % (2 * span.max(1));
                let start = if position < span {
                    position
                } else {
                    2 * span - 1 - position
                };
                format!(
                    "[{}{}{}]",
                    "░".repeat(start),
                    "▓".repeat(block),
                    "░".repeat(width - block - start)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str) -> usize {
        text.chars().count()
    }

    #[test]
    fn download_shows_bytes_of_the_total_and_a_matching_bar() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Download {
            item: "pmuxd".into(),
            received: 5 * 1024 * 1024,
            total: 20 * 1024 * 1024,
        });
        let lines = meter.lines(47);
        assert_eq!(lines[0], "Step 1 of 4: Download");
        assert_eq!(lines[1], "Downloading pmuxd: 5.0 of 20.0 MB");
        assert_eq!(
            lines[2],
            format!("[{}{}]  25%", "█".repeat(10), "░".repeat(30))
        );
        assert_eq!(cells(&lines[2]), 47);
    }

    #[test]
    fn verify_counts_finished_items_and_names_the_current_one() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Verify {
            item: "pmux-attach".into(),
            index: 3,
            count: 4,
        });
        let lines = meter.lines(27);
        assert_eq!(lines[0], "Step 2 of 4: Verify");
        assert_eq!(lines[1], "Verifying 3 of 4: pmux-attach");
        assert_eq!(
            lines[2],
            format!("[{}{}]  50%", "█".repeat(10), "░".repeat(10))
        );
    }

    #[test]
    fn install_moves_without_a_percentage() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Install {
            item: "core binaries".into(),
        });
        let first = meter.lines(27);
        assert_eq!(first[0], "Step 3 of 4: Install");
        assert_eq!(first[1], "Installing core binaries");
        assert!(!first[2].contains('%'), "{}", first[2]);
        assert!(first[2].contains('▓'), "{}", first[2]);
        assert!(meter.tick());
        let second = meter.lines(27);
        assert_ne!(first[2], second[2], "the bar moves on each tick");
        assert_eq!(cells(&first[2]), cells(&second[2]));
    }

    #[test]
    fn the_moving_bar_stays_inside_its_track() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Install { item: "x".into() });
        for _ in 0..200 {
            let bar = &meter.lines(20)[2];
            assert_eq!(cells(bar), 15, "{bar}");
            assert_eq!(bar.matches('▓').count(), 2, "{bar}");
            meter.tick();
        }
    }

    #[test]
    fn restart_only_has_a_single_step() {
        let mut meter = Meter::new(false);
        meter.restart(2, 3, "MCP adapter");
        let lines = meter.lines(27);
        assert_eq!(lines[0], "Step 1 of 1: Restart");
        assert_eq!(lines[1], "Restarting 2 of 3: MCP adapter");
        assert!(lines[2].ends_with(" 33%"), "{}", lines[2]);
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
        let lines = meter.lines(27);
        assert_eq!(lines[0], "Failed at step 2 of 4: Verify");
        assert_eq!(lines[1], "Verifying 2 of 6: pmuxd");
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
    fn narrow_panels_still_fit() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Download {
            item: "pmux".into(),
            received: 2,
            total: 2,
        });
        let bar = &meter.lines(12)[2];
        assert_eq!(bar, &format!("[{}] 100%", "█".repeat(5)));
        assert!(cells(bar) <= 12);
    }
}
