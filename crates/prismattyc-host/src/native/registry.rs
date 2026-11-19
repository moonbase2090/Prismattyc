// SPDX-License-Identifier: MIT
//! Session-owned bindings connect admitted workers to immutable scene pixels.

use super::{
    backing::ImageDescriptor,
    frame::Placement,
    pool::ImagePool,
    retirement::{RetirementError, RetirementFence, SceneProgress},
    upload::{CompletedUpload, OwnedImage, UploadIdentity, UploadOutcome, UploadWorker},
    wire12::{
        Admission, Decline, FrameSequence, ImageUpload, PtyEpoch, Reply, ResourceHandle,
        Retirement, SessionId, UploadFailure, UploadState, MAX_RESOURCES, SESSION_IMAGE_BYTES,
    },
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    ChangedLiveKey,
    HandleExhausted,
    CompletionIdentity,
    DigestMismatch,
    UnknownHandle,
    UnreadyHandle,
    InvalidCrop,
    Allocation,
    Retirement(RetirementError),
}

enum Resource {
    Pending,
    Ready {
        pixels: Arc<OwnedImage>,
        fence: RetirementFence,
    },
}

struct Entry {
    binding: ImageUpload,
    handle: ResourceHandle,
    resource: Resource,
}

struct Running {
    worker: UploadWorker,
    handle: ResourceHandle,
    admitted_at: Instant,
    deadline_reported: bool,
}

pub struct ResolvedPlacement {
    pub placement: Placement,
    pub pixels: Arc<OwnedImage>,
}

pub struct ImageRegistry {
    session: SessionId,
    epoch: PtyEpoch,
    pool: Arc<ImagePool>,
    entries: Vec<Entry>,
    running: Option<Running>,
    next_handle: Option<ResourceHandle>,
    report: UploadState,
}

impl ImageRegistry {
    pub fn new(session: SessionId, epoch: PtyEpoch, pool: Arc<ImagePool>) -> Self {
        Self {
            session,
            epoch,
            pool,
            entries: Vec::new(),
            running: None,
            next_handle: ResourceHandle::new(1),
            report: UploadState::Idle,
        }
    }

    pub fn put(&mut self, source: ImageDescriptor, now: Instant) -> Result<Reply, RegistryError> {
        self.tick(now)?;
        let metadata = source.metadata();
        if let Some(entry) = self.entries.iter().find(|entry| {
            entry.binding.image_id == metadata.image_id
                && entry.binding.source_generation == metadata.source_generation
        }) {
            // Physical rounding is a property of this FD, not image identity.
            if entry.binding.width != metadata.width
                || entry.binding.height != metadata.height
                || entry.binding.digest != metadata.digest
            {
                return Err(RegistryError::ChangedLiveKey);
            }
            let status = match entry.resource {
                Resource::Ready { .. } => Admission::ExistingReady,
                Resource::Pending => {
                    if self
                        .running
                        .as_ref()
                        .is_some_and(|job| job.deadline_reported)
                    {
                        return Ok(Reply::Declined(Decline::Busy));
                    }
                    Admission::Pending
                }
            };
            return Ok(Reply::Admitted {
                handle: entry.handle,
                status,
            });
        }
        if self.running.is_some() {
            return Ok(Reply::Declined(Decline::Busy));
        }
        if self.entries.len() >= MAX_RESOURCES as usize {
            return Ok(Reply::Declined(Decline::HandleBudget));
        }
        let bytes: u64 = self
            .entries
            .iter()
            .map(|entry| entry.binding.logical_bytes)
            .sum();
        if bytes
            .checked_add(metadata.logical_bytes)
            .is_none_or(|total| total > SESSION_IMAGE_BYTES)
        {
            return Ok(Reply::Declined(Decline::ByteBudget));
        }
        let handle = self.next_handle.ok_or(RegistryError::HandleExhausted)?;
        if self.entries.try_reserve(1).is_err() {
            return Ok(Reply::Declined(Decline::Allocation));
        }
        let source = match self.pool.admit(source) {
            Ok(source) => source,
            Err(reason) => return Ok(Reply::Declined(reason)),
        };
        let identity = UploadIdentity {
            session: self.session,
            epoch: self.epoch,
            handle,
        };
        let worker = match UploadWorker::start(source, identity, now) {
            Ok(worker) => worker,
            Err(_) => return Ok(Reply::Declined(Decline::Allocation)),
        };
        self.entries.push(Entry {
            binding: metadata,
            handle,
            resource: Resource::Pending,
        });
        self.running = Some(Running {
            worker,
            handle,
            admitted_at: now,
            deadline_reported: false,
        });
        self.next_handle = handle.get().checked_add(1).and_then(ResourceHandle::new);
        self.report = UploadState::Pending(handle);
        Ok(Reply::Admitted {
            handle,
            status: Admission::Pending,
        })
    }

    /// Called before control requests and on the event loop's pending-work tick.
    pub fn tick(&mut self, now: Instant) -> Result<(), RegistryError> {
        let Some(job) = &mut self.running else {
            return Ok(());
        };
        if now.saturating_duration_since(job.admitted_at) >= DEADLINE {
            job.deadline_reported = true;
            job.worker.cancel();
            self.report = UploadState::Failed {
                handle: job.handle,
                error: UploadFailure::Deadline,
            };
        }
        if let Some(completed) = job.worker.take_finished() {
            let job = self.running.take().unwrap();
            self.complete(job, completed)?;
        }
        Ok(())
    }

    fn complete(&mut self, job: Running, completed: CompletedUpload) -> Result<(), RegistryError> {
        if completed.identity
            != (UploadIdentity {
                session: self.session,
                epoch: self.epoch,
                handle: job.handle,
            })
        {
            return Err(RegistryError::CompletionIdentity);
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.handle == job.handle)
            .ok_or(RegistryError::CompletionIdentity)?;
        if !matches!(self.entries[index].resource, Resource::Pending) {
            return Err(RegistryError::CompletionIdentity);
        }
        if job.deadline_reported {
            self.entries.swap_remove(index);
            return Ok(());
        }
        match completed.outcome {
            UploadOutcome::Ready(pixels) => {
                self.entries[index].resource = Resource::Ready {
                    pixels,
                    fence: RetirementFence::default(),
                };
                self.report = UploadState::Ready(job.handle);
            }
            UploadOutcome::DigestMismatch => {
                self.entries.swap_remove(index);
                return Err(RegistryError::DigestMismatch);
            }
            UploadOutcome::Failed(error) => {
                self.entries.swap_remove(index);
                self.report = UploadState::Failed {
                    handle: job.handle,
                    error,
                };
            }
            UploadOutcome::Cancelled => {
                self.entries.swap_remove(index);
                self.report = UploadState::Failed {
                    handle: job.handle,
                    error: UploadFailure::Read,
                };
            }
        }
        Ok(())
    }

    pub fn pong(&mut self, now: Instant) -> Result<UploadState, RegistryError> {
        self.tick(now)?;
        Ok(self.report)
    }

    #[cfg(test)]
    pub(super) fn finish_for_test(&mut self) -> Result<(), RegistryError> {
        let mut job = self.running.take().unwrap();
        let completed = job.worker.wait_for_test();
        self.complete(job, completed)
    }

    /// Resolve all ownership, crop and retirement constraints before ring acknowledgment.
    pub fn resolve(
        &mut self,
        frame: FrameSequence,
        placements: &[Placement],
    ) -> Result<Vec<ResolvedPlacement>, RegistryError> {
        let mut resolved = Vec::new();
        resolved
            .try_reserve_exact(placements.len())
            .map_err(|_| RegistryError::Allocation)?;
        for placement in placements {
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.handle == placement.handle)
                .ok_or(RegistryError::UnknownHandle)?;
            let Resource::Ready { pixels, .. } = &entry.resource else {
                return Err(RegistryError::UnreadyHandle);
            };
            if !placement.crop_fits(pixels.width(), pixels.height()) {
                return Err(RegistryError::InvalidCrop);
            }
            resolved.push(ResolvedPlacement {
                placement: *placement,
                pixels: Arc::clone(pixels),
            });
        }
        for placement in placements {
            let entry = self
                .entries
                .iter_mut()
                .find(|entry| entry.handle == placement.handle)
                .unwrap();
            let Resource::Ready { fence, .. } = &mut entry.resource else {
                unreachable!()
            };
            fence
                .record_reference(frame)
                .map_err(RegistryError::Retirement)?;
        }
        Ok(resolved)
    }

    pub fn retire(
        &mut self,
        handle: ResourceHandle,
        omitting: FrameSequence,
        progress: SceneProgress,
    ) -> Result<Retirement, RegistryError> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.handle == handle)
            .ok_or(RegistryError::UnknownHandle)?;
        let Resource::Ready { pixels, fence } = &mut self.entries[index].resource else {
            return Err(RegistryError::UnreadyHandle);
        };
        let status = fence
            .request(omitting, progress, pixels)
            .map_err(RegistryError::Retirement)?;
        if status == Retirement::Retired {
            self.entries.swap_remove(index);
            if self.report == UploadState::Ready(handle) {
                self.report = UploadState::Idle;
            }
        }
        Ok(status)
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
