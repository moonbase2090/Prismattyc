//! One Windows process scan, off the UI thread, at most once a second.
//!
//! The pump used to call `CreateToolhelp32Snapshot` / `Process32NextW` on the
//! UI thread. A full walk per pid pegs a core while the window is idle. The
//! caller only reaps a finished job or starts the next one.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Minimum gap between scan starts.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct Slot<T> {
    next: Option<Instant>,
    job: Option<JoinHandle<T>>,
}

/// An empty slot. `T: Default` is not required: the job value does not exist
/// until a scan finishes, and `WindowsDiscovery` has no default.
impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            next: None,
            job: None,
        }
    }
}

/// One host pane the idle scan may ask about.
pub(crate) struct PaneScanView<P> {
    pub(crate) pane: P,
    /// Local PTY child. A log-backed attach has none.
    pub(crate) local_child: Option<u32>,
    /// No attach session yet, so a local child may be adopted.
    pub(crate) unmarked: bool,
    /// Daemon pane this host pane shows, when it is attached.
    pub(crate) remote_pane: Option<u64>,
}

/// Daemon pane child from the snapshot. Not a local PTY pid.
pub(crate) struct RemoteChild {
    pub(crate) pane: u64,
    pub(crate) child_pid: Option<u32>,
}

/// Agent and adoption roots, plus the separate cwd lookup set.
pub(crate) struct WindowsScanRequest<P> {
    pub(crate) agent_roots: Vec<u32>,
    pub(crate) candidates: Vec<(P, u32)>,
    /// Local children plus daemon children of attached panes.
    pub(crate) cwd_pids: Vec<u32>,
}

/// Local PTY children stay the agent and adoption roots. Daemon children of
/// attached panes are added only to the cwd lookup.
pub(crate) fn windows_scan_request<P: Copy>(
    panes: &[PaneScanView<P>],
    remote_children: &[RemoteChild],
) -> WindowsScanRequest<P> {
    let mut agent_roots = Vec::new();
    let mut candidates = Vec::new();
    let mut cwd_pids = Vec::new();
    for pane in panes {
        if let Some(pid) = pane.local_child {
            agent_roots.push(pid);
            push_cwd(&mut cwd_pids, pid);
            if pane.unmarked {
                candidates.push((pane.pane, pid));
            }
        }
        let Some(remote) = pane.remote_pane else {
            continue;
        };
        let Some(child) = remote_children
            .iter()
            .find(|entry| entry.pane == remote)
            .and_then(|entry| entry.child_pid)
        else {
            continue;
        };
        push_cwd(&mut cwd_pids, child);
    }
    WindowsScanRequest {
        agent_roots,
        candidates,
        cwd_pids,
    }
}

fn push_cwd(cwd_pids: &mut Vec<u32>, pid: u32) {
    if !cwd_pids.contains(&pid) {
        cwd_pids.push(pid);
    }
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

    /// `#[derive(Default)]` on `Slot<T>` requires `T: Default`. An empty slot
    /// must build for a job type that has no default value.
    #[test]
    fn default_slot_is_empty_when_the_job_type_has_no_default() {
        #[derive(Debug)]
        struct NoDefault;
        let slot = Slot::<NoDefault>::default();
        assert!(slot.due(Instant::now()));
        assert!(!slot.running());
        assert!(slot.job.is_none());
        assert!(slot.next.is_none());
    }

    #[test]
    fn attached_pane_without_a_local_child_looks_up_the_daemon_child_only() {
        let panes = [
            PaneScanView {
                pane: 1u64,
                local_child: Some(7),
                unmarked: true,
                remote_pane: None,
            },
            PaneScanView {
                pane: 2,
                local_child: None,
                unmarked: false,
                remote_pane: Some(42),
            },
            PaneScanView {
                pane: 3,
                local_child: Some(8),
                unmarked: false,
                remote_pane: Some(43),
            },
        ];
        let remote = [
            RemoteChild {
                pane: 42,
                child_pid: Some(99),
            },
            RemoteChild {
                pane: 43,
                child_pid: Some(100),
            },
            RemoteChild {
                pane: 77,
                child_pid: Some(55),
            },
        ];
        let request = windows_scan_request(&panes, &remote);
        assert_eq!(request.agent_roots, vec![7, 8]);
        assert_eq!(request.candidates, vec![(1, 7)]);
        assert_eq!(request.cwd_pids, vec![7, 99, 8, 100]);
    }
}
