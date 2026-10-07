// SPDX-License-Identifier: MPL-2.0
//! Clipboard paste delivery that never waits on the main thread (#195).
//!
//! With `async_paste = true`, every paste is one [`ChildWrite`] that carries a
//! [`PasteTicket`]. The message takes its place on the pane's own writer
//! channel when the paste starts, so later keys stay behind it and the paste
//! can only reach the pane it started in. The writer thread owns the
//! back-pressure wait (the PTY writer blocks in `write`; the log writer chunks
//! into pmuxd input), which keeps `CSI 200 ~` and `CSI 201 ~` together.
//!
//! An image-only clipboard gets a ticket whose bytes are filled later: a
//! worker encodes the PNG, writes the file, and fills in the reference. The
//! writer waits for it in order, so two image pastes and any keys after them
//! keep their order.
//!
//! Each ticket reports exactly once through [`PasteJobs::take_outcomes`]:
//! delivered, or incomplete with the bytes written. A ticket dropped without a
//! report (writer stopped, pane closed) reports incomplete from `Drop`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;

use crate::mux::Wake;
use crate::rich::ChildWrite;

/// Where a paste started: the mux runtime instance and the pane in it.
/// Pane ids restart at 1 in each runtime, so the pane alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PasteOrigin {
    pub mux: u64,
    pub pane: u64,
}

/// How a paste ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteResult {
    Delivered {
        bytes: usize,
    },
    Incomplete {
        written: usize,
        total: Option<usize>,
        reason: String,
    },
}

/// One finished paste, reported by the writer (or by `Drop`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PasteOutcome {
    pub origin: PasteOrigin,
    /// The file an image paste referenced, for the "pasted image" toast.
    pub image: Option<PathBuf>,
    pub result: PasteResult,
}

enum Slot {
    Pending,
    Ready(Vec<u8>),
    Failed(String),
    Taken,
}

struct Ticket {
    origin: PasteOrigin,
    slot: Mutex<Slot>,
    filled: Condvar,
    image: Mutex<Option<PathBuf>>,
    reported: AtomicBool,
    outcomes: mpsc::Sender<PasteOutcome>,
    wake: Option<Wake>,
}

impl Ticket {
    fn report(&self, result: PasteResult) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let image = self.image.lock().unwrap().clone();
        let _ = self.outcomes.send(PasteOutcome {
            origin: self.origin,
            image,
            result,
        });
        if let Some(wake) = &self.wake {
            wake();
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.report(PasteResult::Incomplete {
            written: 0,
            total: None,
            reason: "the pane writer stopped before the paste".into(),
        });
    }
}

/// Writer-side handle for one paste. Clones share one ticket.
#[derive(Clone)]
pub(crate) struct PasteTicket(Arc<Ticket>);

impl std::fmt::Debug for PasteTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PasteTicket").field(&self.0.origin).finish()
    }
}

impl PartialEq for PasteTicket {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PasteTicket {}

impl PasteTicket {
    fn fill(&self, bytes: Result<Vec<u8>, String>) {
        let mut slot = self.0.slot.lock().unwrap();
        if matches!(*slot, Slot::Pending) {
            *slot = match bytes {
                Ok(bytes) => Slot::Ready(bytes),
                Err(reason) => Slot::Failed(reason),
            };
            self.0.filled.notify_all();
        }
    }

    /// Wait (writer thread only) until the bytes exist, then take them.
    fn take_bytes(&self) -> Result<Vec<u8>, String> {
        let mut slot = self.0.slot.lock().unwrap();
        while matches!(*slot, Slot::Pending) {
            slot = self.0.filled.wait(slot).unwrap();
        }
        match std::mem::replace(&mut *slot, Slot::Taken) {
            Slot::Ready(bytes) => Ok(bytes),
            Slot::Failed(reason) => Err(reason),
            Slot::Taken | Slot::Pending => Err("paste bytes already taken".into()),
        }
    }

    pub(crate) fn delivered(&self, bytes: usize) {
        self.0.report(PasteResult::Delivered { bytes });
    }

    pub(crate) fn incomplete(
        &self,
        written: usize,
        total: Option<usize>,
        reason: impl Into<String>,
    ) {
        self.0.report(PasteResult::Incomplete {
            written,
            total,
            reason: reason.into(),
        });
    }
}

/// Fails a still-pending ticket if the encode worker ends without filling it,
/// so the writer never waits forever.
struct FillGuard(PasteTicket);

impl Drop for FillGuard {
    fn drop(&mut self) {
        self.0.fill(Err("image encode stopped".into()));
    }
}

/// The bytes a writer sends for `msg`, and its paste ticket if any. Blocks
/// until a deferred image paste has its bytes. A failed image encode reports
/// the ticket incomplete and yields no bytes.
pub(crate) fn writer_payload(msg: ChildWrite) -> (Vec<u8>, Option<PasteTicket>) {
    let Some(ticket) = msg.paste else {
        return (msg.bytes, None);
    };
    match ticket.take_bytes() {
        Ok(bytes) => (bytes, Some(ticket)),
        Err(reason) => {
            ticket.incomplete(0, None, reason);
            (Vec::new(), None)
        }
    }
}

/// Write `bytes` in bounded chunks, publishing progress in `progress`, so a
/// failure (or a watcher) can say how much went out.
pub(crate) fn write_counted(
    writer: &mut dyn Write,
    bytes: &[u8],
    progress: &AtomicUsize,
) -> Result<(), (usize, String)> {
    const CHUNK: usize = 4 * 1024;
    let mut written = 0;
    progress.store(0, Ordering::SeqCst);
    for chunk in bytes.chunks(CHUNK) {
        if let Err(error) = writer.write_all(chunk) {
            return Err((written, error.to_string()));
        }
        written += chunk.len();
        progress.store(written, Ordering::SeqCst);
    }
    let _ = writer.flush();
    Ok(())
}

const CHILD_EXITED: &str = "the pane's program exited";

/// A PTY pane's writer, shared by its writer thread and its child-exit
/// watcher.
///
/// Once the child exits and the slave side closes, a Linux PTY master
/// `write` blocks for good; macOS fails it with EIO. So the watcher reports
/// the paste the writer is stuck in and drops the queue: every paste still
/// gets an outcome, and later sends see `Closed`.
pub(crate) struct PtyWriter {
    rx: Mutex<Option<mpsc::Receiver<ChildWrite>>>,
    state: Mutex<WriterState>,
    written: AtomicUsize,
}

#[derive(Default)]
struct WriterState {
    exited: bool,
    /// The paste being written and its length.
    in_flight: Option<(PasteTicket, usize)>,
}

impl PtyWriter {
    pub(crate) fn new(rx: mpsc::Receiver<ChildWrite>) -> Arc<Self> {
        Arc::new(Self {
            rx: Mutex::new(Some(rx)),
            state: Mutex::default(),
            written: AtomicUsize::new(0),
        })
    }

    /// Writer thread: one message at a time, in order. A failed write stops
    /// it; `granted` runs for a capability grant after its bytes are written.
    pub(crate) fn run(
        &self,
        child: &mut dyn Write,
        mut granted: impl FnMut(crate::rich::CapabilityGrant),
    ) {
        loop {
            let msg = {
                let rx = self.rx.lock().unwrap();
                let Some(Ok(msg)) = rx.as_ref().map(mpsc::Receiver::recv) else {
                    return;
                };
                msg
            };
            let grant = msg.capability_grant.clone();
            let (bytes, ticket) = writer_payload(msg);
            {
                let mut state = self.state.lock().unwrap();
                if state.exited {
                    drop(state);
                    if let Some(ticket) = ticket {
                        ticket.incomplete(0, Some(bytes.len()), CHILD_EXITED);
                    }
                    self.close();
                    return;
                }
                state.in_flight = ticket.map(|ticket| (ticket, bytes.len()));
            }
            let result = write_counted(child, &bytes, &self.written);
            let in_flight = self.state.lock().unwrap().in_flight.take();
            match result {
                Ok(()) => {
                    if let Some((ticket, total)) = in_flight {
                        ticket.delivered(total);
                    }
                    if let Some(grant) = grant {
                        granted(grant);
                    }
                }
                Err((written, reason)) => {
                    if let Some((ticket, total)) = in_flight {
                        ticket.incomplete(written, Some(total), reason);
                    }
                    self.close();
                    return;
                }
            }
        }
    }

    /// The child exited: report the paste being written and drop the queue.
    pub(crate) fn child_exited(&self) {
        let in_flight = {
            let mut state = self.state.lock().unwrap();
            state.exited = true;
            state.in_flight.take()
        };
        if let Some((ticket, total)) = in_flight {
            let written = self.written.load(Ordering::SeqCst);
            ticket.incomplete(written, Some(total), CHILD_EXITED);
        }
        self.close();
    }

    /// Drop the receiver and what is queued in it. A writer waiting in
    /// `recv` holds the lock; it sees `exited` on its next message instead.
    fn close(&self) {
        if let Ok(mut rx) = self.rx.try_lock() {
            rx.take();
        }
    }
}

/// Result of offering a paste to a pane writer. Never blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteSend {
    /// The writer accepted the paste; its outcome arrives later.
    Queued,
    /// The writer channel is full: the child has stopped reading and earlier
    /// input is still queued. Nothing was sent, so no bracket is left open.
    Busy,
    /// The writer is gone (the child exited).
    Closed,
}

/// Offer one paste message to the writer, without waiting. A refused paste
/// reports its ticket incomplete.
pub(crate) fn send_paste(to_child: &mpsc::SyncSender<ChildWrite>, msg: ChildWrite) -> PasteSend {
    match to_child.try_send(msg) {
        Ok(()) => PasteSend::Queued,
        Err(mpsc::TrySendError::Full(msg)) => {
            refuse(msg, "the pane's input queue is full");
            PasteSend::Busy
        }
        Err(mpsc::TrySendError::Disconnected(msg)) => {
            refuse(msg, "the pane writer is gone");
            PasteSend::Closed
        }
    }
}

fn refuse(msg: ChildWrite, reason: &str) {
    if let Some(ticket) = msg.paste {
        ticket.incomplete(0, None, reason);
    }
}

/// Clipboard RGBA pixels copied off the clipboard on the main thread.
pub(crate) struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

type Encode = Arc<dyn Fn(u32, u32, &[u8]) -> anyhow::Result<PathBuf> + Send + Sync>;

/// Paste tickets and their outcomes for one window.
pub(crate) struct PasteJobs {
    tx: mpsc::Sender<PasteOutcome>,
    rx: mpsc::Receiver<PasteOutcome>,
    encode: Encode,
}

impl Default for PasteJobs {
    fn default() -> Self {
        Self::with_encoder(Arc::new(prismattyc_mux::write_paste_png))
    }
}

impl PasteJobs {
    fn with_encoder(encode: Encode) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, encode }
    }

    fn ticket(&self, origin: PasteOrigin, wake: Option<Wake>) -> PasteTicket {
        PasteTicket(Arc::new(Ticket {
            origin,
            slot: Mutex::new(Slot::Pending),
            filled: Condvar::new(),
            image: Mutex::new(None),
            reported: AtomicBool::new(false),
            outcomes: self.tx.clone(),
            wake,
        }))
    }

    /// A paste whose bytes are known now. `image` names a referenced image
    /// file (a copied file or URI) for the delivered toast.
    pub(crate) fn text(
        &self,
        origin: PasteOrigin,
        bytes: Vec<u8>,
        image: Option<PathBuf>,
        wake: Option<Wake>,
    ) -> ChildWrite {
        let ticket = self.ticket(origin, wake);
        *ticket.0.image.lock().unwrap() = image;
        ticket.fill(Ok(bytes));
        ChildWrite::paste(ticket)
    }

    /// An image paste. A worker encodes and writes the PNG, then `payload`
    /// turns the file path into the bytes to send. The returned message
    /// holds the pane writer's place until then.
    pub(crate) fn image(
        &self,
        origin: PasteOrigin,
        image: ClipboardImage,
        payload: impl FnOnce(&std::path::Path) -> Vec<u8> + Send + 'static,
        wake: Option<Wake>,
    ) -> ChildWrite {
        let ticket = self.ticket(origin, wake);
        let guard = FillGuard(ticket.clone());
        let encode = self.encode.clone();
        let spawned = thread::Builder::new()
            .name("prism-image-paste".into())
            .spawn(move || {
                let guard = guard;
                let filled = match encode(image.width, image.height, &image.rgba) {
                    Ok(path) => {
                        let bytes = payload(&path);
                        *guard.0 .0.image.lock().unwrap() = Some(path);
                        Ok(bytes)
                    }
                    Err(error) => Err(format!("image paste failed: {error:#}")),
                };
                guard.0.fill(filled);
            });
        if let Err(error) = spawned {
            ticket.fill(Err(format!("spawn image paste worker: {error}")));
        }
        ChildWrite::paste(ticket)
    }

    /// Outcomes reported since the last call.
    pub(crate) fn take_outcomes(&self) -> Vec<PasteOutcome> {
        self.rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const ORIGIN: PasteOrigin = PasteOrigin { mux: 1, pane: 1 };

    /// The PTY writer loop's per-message step, over any `Write`.
    fn write_message(writer: &mut dyn Write, msg: ChildWrite) -> bool {
        let (bytes, ticket) = writer_payload(msg);
        match write_counted(writer, &bytes, &AtomicUsize::new(0)) {
            Ok(()) => {
                if let Some(ticket) = ticket {
                    ticket.delivered(bytes.len());
                }
                true
            }
            Err((written, reason)) => {
                if let Some(ticket) = ticket {
                    ticket.incomplete(written, Some(bytes.len()), reason);
                }
                false
            }
        }
    }

    fn wait_outcomes(jobs: &PasteJobs, count: usize) -> Vec<PasteOutcome> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all = Vec::new();
        while all.len() < count && Instant::now() < deadline {
            all.extend(jobs.take_outcomes());
            thread::sleep(Duration::from_millis(2));
        }
        all
    }

    #[test]
    fn whole_bracketed_paste_is_one_writer_message_ahead_of_later_keys() {
        let jobs = PasteJobs::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut payload = b"\x1b[200~".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 64 * 1024));
        payload.extend_from_slice(b"\x1b[201~");
        let msg = jobs.text(ORIGIN, payload.clone(), None, None);
        assert_eq!(send_paste(&tx, msg), PasteSend::Queued);
        tx.try_send(ChildWrite::bytes(b"k".to_vec())).unwrap();
        let mut out = Vec::new();
        assert!(write_message(&mut out, rx.try_recv().unwrap()));
        assert_eq!(out, payload, "the paste must arrive whole, as one write");
        assert!(write_message(&mut out, rx.try_recv().unwrap()));
        assert!(
            out.ends_with(b"\x1b[201~k"),
            "the key lands after the closing bracket"
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            jobs.take_outcomes()[0].result,
            PasteResult::Delivered {
                bytes: payload.len()
            }
        );
    }

    #[test]
    fn full_writer_is_busy_without_waiting_and_reports_incomplete() {
        let jobs = PasteJobs::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.try_send(ChildWrite::bytes(b"earlier".to_vec())).unwrap();
        let started = Instant::now();
        let msg = jobs.text(ORIGIN, b"paste".to_vec(), None, None);
        assert_eq!(send_paste(&tx, msg), PasteSend::Busy);
        // The old path polled in 2 ms sleeps for up to 250 ms here.
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(rx.try_recv().unwrap().bytes, b"earlier");
        assert!(rx.try_recv().is_err(), "nothing of the paste may be sent");
        let outcomes = jobs.take_outcomes();
        assert!(
            matches!(
                &outcomes[..],
                [PasteOutcome {
                    result: PasteResult::Incomplete { written: 0, .. },
                    ..
                }]
            ),
            "{outcomes:?}"
        );
    }

    #[test]
    fn closed_writer_is_reported() {
        let jobs = PasteJobs::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        drop(rx);
        let msg = jobs.text(ORIGIN, b"paste".to_vec(), None, None);
        assert_eq!(send_paste(&tx, msg), PasteSend::Closed);
        assert!(matches!(
            jobs.take_outcomes()[..],
            [PasteOutcome {
                result: PasteResult::Incomplete { .. },
                ..
            }]
        ));
    }

    /// A writer that accepts `limit` bytes, then fails like a PTY whose
    /// child exited.
    struct FailsAfter {
        limit: usize,
        taken: Vec<u8>,
    }

    impl Write for FailsAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let room = self.limit - self.taken.len();
            if room == 0 {
                return Err(std::io::Error::other("child exited"));
            }
            let n = room.min(buf.len());
            self.taken.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_failure_after_accepting_a_prefix_reports_incomplete() {
        let jobs = PasteJobs::default();
        let total = 300 * 1024;
        let msg = jobs.text(ORIGIN, vec![b'y'; total], None, None);
        let mut child = FailsAfter {
            limit: 100 * 1024,
            taken: Vec::new(),
        };
        assert!(!write_message(&mut child, msg));
        let outcomes = jobs.take_outcomes();
        let [PasteOutcome {
            result:
                PasteResult::Incomplete {
                    written,
                    total: Some(reported),
                    ..
                },
            ..
        }] = &outcomes[..]
        else {
            panic!("expected one incomplete outcome: {outcomes:?}");
        };
        assert_eq!(*reported, total);
        assert!(*written < total, "written {written} of {total}");
    }

    /// A Linux PTY master after the child exits: accepts `limit` bytes,
    /// then blocks until released (never, in production).
    struct BlocksAfter {
        limit: usize,
        taken: usize,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlocksAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.taken == self.limit {
                let _ = self.release.recv();
                return Err(std::io::Error::other("released"));
            }
            let n = (self.limit - self.taken).min(buf.len());
            self.taken += n;
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn child_exit_reports_a_paste_stuck_in_a_blocked_write_and_closes_the_queue() {
        let jobs = PasteJobs::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let writer = PtyWriter::new(rx);
        let (release, held) = mpsc::channel();
        let limit = 64 * 1024;
        let mut child = BlocksAfter {
            limit,
            taken: 0,
            release: held,
        };
        let thread = {
            let writer = writer.clone();
            thread::spawn(move || writer.run(&mut child, |_| {}))
        };
        let total = 300 * 1024;
        let stuck = jobs.text(ORIGIN, vec![b'y'; total], None, None);
        assert_eq!(send_paste(&tx, stuck), PasteSend::Queued);
        let queued = jobs.text(ORIGIN, b"behind".to_vec(), None, None);
        assert_eq!(send_paste(&tx, queued), PasteSend::Queued);
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.written.load(Ordering::SeqCst) < limit {
            assert!(Instant::now() < deadline, "writer never reached the limit");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            jobs.take_outcomes().is_empty(),
            "the write is still blocked"
        );

        writer.child_exited();
        let outcomes = jobs.take_outcomes();
        assert_eq!(
            outcomes.iter().map(|o| &o.result).collect::<Vec<_>>(),
            [
                &PasteResult::Incomplete {
                    written: limit,
                    total: Some(total),
                    reason: CHILD_EXITED.into(),
                },
                &PasteResult::Incomplete {
                    written: 0,
                    total: None,
                    reason: "the pane writer stopped before the paste".into(),
                },
            ]
        );
        let later = jobs.text(ORIGIN, b"later".to_vec(), None, None);
        assert_eq!(send_paste(&tx, later), PasteSend::Closed);
        assert_eq!(jobs.take_outcomes().len(), 1);

        // If the write ever returns, nothing reports twice.
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(jobs.take_outcomes().is_empty());
    }

    #[test]
    fn child_exit_while_idle_fails_the_next_paste_and_closes_the_queue() {
        let jobs = PasteJobs::default();
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let writer = PtyWriter::new(rx);
        let thread = {
            let writer = writer.clone();
            thread::spawn(move || writer.run(&mut Vec::new(), |_| {}))
        };
        // The writer holds the receiver lock only while waiting in `recv`.
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.rx.try_lock().is_ok() {
            assert!(Instant::now() < deadline, "writer never waited");
            thread::sleep(Duration::from_millis(1));
        }
        writer.child_exited();
        let msg = jobs.text(ORIGIN, b"after exit".to_vec(), None, None);
        assert_eq!(send_paste(&tx, msg), PasteSend::Queued);
        thread.join().unwrap();
        assert!(matches!(
            &jobs.take_outcomes()[..],
            [PasteOutcome { result: PasteResult::Incomplete { written: 0, reason, .. }, .. }]
                if reason == CHILD_EXITED
        ));
        let later = jobs.text(ORIGIN, b"later".to_vec(), None, None);
        assert_eq!(send_paste(&tx, later), PasteSend::Closed);
    }

    #[test]
    fn a_dropped_paste_reports_incomplete() {
        let jobs = PasteJobs::default();
        drop(jobs.text(ORIGIN, b"lost".to_vec(), None, None));
        assert!(matches!(
            jobs.take_outcomes()[..],
            [PasteOutcome {
                result: PasteResult::Incomplete { .. },
                ..
            }]
        ));
    }

    /// An encoder that waits for a gate, then returns a path naming its
    /// thread, so tests control finish order and see where it ran.
    fn gated_encoder(gate: mpsc::Receiver<()>) -> Encode {
        let gate = Mutex::new(gate);
        Arc::new(move |width, height, _rgba| {
            gate.lock().unwrap().recv_timeout(Duration::from_secs(5))?;
            Ok(PathBuf::from(format!(
                "{width}x{height}-{:?}.png",
                thread::current().id()
            )))
        })
    }

    fn image(width: u32) -> ClipboardImage {
        ClipboardImage {
            width,
            height: 1,
            rgba: vec![0; width as usize * 4],
        }
    }

    fn reference(path: &std::path::Path) -> Vec<u8> {
        format!("[{}]", path.display()).into_bytes()
    }

    #[test]
    fn image_pastes_and_later_keys_keep_their_order() {
        // The first encode is held back; the second finishes first.
        let (first_gate, first) = mpsc::channel();
        let (second_gate, second) = mpsc::channel();
        let first_jobs = PasteJobs::with_encoder(gated_encoder(first));
        let second_jobs = PasteJobs::with_encoder(gated_encoder(second));
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(8);
        assert_eq!(
            send_paste(&tx, first_jobs.image(ORIGIN, image(1), reference, None)),
            PasteSend::Queued
        );
        assert_eq!(
            send_paste(&tx, second_jobs.image(ORIGIN, image(2), reference, None)),
            PasteSend::Queued
        );
        tx.try_send(ChildWrite::bytes(b"\r".to_vec())).unwrap();
        drop(tx);
        second_gate.send(()).unwrap();
        let writer = thread::spawn(move || {
            let mut out = Vec::new();
            for msg in rx {
                write_message(&mut out, msg);
            }
            out
        });
        thread::sleep(Duration::from_millis(50));
        first_gate.send(()).unwrap();
        let out = String::from_utf8(writer.join().unwrap()).unwrap();
        let one = out.find("[1x1-").expect(&out);
        let two = out.find("[2x1-").expect(&out);
        let enter = out.find('\r').expect(&out);
        assert!(one < two && two < enter, "order: {out:?}");
        assert!(
            !out.contains(&format!("{:?}", thread::current().id())),
            "encode ran on the caller: {out:?}"
        );
    }

    #[test]
    fn an_image_paste_goes_to_its_own_pane_writer_and_names_its_origin() {
        // Two runtimes whose focused panes share id 1 (Space A and Space B).
        let (gate, held) = mpsc::channel();
        let jobs = PasteJobs::with_encoder(gated_encoder(held));
        let (space_a, a_rx) = mpsc::sync_channel::<ChildWrite>(4);
        let (_space_b, b_rx) = mpsc::sync_channel::<ChildWrite>(4);
        let origin = PasteOrigin { mux: 10, pane: 1 };
        assert_eq!(
            send_paste(&space_a, jobs.image(origin, image(3), reference, None)),
            PasteSend::Queued
        );
        // The host switches to Space B while the encode is still running.
        gate.send(()).unwrap();
        let mut out = Vec::new();
        write_message(&mut out, a_rx.try_recv().unwrap());
        assert!(String::from_utf8(out).unwrap().starts_with("[3x1-"));
        assert!(
            b_rx.try_recv().is_err(),
            "Space B's pane 1 must get nothing"
        );
        let outcomes = wait_outcomes(&jobs, 1);
        assert_eq!(outcomes[0].origin, origin);
        assert!(outcomes[0].image.is_some());
    }

    #[test]
    fn a_failed_encode_unblocks_the_writer_and_reports() {
        let jobs = PasteJobs::with_encoder(Arc::new(|_, _, _| anyhow::bail!("disk full")));
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        send_paste(&tx, jobs.image(ORIGIN, image(1), reference, None));
        tx.try_send(ChildWrite::bytes(b"k".to_vec())).unwrap();
        let mut out = Vec::new();
        write_message(&mut out, rx.try_recv().unwrap());
        write_message(&mut out, rx.try_recv().unwrap());
        assert_eq!(out, b"k");
        let outcomes = wait_outcomes(&jobs, 1);
        assert!(
            matches!(&outcomes[0].result, PasteResult::Incomplete { reason, .. } if reason.contains("disk full")),
            "{outcomes:?}"
        );
    }

    /// Writes into a scratch directory, never the real `prism-paste` one,
    /// whose pruning could delete a person's recent paste files.
    fn scratch_encoder(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("paste-195-img-{}", std::process::id()));
        prismattyc_mux::write_paste_png_in(&dir, width, height, rgba)
    }

    /// Proof harness for #195 part 2: main-thread cost of an image paste.
    /// `cargo test -p prismattyc-host --bin prismattyc-host -- --ignored --nocapture image_paste`
    #[test]
    #[ignore = "measurement harness; run with --ignored"]
    fn image_paste_main_thread_cost_harness() {
        let (width, height) = (3024u32, 1964u32);
        let rgba: Vec<u8> = (0..width as usize * height as usize * 4)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let started = Instant::now();
        scratch_encoder(width, height, &rgba).unwrap();
        let inline = started.elapsed();
        let jobs = PasteJobs::with_encoder(Arc::new(scratch_encoder));
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        let started = Instant::now();
        let msg = jobs.image(
            ORIGIN,
            ClipboardImage {
                width,
                height,
                rgba,
            },
            reference,
            None,
        );
        send_paste(&tx, msg);
        let call = started.elapsed();
        let mut out = Vec::new();
        write_message(&mut out, rx.recv().unwrap());
        let total = started.elapsed();
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("paste-195-img-{}", std::process::id())),
        );
        println!(
            "image {width}x{height}: inline encode+write on main={inline:?}; \
             async image()+send on main={call:?}, writer had the reference after {total:?}"
        );
        assert!(!out.is_empty());
        assert!(
            call < Duration::from_millis(5),
            "image()+send took {call:?}"
        );
    }
}
