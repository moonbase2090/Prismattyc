// SPDX-License-Identifier: MIT
//! Admitted uploads copy and hash owned chunks without blocking the event loop.

use super::{
    pool::{AdmittedSource, PixelCredit, UploadCredits},
    wire12::{PtyEpoch, ResourceHandle, SessionId, UploadFailure},
};
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroU32,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const CHUNK: usize = 64 * 1024;
const JOB_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadIdentity {
    pub session: SessionId,
    pub epoch: PtyEpoch,
    pub handle: ResourceHandle,
}

pub struct OwnedImage {
    width: NonZeroU32,
    height: NonZeroU32,
    pixels: Box<[u8]>,
    _credit: PixelCredit,
}

impl OwnedImage {
    pub fn width(&self) -> u32 {
        self.width.get()
    }
    pub fn height(&self) -> u32 {
        self.height.get()
    }
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

pub enum UploadOutcome {
    Ready(Arc<OwnedImage>),
    Failed(UploadFailure),
    DigestMismatch,
    Cancelled,
}

pub struct CompletedUpload {
    pub identity: UploadIdentity,
    pub outcome: UploadOutcome,
}

pub struct UploadWorker {
    identity: UploadIdentity,
    cancelled: Arc<AtomicBool>,
    thread: Option<JoinHandle<UploadOutcome>>,
}

enum Clock {
    System,
    #[cfg(test)]
    Manual(Arc<std::sync::Mutex<Instant>>),
}

impl Clock {
    fn now(&self) -> Instant {
        match self {
            Self::System => Instant::now(),
            #[cfg(test)]
            Self::Manual(time) => *time.lock().unwrap(),
        }
    }
}

struct Control {
    clock: Clock,
    #[cfg(test)]
    hooks: Option<CopyHooks>,
    #[cfg(test)]
    exited: Option<std::sync::mpsc::Sender<bool>>,
}

impl Control {
    fn realtime() -> Self {
        Self {
            clock: Clock::System,
            #[cfg(test)]
            hooks: None,
            #[cfg(test)]
            exited: None,
        }
    }
}

#[cfg(test)]
struct CopyHooks {
    copied: std::sync::mpsc::Sender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
struct ExitObservation {
    sender: Option<std::sync::mpsc::Sender<bool>>,
    cancelled: bool,
}

#[cfg(test)]
impl Drop for ExitObservation {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(self.cancelled);
        }
    }
}

impl UploadWorker {
    pub fn start(
        source: AdmittedSource,
        identity: UploadIdentity,
        admitted_at: Instant,
    ) -> Result<Self, UploadFailure> {
        Self::start_inner(source, identity, admitted_at, Control::realtime())
    }

    fn start_inner(
        source: AdmittedSource,
        identity: UploadIdentity,
        admitted_at: Instant,
        control: Control,
    ) -> Result<Self, UploadFailure> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let thread = std::thread::Builder::new()
            .name("native-image-copy".into())
            .spawn(move || {
                #[cfg(test)]
                let mut observation = ExitObservation {
                    sender: control.exited,
                    cancelled: false,
                };
                let (descriptor, credits) = source.into_parts();
                let UploadCredits {
                    pixels,
                    source,
                    worker,
                } = credits;
                // The permit outlives mapping/FD cleanup. A finished join handle is
                // required before this session admits another worker.
                let _worker = worker;
                let _source_credit = source;
                let metadata = descriptor.metadata();
                let outcome = (|| {
                    check(&worker_cancelled, &control.clock, admitted_at)?;
                    let mapping = descriptor
                        .map()
                        .map_err(|_| UploadOutcome::Failed(UploadFailure::Read))?;
                    let length = mapping.logical_bytes();
                    let mut owned = Vec::new();
                    owned
                        .try_reserve_exact(length)
                        .map_err(|_| UploadOutcome::Failed(UploadFailure::Allocation))?;
                    let mut digest = Sha256::new();
                    for offset in (0..length).step_by(CHUNK) {
                        check(&worker_cancelled, &control.clock, admitted_at)?;
                        let end = offset.saturating_add(CHUNK).min(length);
                        owned.resize(end, 0);
                        check(&worker_cancelled, &control.clock, admitted_at)?;
                        mapping
                            .copy_chunk(offset, &mut owned[offset..end])
                            .map_err(|_| UploadOutcome::Failed(UploadFailure::Read))?;
                        #[cfg(test)]
                        if let Some(hooks) = &control.hooks {
                            hooks
                                .copied
                                .send(())
                                .map_err(|_| UploadOutcome::Cancelled)?;
                            // The observing test owns the sender and its health
                            // bound; unwinding that test also releases this wait.
                            hooks.proceed.recv().map_err(|_| UploadOutcome::Cancelled)?;
                        }
                        check(&worker_cancelled, &control.clock, admitted_at)?;
                        digest.update(&owned[offset..end]);
                    }
                    check(&worker_cancelled, &control.clock, admitted_at)?;
                    let actual: [u8; 32] = digest.finalize().into();
                    check(&worker_cancelled, &control.clock, admitted_at)?;
                    if actual != metadata.digest {
                        return Err(UploadOutcome::DigestMismatch);
                    }
                    Ok(Arc::new(OwnedImage {
                        width: metadata.width,
                        height: metadata.height,
                        pixels: owned.into_boxed_slice(),
                        _credit: pixels,
                    }))
                })();
                let result = match outcome {
                    Ok(pixels) => UploadOutcome::Ready(pixels),
                    Err(failure) => failure,
                };
                #[cfg(test)]
                {
                    observation.cancelled = matches!(result, UploadOutcome::Cancelled);
                }
                result
            })
            .map_err(|_| UploadFailure::Allocation)?;
        Ok(Self {
            identity,
            cancelled,
            thread: Some(thread),
        })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Called on the event loop's bounded pending-work tick; never waits.
    pub fn take_finished(&mut self) -> Option<CompletedUpload> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        Some(self.join_finished())
    }

    fn join_finished(&mut self) -> CompletedUpload {
        let outcome = self
            .thread
            .take()
            .unwrap()
            .join()
            .unwrap_or(UploadOutcome::Failed(UploadFailure::Read));
        CompletedUpload {
            identity: self.identity,
            outcome,
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_test(&mut self) -> CompletedUpload {
        self.join_finished()
    }

    #[cfg(test)]
    fn finish(mut self) -> CompletedUpload {
        self.join_finished()
    }
}

impl Drop for UploadWorker {
    fn drop(&mut self) {
        self.cancel();
        // Dropping a join handle detaches: cancellation never joins on the UI.
        // The thread retains its source/allocation credits and worker permit.
    }
}

fn check(cancelled: &AtomicBool, clock: &Clock, admitted_at: Instant) -> Result<(), UploadOutcome> {
    if cancelled.load(Ordering::Acquire) {
        return Err(UploadOutcome::Cancelled);
    }
    if clock.now().saturating_duration_since(admitted_at) >= JOB_DEADLINE {
        return Err(UploadOutcome::Failed(UploadFailure::Deadline));
    }
    Ok(())
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
