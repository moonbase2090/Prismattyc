// SPDX-License-Identifier: MIT
//! Owned frame queues keep consumption, installation and resource lifetime distinct.

use super::{
    backing::ImageDescriptor,
    frame::FrameSnapshot,
    pool::ImagePool,
    profile::Dialect,
    registry::{ImageRegistry, RegistryError, ResolvedPlacement},
    retirement::{ProgressError, SceneProgress},
    ring::{RingContext, RingError, SharedRing},
    wire12::{FrameSequence, Reply, ResourceHandle, Retirement, UploadState, RING_SLOTS},
};
use std::{collections::VecDeque, io, os::fd::BorrowedFd, sync::Arc, time::Instant};

pub struct NativeScene {
    frame: FrameSnapshot,
    images: Vec<ResolvedPlacement>,
    blinks: bool,
}

impl NativeScene {
    pub fn blinks(&self) -> bool {
        self.blinks
    }
    pub fn grid(&self) -> super::grid::NativeGrid<'_> {
        super::grid::NativeGrid::new(&self.frame)
    }
    pub fn frame(&self) -> &FrameSnapshot {
        &self.frame
    }
    /// Stable depth/ordinal order for both negative and nonnegative image passes.
    pub fn images(&self) -> &[ResolvedPlacement] {
        &self.images
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum SceneError {
    Ring(RingError),
    Resource(RegistryError),
    Progress(ProgressError),
}

pub struct SessionFrames {
    ring: SharedRing,
    resources: ImageRegistry,
    pending: VecDeque<Arc<NativeScene>>,
    installed: Option<Arc<NativeScene>>,
    progress: SceneProgress,
}

impl SessionFrames {
    pub fn new(context: RingContext, pool: Arc<ImagePool>) -> io::Result<Self> {
        Self::create_for(context, pool, Dialect::Images2)
    }
    pub fn new_legacy(context: RingContext, pool: Arc<ImagePool>) -> io::Result<Self> {
        Self::create_for(context, pool, Dialect::Grid0)
    }
    fn create_for(
        context: RingContext,
        pool: Arc<ImagePool>,
        dialect: Dialect,
    ) -> io::Result<Self> {
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(RING_SLOTS as usize)
            .map_err(io::Error::other)?;
        Ok(Self {
            ring: match dialect {
                Dialect::Grid0 => SharedRing::create_legacy(context)?,
                Dialect::Images2 => SharedRing::create(context)?,
            },
            resources: ImageRegistry::new(context.session, context.epoch, pool),
            pending,
            installed: None,
            progress: SceneProgress::default(),
        })
    }

    pub fn descriptor(&self) -> BorrowedFd<'_> {
        self.ring.descriptor()
    }
    pub fn physical_bytes(&self) -> u64 {
        self.ring.physical_bytes()
    }
    pub fn installed(&self) -> Option<&Arc<NativeScene>> {
        self.installed.as_ref()
    }

    pub fn put(&mut self, source: ImageDescriptor, now: Instant) -> Result<Reply, RegistryError> {
        self.resources.put(source, now)
    }
    pub fn pong(&mut self, now: Instant) -> Result<UploadState, RegistryError> {
        self.resources.pong(now)
    }
    #[cfg(test)]
    pub(crate) fn finish_upload_for_test(&mut self) -> Result<(), RegistryError> {
        self.resources.finish_for_test()
    }
    pub fn retire(
        &mut self,
        handle: ResourceHandle,
        omitting: FrameSequence,
    ) -> Result<Retirement, RegistryError> {
        self.resources.retire(handle, omitting, self.progress)
    }

    /// Copies at most one slot. An unresolved resource leaves the consumer unacknowledged.
    pub fn prepare_next(&mut self) -> Result<bool, SceneError> {
        if self.pending.len() == RING_SLOTS as usize {
            return Ok(false);
        }
        let Some(copied) = self.ring.copy_next().map_err(SceneError::Ring)? else {
            return Ok(false);
        };
        let mut images = self
            .resources
            .resolve(
                copied.snapshot.context.sequence,
                &copied.snapshot.placements,
            )
            .map_err(SceneError::Resource)?;
        images.sort_unstable_by_key(|item| (item.placement.depth, item.placement.ordinal));
        let blinks = copied.snapshot.cells.iter().any(|cell| {
            let ink = match cell.glyph {
                super::frame::Glyph::Continuation => false,
                super::frame::Glyph::Scalar(character) => !character.is_whitespace(),
                super::frame::Glyph::Cluster(index) => copied.snapshot.clusters[index as usize]
                    .chars()
                    .any(|character| !character.is_whitespace()),
            };
            cell.width() != 0
                && cell.meta & 0x140 == 0x40
                && (ink || cell.meta & (0x38 | 0x600) != 0)
        });
        let scene = Arc::new(NativeScene {
            frame: copied.snapshot,
            images,
            blinks,
        });
        let consumed = self
            .ring
            .acknowledge(copied.receipt)
            .map_err(SceneError::Ring)?;
        self.progress
            .consume(consumed)
            .map_err(SceneError::Progress)?;
        self.pending.push_back(scene);
        Ok(true)
    }

    /// Publish the same immutable scene used by every native reader, before ACTIVE.
    pub fn install_next(&mut self) -> Result<Option<FrameSequence>, SceneError> {
        let Some(scene) = self.pending.front() else {
            return Ok(None);
        };
        let sequence = scene.frame.context.sequence;
        self.progress
            .install(sequence)
            .map_err(SceneError::Progress)?;
        self.installed = self.pending.pop_front();
        Ok(Some(sequence))
    }
}

#[cfg(test)]
#[path = "scene_tests.rs"]
pub(crate) mod tests;
