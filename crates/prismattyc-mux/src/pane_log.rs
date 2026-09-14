//! Per-pane event log (PT-76) and SubscribePane catch-up (PT-77).
//!
//! Sequence numbers are monotonic and never reused. The ring is bounded by
//! payload bytes (default 4 MiB), not event count.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Default ring capacity per pane.
pub(crate) const DEFAULT_PANE_LOG_BYTES: usize = 4 * 1024 * 1024;
/// Fixed sequence and discriminator bytes charged to each pane-log frame.
pub(crate) const PANE_FRAME_OVERHEAD: usize = 9;
/// Maximum queued PTY output admitted for one pane stream.
pub(crate) const PTY_OUTPUT_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// A byte budget that admits owned payloads before their allocation.
#[derive(Clone)]
pub(crate) struct ByteBudget {
    inner: Arc<ByteBudgetInner>,
}

struct ByteBudgetInner {
    reserved: Mutex<usize>,
    available: Condvar,
    cap: usize,
}

/// A budget reservation released when the admitted payload is dropped.
pub(crate) struct ByteReservation {
    budget: Arc<ByteBudgetInner>,
    bytes: usize,
}

impl ByteBudget {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(ByteBudgetInner {
                reserved: Mutex::new(0),
                available: Condvar::new(),
                cap,
            }),
        }
    }

    fn reserve(&self, bytes: usize) -> Option<ByteReservation> {
        if bytes > self.inner.cap {
            return None;
        }
        let mut reserved = self
            .inner
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while reserved.saturating_add(bytes) > self.inner.cap {
            reserved = self
                .inner
                .available
                .wait(reserved)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *reserved = reserved.saturating_add(bytes);
        Some(ByteReservation {
            budget: Arc::clone(&self.inner),
            bytes,
        })
    }

    /// Reserve bytes before running the allocation closure.
    pub(crate) fn admit<T>(
        &self,
        bytes: usize,
        allocate: impl FnOnce() -> T,
    ) -> Option<(T, ByteReservation)> {
        let reservation = self.reserve(bytes)?;
        Some((allocate(), reservation))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn reserved(&self) -> usize {
        *self
            .inner
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        let mut reserved = self
            .budget
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *reserved = reserved.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

/// One logged pane event. Order matches application to the server emulator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneEvent {
    Output {
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
        cell_px: (u32, u32),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_owner: Option<crate::remote_size::SizeOwner>,
        /// Missing on older daemons, whose resize clips the primary grid.
        #[serde(default)]
        reflow: bool,
    },
    Title {
        text: String,
    },
    Cwd {
        path: PathBuf,
    },
    /// `None` clears guest status.
    Status {
        text: Option<String>,
    },
    Attention {
        text: String,
    },
    MailDepth {
        depth: u32,
    },
    /// Who owns the window size (PT-202). Independent of Resize.
    SizeOwnerChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<crate::remote_size::SizeOwner>,
    },
    Exited {
        code: Option<u32>,
        signal: Option<String>,
    },
}

/// Internal classification for event and snapshot frames.
///
/// This is not serialized. The wire format continues to use `PaneEvent`'s
/// existing `kind` discriminator.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneFrameKind {
    Output,
    Resize,
    Title,
    Cwd,
    Status,
    Attention,
    MailDepth,
    SizeOwnerChanged,
    Exited,
    PaneStyled,
}

/// Pure behavior policy for one pane or snapshot frame.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFramePolicy {
    /// Never coalesce or drop the frame. Stop reading at the byte limit.
    Backpressure,
    /// Keep the newest pending value. Count superseded frames and bytes.
    KeepNewest,
    /// Replace only within the allowed contiguous run or sequence boundary.
    ReplaceableInRun,
}

impl PaneFrameKind {
    #[cfg_attr(not(test), allow(dead_code))]
    fn policy(self) -> PaneFramePolicy {
        match self {
            Self::Output | Self::Attention | Self::Exited => PaneFramePolicy::Backpressure,
            Self::Title | Self::Cwd | Self::Status | Self::MailDepth | Self::SizeOwnerChanged => {
                PaneFramePolicy::KeepNewest
            }
            Self::Resize | Self::PaneStyled => PaneFramePolicy::ReplaceableInRun,
        }
    }
}

impl PaneEvent {
    /// Classify an event without serializing or changing pane-log state.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn frame_kind(&self) -> PaneFrameKind {
        match self {
            Self::Output { .. } => PaneFrameKind::Output,
            Self::Resize { .. } => PaneFrameKind::Resize,
            Self::Title { .. } => PaneFrameKind::Title,
            Self::Cwd { .. } => PaneFrameKind::Cwd,
            Self::Status { .. } => PaneFrameKind::Status,
            Self::Attention { .. } => PaneFrameKind::Attention,
            Self::MailDepth { .. } => PaneFrameKind::MailDepth,
            Self::SizeOwnerChanged { .. } => PaneFrameKind::SizeOwnerChanged,
            Self::Exited { .. } => PaneFrameKind::Exited,
        }
    }

    /// Map a pane event to the approved PT-268 behavior policy.
    pub fn frame_policy(&self) -> PaneFramePolicy {
        self.frame_kind().policy()
    }

    fn byte_len(&self) -> usize {
        // 8-byte seq + 1-byte tag, counted with the payload.
        PANE_FRAME_OVERHEAD
            + match self {
                Self::Output { bytes } => bytes.len(),
                Self::Resize { .. } => 2 + 2 + 4 + 4 + 1,
                Self::Title { text } => text.len(),
                Self::Cwd { path } => path.as_os_str().as_encoded_bytes().len(),
                Self::Status { text } => text.as_ref().map(String::len).unwrap_or(0),
                Self::Attention { text } => text.len(),
                Self::MailDepth { .. } => 4,
                Self::SizeOwnerChanged { .. } => 16,
                Self::Exited { signal, .. } => 4 + signal.as_ref().map(String::len).unwrap_or(0),
            }
    }
}

/// One ring entry. `seq` is never reused after the entry is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLogFrame {
    pub seq: u64,
    pub event: PaneEvent,
}

/// Catch-up result for `SubscribePane { from_seq }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatchUp {
    Events(Vec<PaneLogFrame>),
    Gap { oldest: u64, current: u64 },
    Ahead { current: u64, oldest: Option<u64> },
}

/// Bounded per-pane log. First event is seq 1.
#[derive(Debug, Clone)]
pub(crate) struct PaneLog {
    next_seq: u64,
    events: VecDeque<PaneLogFrame>,
    bytes: usize,
    cap: usize,
}

impl PaneLog {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            next_seq: 1,
            events: VecDeque::new(),
            bytes: 0,
            cap: cap.max(1),
        }
    }

    pub(crate) fn append(&mut self, event: PaneEvent) -> u64 {
        let cost = event.byte_len();
        while !self.events.is_empty() && self.bytes.saturating_add(cost) > self.cap {
            if let Some(old) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.event.byte_len());
            }
        }
        if self.bytes.saturating_add(cost) > self.cap {
            self.events.clear();
            self.bytes = 0;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.bytes = self.bytes.saturating_add(cost);
        self.events.push_back(PaneLogFrame { seq, event });
        seq
    }

    #[allow(dead_code)]
    pub(crate) fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub(crate) fn current_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub(crate) fn oldest_seq(&self) -> Option<u64> {
        self.events.front().map(|e| e.seq)
    }

    #[allow(dead_code)]
    pub(crate) fn byte_len(&self) -> usize {
        self.bytes
    }

    #[allow(dead_code)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &PaneLogFrame> {
        self.events.iter()
    }

    pub(crate) fn frames(&self) -> Vec<PaneLogFrame> {
        self.events.iter().cloned().collect()
    }

    /// Rebuild a ring from persisted frames, keeping their sequence numbers.
    ///
    /// Frames that overflow `cap` drop from the front. The newest frame is
    /// kept even when it alone is larger than `cap`.
    pub(crate) fn from_frames(frames: Vec<PaneLogFrame>, cap: usize) -> Self {
        let mut log = Self::new(cap);
        if let Some(last) = frames.last() {
            log.next_seq = last.seq.saturating_add(1);
        }
        for frame in frames {
            log.bytes = log.bytes.saturating_add(frame.event.byte_len());
            log.events.push_back(frame);
        }
        while log.bytes > log.cap && log.events.len() > 1 {
            if let Some(old) = log.events.pop_front() {
                log.bytes = log.bytes.saturating_sub(old.event.byte_len());
            }
        }
        log
    }

    /// Events after `from_seq`, or Gap/Ahead like the domain event ring.
    pub(crate) fn catch_up(&self, from_seq: u64) -> CatchUp {
        let current = self.current_seq();
        if from_seq > current {
            return CatchUp::Ahead {
                current,
                oldest: self.oldest_seq(),
            };
        }
        let oldest = self.oldest_seq().unwrap_or(1);
        if from_seq.saturating_add(1) < oldest {
            return CatchUp::Gap { oldest, current };
        }
        CatchUp::Events(
            self.events
                .iter()
                .filter(|frame| frame.seq > from_seq)
                .cloned()
                .collect(),
        )
    }

    #[cfg(test)]
    pub(crate) fn events(&self) -> Vec<&PaneEvent> {
        self.events.iter().map(|e| &e.event).collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared pane-log signal. Same lock-coupled ring as `MailboxWatch`.
///
/// Producers call [`Self::note`] under the control-plane lock, then
/// [`Self::ring_pending`] after that lock drops. Waiters hold this watch
/// lock while briefly taking the plane lock. Ringing under the plane lock
/// deadlocks (watch→plane vs plane→watch).
#[derive(Default)]
pub(crate) struct PaneLogWatch {
    state: Mutex<()>,
    cond: Condvar,
    pending: AtomicBool,
}

impl PaneLogWatch {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record that waiters should wake. Does not take the watch lock.
    pub(crate) fn note(&self) {
        self.pending.store(true, Ordering::Release);
    }

    /// Wake waiters if [`Self::note`] ran. Takes the watch lock; call only
    /// after dropping the control-plane lock.
    pub(crate) fn ring_pending(&self) {
        if self.pending.swap(false, Ordering::AcqRel) {
            self.ring();
        }
    }

    pub(crate) fn ring(&self) {
        let _guard = lock(&self.state);
        self.cond.notify_all();
    }

    pub(crate) fn wait_until<T, E>(
        &self,
        timeout: Duration,
        mut check: impl FnMut() -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = check()? {
                return Ok(Some(value));
            }
            let mut guard = lock(&self.state);
            if let Some(value) = check()? {
                return Ok(Some(value));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            guard = self
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            drop(guard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_policy_table_covers_every_event_variant_and_pane_styled() {
        let cases = [
            (
                PaneEvent::Output {
                    bytes: b"output".to_vec(),
                },
                PaneFrameKind::Output,
                PaneFramePolicy::Backpressure,
            ),
            (
                PaneEvent::Resize {
                    cols: 80,
                    rows: 24,
                    cell_px: (10, 20),
                    size_owner: None,
                    reflow: true,
                },
                PaneFrameKind::Resize,
                PaneFramePolicy::ReplaceableInRun,
            ),
            (
                PaneEvent::Title {
                    text: "title".into(),
                },
                PaneFrameKind::Title,
                PaneFramePolicy::KeepNewest,
            ),
            (
                PaneEvent::Cwd {
                    path: PathBuf::from("/tmp/work"),
                },
                PaneFrameKind::Cwd,
                PaneFramePolicy::KeepNewest,
            ),
            (
                PaneEvent::Status { text: None },
                PaneFrameKind::Status,
                PaneFramePolicy::KeepNewest,
            ),
            (
                PaneEvent::Attention {
                    text: "attention".into(),
                },
                PaneFrameKind::Attention,
                PaneFramePolicy::Backpressure,
            ),
            (
                PaneEvent::MailDepth { depth: 1 },
                PaneFrameKind::MailDepth,
                PaneFramePolicy::KeepNewest,
            ),
            (
                PaneEvent::SizeOwnerChanged { owner: None },
                PaneFrameKind::SizeOwnerChanged,
                PaneFramePolicy::KeepNewest,
            ),
            (
                PaneEvent::Exited {
                    code: Some(0),
                    signal: None,
                },
                PaneFrameKind::Exited,
                PaneFramePolicy::Backpressure,
            ),
        ];

        for (event, expected_kind, expected_policy) in cases {
            assert_eq!(event.frame_kind(), expected_kind, "{event:?}");
            assert_eq!(event.frame_policy(), expected_policy, "{event:?}");
            assert_eq!(expected_kind.policy(), expected_policy, "{event:?}");
        }
        assert_eq!(
            PaneFrameKind::PaneStyled.policy(),
            PaneFramePolicy::ReplaceableInRun
        );
    }

    #[test]
    fn frame_policy_keeps_approved_behavior_boundaries() {
        assert_eq!(
            PaneEvent::Resize {
                cols: 80,
                rows: 24,
                cell_px: (10, 20),
                size_owner: None,
                reflow: true,
            }
            .frame_policy(),
            PaneFramePolicy::ReplaceableInRun
        );
        assert_eq!(
            PaneEvent::Attention {
                text: "bell".into(),
            }
            .frame_policy(),
            PaneFramePolicy::Backpressure
        );
        assert_eq!(
            PaneEvent::Title {
                text: "title".into(),
            }
            .frame_policy(),
            PaneFramePolicy::KeepNewest
        );
        assert_eq!(
            PaneFrameKind::PaneStyled.policy(),
            PaneFramePolicy::ReplaceableInRun
        );
    }

    #[test]
    fn byte_admission_reserves_before_allocation_and_releases() {
        let budget = ByteBudget::new(4);
        let allocated = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&allocated);
        assert!(budget
            .admit(5, || {
                flag.store(true, Ordering::SeqCst);
                vec![0u8; 5]
            })
            .is_none());
        assert!(!allocated.load(Ordering::SeqCst));

        let (payload, reservation) = budget.admit(4, || vec![1u8; 4]).unwrap();
        assert_eq!(payload.len(), 4);
        assert_eq!(budget.reserved(), 4);
        drop(payload);
        assert_eq!(budget.reserved(), 4);
        drop(reservation);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn byte_admission_blocks_before_the_next_allocation() {
        let budget = ByteBudget::new(4);
        let held = budget.admit(4, || vec![0u8; 4]).unwrap();
        let allocated = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&allocated);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiting_budget = budget.clone();
        let waiter = std::thread::spawn(move || {
            let admitted = waiting_budget.admit(1, || {
                flag.store(true, Ordering::SeqCst);
                vec![1u8]
            });
            done_tx.send(admitted.is_some()).unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(!allocated.load(Ordering::SeqCst));
        drop(held);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)), Ok(true));
        waiter.join().unwrap();
        assert!(allocated.load(Ordering::SeqCst));
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn frame_kind_does_not_change_wire_serialization() {
        let event = PaneEvent::MailDepth { depth: 0 };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some("mail_depth")
        );
        assert_eq!(
            value.get("depth").and_then(serde_json::Value::as_u64),
            Some(0)
        );
    }

    #[test]
    fn seq_starts_at_one_and_never_reuses() {
        let mut log = PaneLog::new(64);
        assert_eq!(log.append(PaneEvent::MailDepth { depth: 1 }), 1);
        assert_eq!(log.append(PaneEvent::MailDepth { depth: 2 }), 2);
        assert_eq!(log.next_seq(), 3);
        assert_eq!(log.oldest_seq(), Some(1));
    }

    #[test]
    fn byte_cap_drops_oldest_and_keeps_seq() {
        let mut log = PaneLog::new(64);
        let seq1 = log.append(PaneEvent::Output {
            bytes: vec![b'a'; 20],
        });
        let seq2 = log.append(PaneEvent::Output {
            bytes: vec![b'b'; 20],
        });
        assert_eq!((seq1, seq2), (1, 2));
        let seq3 = log.append(PaneEvent::Output {
            bytes: vec![b'c'; 20],
        });
        assert_eq!(seq3, 3);
        assert_eq!(log.oldest_seq(), Some(2));
        assert_eq!(log.next_seq(), 4);
        let payloads: Vec<&[u8]> = log
            .iter()
            .filter_map(|e| match &e.event {
                PaneEvent::Output { bytes } => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(payloads, vec![&vec![b'b'; 20][..], &vec![b'c'; 20][..]]);
        assert!(log.byte_len() <= 64);
    }

    #[test]
    fn one_event_larger_than_cap_is_kept() {
        let mut log = PaneLog::new(16);
        log.append(PaneEvent::Output {
            bytes: vec![b'x'; 8],
        });
        let seq = log.append(PaneEvent::Output {
            bytes: vec![b'y'; 64],
        });
        assert_eq!(seq, 2);
        assert_eq!(log.iter().count(), 1);
        assert_eq!(log.oldest_seq(), Some(2));
        let last = log.iter().next().unwrap().event.clone();
        match last {
            PaneEvent::Output { bytes } => assert_eq!(bytes.len(), 64),
            other => panic!("expected oversized output, got {other:?}"),
        }
    }

    #[test]
    fn catch_up_matches_event_gap_rules() {
        let mut log = PaneLog::new(64);
        assert!(matches!(log.catch_up(0), CatchUp::Events(v) if v.is_empty()));
        log.append(PaneEvent::MailDepth { depth: 1 });
        log.append(PaneEvent::MailDepth { depth: 2 });
        match log.catch_up(0) {
            CatchUp::Events(frames) => {
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[0].seq, 1);
            }
            other => panic!("{other:?}"),
        }
        match log.catch_up(1) {
            CatchUp::Events(frames) => assert_eq!(frames[0].seq, 2),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            log.catch_up(5),
            CatchUp::Ahead {
                current: 2,
                oldest: Some(1)
            }
        ));
        // Drop seq 1 by overflowing the cap with large outputs.
        log.append(PaneEvent::Output {
            bytes: vec![b'x'; 40],
        });
        log.append(PaneEvent::Output {
            bytes: vec![b'y'; 40],
        });
        let oldest = log.oldest_seq().unwrap();
        assert!(oldest > 1);
        assert!(matches!(
            log.catch_up(0),
            CatchUp::Gap { oldest: o, .. } if o == oldest
        ));
        match log.catch_up(oldest.saturating_sub(1)) {
            CatchUp::Events(frames) => assert_eq!(frames[0].seq, oldest),
            other => panic!("{other:?}"),
        }
    }
}
