//! One Windows process scan, off the UI thread, at most once a second.
//!
//! The pump used to call `CreateToolhelp32Snapshot` / `Process32NextW` on the
//! UI thread. A full walk per pid pegs a core while the window is idle. The
//! caller only reaps a finished job or starts the next one.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Minimum gap between scan starts.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub(crate) struct Slot<T> {
    next: Option<Instant>,
    job: Option<JoinHandle<T>>,
}

impl<T: Send + 'static> Slot<T> {
    /// The finished job's value. A running job returns `None` without waiting.
    pub(crate) fn reap(&mut self) -> Option<T> {
        if self.job.as_ref().is_some_and(JoinHandle::is_finished) {
            self.job.take().and_then(|job| job.join().ok())
        } else {
            None
        }
    }

    pub(crate) fn running(&self) -> bool {
        self.job.is_some()
    }

    /// Nothing is running, and `INTERVAL` has passed since the last start.
    pub(crate) fn due(&self, now: Instant) -> bool {
        !self.running() && self.next.is_none_or(|next| now >= next)
    }

    /// Start `work` when [`Self::due`] is true.
    pub(crate) fn start<F>(&mut self, now: Instant, work: F) -> bool
    where
        F: FnOnce() -> T + Send + 'static,
    {
        if !self.due(now) {
            return false;
        }
        self.next = Some(now + INTERVAL);
        self.job = Some(std::thread::spawn(work));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn reap_does_not_wait_for_a_running_scan() {
        let mut slot = Slot::default();
        let (tx, rx) = mpsc::channel();
        let now = Instant::now();
        assert!(slot.start(now, move || rx.recv().unwrap_or(0)));
        let began = Instant::now();
        assert_eq!(slot.reap(), None);
        assert!(began.elapsed() < Duration::from_millis(200));
        assert!(slot.running());
        tx.send(7).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = None;
        while got.is_none() {
            assert!(Instant::now() < deadline, "scan did not finish");
            got = slot.reap();
            if got.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(got, Some(7));
        assert!(!slot.running());
    }

    #[test]
    fn start_is_at_most_once_per_second() {
        let mut slot = Slot::default();
        let now = Instant::now();
        assert!(slot.start(now, || ()));
        assert!(!slot.start(now, || ()));
        assert!(!slot.start(now + Duration::from_millis(999), || ()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while slot.running() {
            assert!(Instant::now() < deadline, "scan did not finish");
            let _ = slot.reap();
            if slot.running() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(!slot.start(now + Duration::from_millis(999), || ()));
        assert!(slot.start(now + INTERVAL, || ()));
    }

    #[test]
    fn due_waits_out_the_interval_after_the_job_finishes() {
        let mut slot = Slot::default();
        let now = Instant::now();
        assert!(slot.due(now));
        assert!(slot.start(now, || ()));
        assert!(!slot.due(now));
        let deadline = Instant::now() + Duration::from_secs(2);
        while slot.running() {
            assert!(Instant::now() < deadline, "scan did not finish");
            let _ = slot.reap();
            if slot.running() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(!slot.due(now + Duration::from_millis(999)));
        assert!(slot.due(now + INTERVAL));
    }
}
