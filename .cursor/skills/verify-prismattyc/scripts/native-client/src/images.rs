// SPDX-License-Identifier: MIT
use crate::frame::Placement;
use lunatui_core::Rect;
use lunatui_image::{ImageRequest, ImageSink, ImageSource, PixelSize, PixelTarget};
use std::collections::BTreeMap;

pub type Key = (u32, u64);

enum Resource {
    Pending(u64),
    Ready(u64),
    Declined,
    Retiring(u64),
}

pub struct Images {
    cell: [u32; 2],
    resources: BTreeMap<Key, Resource>,
    requested: Option<(Key, ImageSource)>,
    placements: Vec<Placement>,
    placed_handles: Vec<u64>,
    placed_ids: Vec<u32>,
}

impl Images {
    pub fn new(cell: [u32; 2]) -> Self {
        Self {
            cell,
            resources: BTreeMap::new(),
            requested: None,
            placements: Vec::new(),
            placed_handles: Vec::new(),
            placed_ids: Vec::new(),
        }
    }
    pub fn begin_frame(&mut self) {
        self.placements.clear();
        self.placed_handles.clear();
        self.placed_ids.clear();
    }
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }
    pub fn pending(&self) -> bool {
        self.resources
            .values()
            .any(|resource| matches!(resource, Resource::Pending(_)))
    }
    pub fn decline_preparation(&mut self, key: Key) {
        self.resources.insert(key, Resource::Declined);
    }
    pub fn take_request(&mut self) -> Option<(Key, ImageSource)> {
        self.requested
            .take()
            .filter(|(key, _)| !self.resources.contains_key(key))
    }

    pub fn admission(&mut self, key: Key, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.len() != 48 || crate::u16_at(bytes, 4) != 11 || crate::u32_at(bytes, 44) != 0 {
            return Err(crate::invalid("image result"));
        }
        let handle = crate::u64_at(bytes, 32);
        let status = crate::u32_at(bytes, 40);
        let resource = match (status, handle) {
            (0, handle) if handle != 0 => Resource::Pending(handle),
            (1, handle) if handle != 0 => Resource::Ready(handle),
            (2..=5, 0) => Resource::Declined,
            _ => return Err(crate::invalid("image admission status")),
        };
        self.resources.insert(key, resource);
        Ok(())
    }

    /// A READY response is the only path from PENDING to visible placement.
    pub fn pong(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        if bytes.len() != 48 || crate::u16_at(bytes, 4) != 15 {
            return Err(crate::invalid("image heartbeat"));
        }
        let handle = crate::u64_at(bytes, 32);
        let state = crate::u32_at(bytes, 40);
        let error = crate::u32_at(bytes, 44);
        if !matches!(
            (state, handle, error),
            (0, 0, 0) | (1 | 2, 1.., 0) | (3, 1.., 1..=3)
        ) {
            return Err(crate::invalid("image heartbeat state"));
        }
        let mut changed = false;
        for resource in self.resources.values_mut() {
            if matches!(resource, Resource::Pending(value) if *value == handle) {
                match state {
                    2 => {
                        *resource = Resource::Ready(handle);
                        changed = true;
                    }
                    3 => {
                        *resource = Resource::Declined;
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        Ok(changed)
    }

    pub fn retirement(&mut self) -> Option<(Key, u64)> {
        self.resources
            .iter_mut()
            .find_map(|(key, resource)| match *resource {
                Resource::Ready(handle) if !self.placed_handles.contains(&handle) => {
                    *resource = Resource::Retiring(handle);
                    Some((*key, handle))
                }
                Resource::Retiring(handle) => Some((*key, handle)),
                _ => None,
            })
    }

    pub fn retired(&mut self, key: Key, bytes: &[u8]) -> std::io::Result<bool> {
        if bytes.len() != 48 || crate::u16_at(bytes, 4) != 13 || crate::u32_at(bytes, 44) != 0 {
            return Err(crate::invalid("image retirement reply"));
        }
        let Some(Resource::Retiring(handle)) = self.resources.get(&key) else {
            return Err(crate::invalid("retirement identity"));
        };
        if *handle != crate::u64_at(bytes, 32) {
            return Err(crate::invalid("retirement handle"));
        }
        match crate::u32_at(bytes, 40) {
            0 => {
                self.resources.remove(&key);
                Ok(true)
            }
            1 => Ok(false),
            _ => Err(crate::invalid("retirement status")),
        }
    }
}

impl ImageSink for Images {
    fn supports_pixels(&self) -> bool {
        true
    }

    fn pixel_target(&self, area: Rect) -> Option<PixelTarget> {
        if self.placements.len() >= 256 {
            return None;
        }
        let width = u32::from(area.width).checked_mul(self.cell[0])?;
        let height = u32::from(area.height).checked_mul(self.cell[1])?;
        if width > 4096 || height > 4096 || width.checked_mul(height)? > 1_048_576 {
            return None;
        }
        Some(PixelTarget::new(
            PixelSize::new(width, height)?,
            true,
            Some([0, 0, 0]),
        ))
    }

    fn submit(&mut self, request: ImageRequest<'_>) -> bool {
        let key = (request.id().get(), request.generation());
        let handle = match self.resources.get(&key) {
            Some(Resource::Ready(handle)) => *handle,
            Some(_) => return false,
            None => {
                // One coalesced candidate. A renderer repeats declined/pending images
                // on the next scheduled render and keeps its actual fallback cells.
                self.requested = Some((key, request.source().clone()));
                return false;
            }
        };
        if self.placements.len() >= 256 || self.placed_ids.contains(&key.0) {
            return false;
        }
        let Ok(placement) =
            Placement::from_request(request, handle, self.cell, self.placements.len() as u32)
        else {
            return false;
        };
        self.placements.push(placement);
        self.placed_handles.push(handle);
        self.placed_ids.push(key.0);
        true
    }
}
