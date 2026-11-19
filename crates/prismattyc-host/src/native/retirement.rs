// SPDX-License-Identifier: MIT
//! Retirement fences retain references from frames preceding the omission.

use super::wire12::{FrameSequence, Retirement};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SceneProgress {
    consumed: u64,
    installed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressError {
    ConsumeOrder,
    InstallOrder,
    UnconsumedInstall,
}

impl SceneProgress {
    pub fn consume(&mut self, frame: FrameSequence) -> Result<(), ProgressError> {
        if self.consumed.checked_add(1) != Some(frame.get()) {
            return Err(ProgressError::ConsumeOrder);
        }
        self.consumed = frame.get();
        Ok(())
    }

    /// Called after the scene was installed, including transient scenes coalesced before presentation.
    pub fn install(&mut self, frame: FrameSequence) -> Result<(), ProgressError> {
        if self.installed.checked_add(1) != Some(frame.get()) {
            return Err(ProgressError::InstallOrder);
        }
        if frame.get() > self.consumed {
            return Err(ProgressError::UnconsumedInstall);
        }
        self.installed = frame.get();
        Ok(())
    }

    fn reached(self, frame: FrameSequence) -> bool {
        self.consumed >= frame.get() && self.installed >= frame.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetirementError {
    OmissionStillReferencesResource,
    ReferenceAfterOmission,
}

#[derive(Debug, Default)]
pub struct RetirementFence {
    last_reference: Option<FrameSequence>,
    forbidden_from: Option<FrameSequence>,
}

impl RetirementFence {
    /// Called only after a complete owned frame has passed validation.
    pub fn record_reference(&mut self, frame: FrameSequence) -> Result<(), RetirementError> {
        if self.forbidden_from.is_some_and(|first| frame >= first) {
            return Err(RetirementError::ReferenceAfterOmission);
        }
        self.last_reference = self.last_reference.max(Some(frame));
        Ok(())
    }

    /// On Retired the caller removes its registry entry before processing more work.
    pub fn request<T: ?Sized>(
        &mut self,
        omitting_frame: FrameSequence,
        progress: SceneProgress,
        pixels: &Arc<T>,
    ) -> Result<Retirement, RetirementError> {
        if self
            .last_reference
            .is_some_and(|last| last >= omitting_frame)
        {
            return Err(RetirementError::OmissionStillReferencesResource);
        }
        self.forbidden_from = Some(
            self.forbidden_from
                .map_or(omitting_frame, |first| first.min(omitting_frame)),
        );
        if progress.reached(omitting_frame) && Arc::strong_count(pixels) == 1 {
            Ok(Retirement::Retired)
        } else {
            Ok(Retirement::Busy)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u64) -> FrameSequence {
        FrameSequence::new(value).unwrap()
    }
    fn progress(consumed: u64, installed: u64) -> SceneProgress {
        let mut progress = SceneProgress::default();
        for sequence in 1..=consumed {
            progress.consume(frame(sequence)).unwrap();
        }
        for sequence in 1..=installed {
            progress.install(frame(sequence)).unwrap();
        }
        progress
    }

    #[test]
    fn retirement_waits_for_both_consumption_and_actual_installation() {
        let pixels = Arc::new([255, 0, 0, 128]);
        let mut fence = RetirementFence::default();
        fence.record_reference(frame(2)).unwrap();
        assert_eq!(
            fence.request(frame(3), progress(2, 2), &pixels),
            Ok(Retirement::Busy)
        );
        assert_eq!(
            fence.request(frame(3), progress(3, 2), &pixels),
            Ok(Retirement::Busy)
        );
        assert_eq!(
            fence.request(frame(3), progress(3, 3), &pixels),
            Ok(Retirement::Retired)
        );
    }

    #[test]
    fn an_owned_scene_reference_keeps_a_ready_resource_busy() {
        let pixels = Arc::new([255, 0, 0, 128]);
        let held_scene_pixels = Arc::clone(&pixels);
        let mut fence = RetirementFence::default();
        assert_eq!(
            fence.request(frame(3), progress(3, 3), &pixels),
            Ok(Retirement::Busy)
        );
        drop(held_scene_pixels);
        assert_eq!(
            fence.request(frame(3), progress(3, 3), &pixels),
            Ok(Retirement::Retired)
        );
    }

    #[test]
    fn queued_references_before_the_omission_remain_valid() {
        let pixels = Arc::new([255, 0, 0, 128]);
        let mut fence = RetirementFence::default();
        assert_eq!(
            fence.request(frame(3), progress(1, 1), &pixels),
            Ok(Retirement::Busy)
        );
        fence.record_reference(frame(2)).unwrap();
        assert_eq!(
            fence.record_reference(frame(3)),
            Err(RetirementError::ReferenceAfterOmission)
        );
        assert_eq!(
            fence.record_reference(frame(4)),
            Err(RetirementError::ReferenceAfterOmission)
        );
    }

    #[test]
    fn retries_keep_the_first_reference_fence_and_wait_for_the_named_omission() {
        let pixels = Arc::new([255, 0, 0, 128]);
        let mut fence = RetirementFence::default();
        assert_eq!(
            fence.request(frame(3), progress(2, 2), &pixels),
            Ok(Retirement::Busy)
        );
        assert_eq!(
            fence.request(frame(4), progress(3, 3), &pixels),
            Ok(Retirement::Busy)
        );
        assert_eq!(
            fence.record_reference(frame(3)),
            Err(RetirementError::ReferenceAfterOmission)
        );
        assert_eq!(
            fence.request(frame(4), progress(4, 4), &pixels),
            Ok(Retirement::Retired)
        );
    }

    #[test]
    fn a_named_omission_cannot_precede_the_latest_referenced_frame() {
        let pixels = Arc::new([255, 0, 0, 128]);
        let mut fence = RetirementFence::default();
        fence.record_reference(frame(3)).unwrap();
        for omitted in [2, 3] {
            assert_eq!(
                fence.request(frame(omitted), progress(3, 3), &pixels),
                Err(RetirementError::OmissionStillReferencesResource)
            );
        }
    }

    #[test]
    fn installation_progress_cannot_outrun_consumption() {
        let mut progress = SceneProgress::default();
        assert_eq!(
            progress.install(frame(1)),
            Err(ProgressError::UnconsumedInstall)
        );
        progress.consume(frame(1)).unwrap();
        progress.install(frame(1)).unwrap();
        assert_eq!(progress.consume(frame(3)), Err(ProgressError::ConsumeOrder));
        progress.consume(frame(2)).unwrap();
        assert_eq!(progress.install(frame(3)), Err(ProgressError::InstallOrder));
        progress.install(frame(2)).unwrap();
    }
}
