// SPDX-License-Identifier: MIT
//! Inactivity timing for a confirmed empty agent prompt.
//!
//! A caller must obtain a fresh observation before every decision. Terminal
//! silence alone is not an `Empty` observation; an unknown prompt fails closed.

use std::num::{NonZeroU32, NonZeroU64};
use std::time::{Duration, Instant};

/// Identifies one live pane and the activity visible when its prompt was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyPrompt {
    pub pane_id: NonZeroU64,
    pub child_pid: NonZeroU32,
    pub input_revision: u64,
    pub output_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptObservation {
    Empty(EmptyPrompt),
    Busy,
    Unknown,
}

/// Measures a continuous run of matching, positively identified empty prompts.
/// This type never writes input, reads mail or decides whether work is queued.
#[derive(Debug, Default)]
pub struct PromptIdleTracker {
    empty: Option<(EmptyPrompt, Instant)>,
    last_observed: Option<Instant>,
}

impl PromptIdleTracker {
    /// Returns the continuous idle duration only for a fresh empty observation.
    /// Activity, a replacement process, or loss of readiness restarts the timer.
    pub fn observe(&mut self, observation: PromptObservation, now: Instant) -> Option<Duration> {
        if self.last_observed.is_some_and(|last| now < last) {
            self.empty = None;
        }
        self.last_observed = Some(now);
        let PromptObservation::Empty(prompt) = observation else {
            self.empty = None;
            return None;
        };
        let since = match self.empty {
            Some((previous, since)) if previous == prompt => since,
            _ => now,
        };
        self.empty = Some((prompt, since));
        Some(now.duration_since(since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> EmptyPrompt {
        EmptyPrompt {
            pane_id: NonZeroU64::new(1).unwrap(),
            child_pid: NonZeroU32::new(100).unwrap(),
            input_revision: 4,
            output_revision: 9,
        }
    }

    #[test]
    fn only_confirmed_empty_time_counts_toward_the_threshold() {
        let mut tracker = PromptIdleTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.observe(PromptObservation::Unknown, start), None);
        let ready = start + Duration::from_secs(3600);
        assert_eq!(
            tracker.observe(PromptObservation::Empty(prompt()), ready),
            Some(Duration::ZERO)
        );
        assert_eq!(
            tracker.observe(
                PromptObservation::Empty(prompt()),
                ready + Duration::from_secs(599)
            ),
            Some(Duration::from_secs(599))
        );
        assert_eq!(
            tracker.observe(
                PromptObservation::Empty(prompt()),
                ready + Duration::from_secs(600)
            ),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn busy_or_unknown_breaks_continuous_readiness() {
        for interruption in [PromptObservation::Busy, PromptObservation::Unknown] {
            let mut tracker = PromptIdleTracker::default();
            let start = Instant::now();
            tracker.observe(PromptObservation::Empty(prompt()), start);
            assert_eq!(
                tracker.observe(interruption, start + Duration::from_secs(599)),
                None
            );
            assert_eq!(
                tracker.observe(
                    PromptObservation::Empty(prompt()),
                    start + Duration::from_secs(600)
                ),
                Some(Duration::ZERO)
            );
        }
    }

    #[test]
    fn input_or_output_activity_restarts_the_timer() {
        for changed in [
            EmptyPrompt {
                input_revision: 5,
                ..prompt()
            },
            EmptyPrompt {
                output_revision: 10,
                ..prompt()
            },
        ] {
            let mut tracker = PromptIdleTracker::default();
            let start = Instant::now();
            tracker.observe(PromptObservation::Empty(prompt()), start);
            assert_eq!(
                tracker.observe(
                    PromptObservation::Empty(changed),
                    start + Duration::from_secs(600)
                ),
                Some(Duration::ZERO)
            );
        }
    }

    #[test]
    fn replacement_pane_or_process_does_not_inherit_idle_time() {
        for replacement in [
            EmptyPrompt {
                pane_id: NonZeroU64::new(2).unwrap(),
                ..prompt()
            },
            EmptyPrompt {
                child_pid: NonZeroU32::new(101).unwrap(),
                ..prompt()
            },
        ] {
            let mut tracker = PromptIdleTracker::default();
            let start = Instant::now();
            tracker.observe(PromptObservation::Empty(prompt()), start);
            assert_eq!(
                tracker.observe(
                    PromptObservation::Empty(replacement),
                    start + Duration::from_secs(600)
                ),
                Some(Duration::ZERO)
            );
        }
    }

    #[test]
    fn out_of_order_observations_restart_instead_of_extending_idle_time() {
        let mut tracker = PromptIdleTracker::default();
        let start = Instant::now();
        tracker.observe(PromptObservation::Empty(prompt()), start);
        tracker.observe(
            PromptObservation::Empty(prompt()),
            start + Duration::from_secs(600),
        );
        assert_eq!(
            tracker.observe(
                PromptObservation::Empty(prompt()),
                start + Duration::from_secs(300)
            ),
            Some(Duration::ZERO)
        );
    }
}
