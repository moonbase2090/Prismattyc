// SPDX-License-Identifier: MIT
//! The progress meter shown while Update and restart runs.
//!
//! The release download reports bytes of a known total, so its bar is exact.
//! Verify and restart report "n of m" items, and the bar counts finished
//! items. Installing has no measurable fraction, so its bar moves without a
//! percentage instead of showing a made-up one.
use prismattyc_mux::release_update::progress::Progress;
use serde::{Deserialize, Serialize};

/// A progress bar: filled to a fraction, or a block that sweeps across it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MeterBar {
    /// Filled to this many thousandths.
    Fraction(u16),
    /// No measurable fraction: a block sweeps across the track, one step per frame.
    Moving(usize),
}

/// Steps for a moving bar to cross its track one way.
pub(super) const METER_SWEEP: usize = 24;

/// The filled part of a track `width` pixels wide, as (offset, length).
pub(super) fn meter_fill(width: usize, bar: MeterBar) -> (usize, usize) {
    match bar {
        MeterBar::Fraction(permille) => (
            0,
            width.saturating_mul(usize::from(permille.min(1000))) / 1000,
        ),
        MeterBar::Moving(frame) => {
            let block = (width / 5).max(1).min(width);
            let travel = width - block;
            let step = frame % (2 * METER_SWEEP);
            let step = if step <= METER_SWEEP {
                step
            } else {
                2 * METER_SWEEP - step
            };
            (travel * step / METER_SWEEP, block)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Amount {
    Bytes { received: u64, total: u64 },
    Items { index: usize, count: usize },
    Unknown,
}

/// The meter's state travels with the relaunch, so the new app continues
/// the same steps and ends on the same receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Meter {
    steps: Vec<Step>,
    step: usize,
    detail: String,
    amount: Amount,
    failed: bool,
    /// Every step has finished.
    #[serde(default)]
    finished: bool,
    /// Moves the bar of a step without a fraction.
    #[serde(skip)]
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
        // The update first checks that the sessions are the ones the user
        // agreed to restart.
        let detail = "Checking the sessions";
        Meter {
            steps,
            step: 0,
            detail: detail.into(),
            amount: Amount::Unknown,
            failed: false,
            finished: false,
            frame: 0,
        }
    }

    fn enter(&mut self, step: Step) {
        if let Some(index) = self.steps.iter().position(|s| *s == step) {
            self.step = index;
        }
    }

    /// The current step's number from 1 and the number of steps.
    pub(super) fn position(&self) -> (usize, usize) {
        (self.step + 1, self.steps.len())
    }

    pub(super) fn current(&self) -> Step {
        self.steps[self.step]
    }

    pub(super) fn apply(&mut self, progress: &Progress) {
        if self.failed || self.finished {
            return;
        }
        // The command line prints the same words for each event.
        self.detail = progress.describe();
        match progress {
            Progress::Download {
                received, total, ..
            } => {
                self.enter(Step::Download);
                self.amount = Amount::Bytes {
                    received: *received,
                    total: *total,
                };
            }
            Progress::Verify { index, count, .. } => {
                self.enter(Step::Verify);
                self.amount = Amount::Items {
                    index: *index,
                    count: *count,
                };
            }
            Progress::Install { .. } => {
                self.enter(Step::Install);
                self.amount = Amount::Unknown;
            }
            Progress::Restart { index, count, .. } => {
                self.enter(Step::Restart);
                self.amount = Amount::Items {
                    index: *index,
                    count: *count,
                };
            }
        }
    }

    /// Keep the bar at the step that failed.
    pub(super) fn fail(&mut self) {
        self.failed = true;
    }

    /// Every step has finished: the bar is full.
    pub(super) fn finish(&mut self) {
        if !self.failed {
            self.finished = true;
        }
    }

    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    /// One line for each step: done, failed with what it was doing, under
    /// way, or not started.
    pub(super) fn receipt(&self) -> Vec<String> {
        self.steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let state = if index < self.step || (index == self.step && self.finished) {
                    "done".to_string()
                } else if index > self.step {
                    "not started".to_string()
                } else if self.failed {
                    format!("failed at {}", self.detail)
                } else {
                    format!("under way: {}", self.detail)
                };
                format!("{}: {state}", step.name())
            })
            .collect()
    }

    /// Advance the moving bar; true when the meter has one to move.
    pub(super) fn tick(&mut self) -> bool {
        if self.failed || self.finished || self.amount != Amount::Unknown {
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
            } else if self.finished {
                "Finished step"
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
            _ if self.finished => Some(1.0),
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

    fn restart(index: usize, count: usize, item: &str) -> Progress {
        Progress::Restart {
            item: item.into(),
            index,
            count,
        }
    }

    #[test]
    fn restart_only_has_a_single_step() {
        let mut meter = Meter::new(false);
        meter.apply(&restart(2, 3, "Restarting the MCP adapter"));
        assert_eq!(
            meter.lines(),
            [
                "Step 1 of 1: Restart",
                "Restarting the MCP adapter (2 of 3)"
            ]
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
        meter.apply(&restart(1, 3, "Stopping the old daemon"));
        assert!(!meter.tick());
        assert_eq!(
            meter.lines(),
            ["Failed at step 2 of 4: Verify", "Verifying 2 of 6: pmuxd"]
        );
        assert_eq!(meter.bar(), (MeterBar::Fraction(166), "16%".into()));
    }

    #[test]
    fn the_receipt_names_each_steps_outcome() {
        let mut meter = Meter::new(true);
        meter.apply(&Progress::Verify {
            item: "pmuxd".into(),
            index: 2,
            count: 6,
        });
        assert_eq!(
            meter.receipt(),
            [
                "Download: done",
                "Verify: under way: Verifying 2 of 6: pmuxd",
                "Install: not started",
                "Restart: not started"
            ]
        );
        meter.fail();
        meter.finish();
        assert!(!meter.finished(), "a failed update cannot finish");
        assert_eq!(
            meter.receipt()[1],
            "Verify: failed at Verifying 2 of 6: pmuxd"
        );
    }

    #[test]
    fn a_failure_before_any_stage_names_the_session_check() {
        let mut meter = Meter::new(false);
        meter.fail();
        assert_eq!(
            meter.lines(),
            ["Failed at step 1 of 1: Restart", "Checking the sessions"]
        );
        assert_eq!(
            meter.receipt(),
            ["Restart: failed at Checking the sessions"]
        );
    }

    #[test]
    fn a_finished_meter_is_full_and_still() {
        let mut meter = Meter::new(false);
        meter.apply(&restart(3, 6, "Starting the new daemon"));
        meter.finish();
        assert_eq!(meter.bar(), (MeterBar::Fraction(1000), "100%".into()));
        assert!(!meter.tick());
        meter.apply(&restart(4, 6, "Reconciling the official plug-ins"));
        assert_eq!(
            meter.lines(),
            [
                "Finished step 1 of 1: Restart",
                "Starting the new daemon (3 of 6)"
            ],
            "events after the finish do not move it"
        );
        assert_eq!(meter.receipt(), ["Restart: done"]);
    }

    #[test]
    fn the_note_warns_when_sessions_stop() {
        let mut meter = Meter::new(true);
        assert_eq!(
            meter.note(),
            "Your sessions keep running until the restart step."
        );
        meter.apply(&restart(1, 3, "Saving the sessions"));
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
    fn a_meter_fills_its_fraction_of_the_track() {
        assert_eq!(meter_fill(200, MeterBar::Fraction(0)), (0, 0));
        assert_eq!(meter_fill(200, MeterBar::Fraction(250)), (0, 50));
        assert_eq!(meter_fill(200, MeterBar::Fraction(1000)), (0, 200));
        assert_eq!(meter_fill(200, MeterBar::Fraction(1400)), (0, 200));
    }

    #[test]
    fn a_moving_meter_sweeps_inside_its_track_without_jumping() {
        let mut previous = None;
        for frame in 0..(4 * METER_SWEEP) {
            let (start, length) = meter_fill(200, MeterBar::Moving(frame));
            assert_eq!(length, 40);
            assert!(start + length <= 200, "frame {frame}: {start}");
            if let Some(before) = previous {
                let moved = (start as isize - before as isize).unsigned_abs();
                assert!(
                    moved > 0 && moved <= 7,
                    "frame {frame}: {before} -> {start}"
                );
            }
            previous = Some(start);
        }
        assert_eq!(meter_fill(200, MeterBar::Moving(0)).0, 0);
        assert_eq!(meter_fill(200, MeterBar::Moving(METER_SWEEP)).0, 160);
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
