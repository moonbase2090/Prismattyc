// SPDX-License-Identifier: MPL-2.0
//! A static, pane-owned visual bell with one cleanup deadline.
use prismattyc_protocol::ViewerId;
use std::time::{Duration, Instant};

use crate::border_underlay::BorderUnderlay;
use crate::frame_damage::{border_strips, FrameDamage, PixelRect};

#[derive(Default)]
pub(crate) struct PaneBells {
    entries: Vec<Entry>,
}

struct Entry {
    pane: u64,
    view: ViewerId,
    space: Option<String>,
    slot: PixelRect,
    until: Option<Instant>,
    underlay: BorderUnderlay,
    captured: bool,
}

impl PaneBells {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn ring(
        &mut self,
        pane: u64,
        view: ViewerId,
        space: Option<&str>,
        slot: PixelRect,
        now: Instant,
    ) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.pane == pane
                && entry.view == view
                && entry.space.as_deref() == space
                && entry.slot == slot
        }) {
            if entry.until.is_some_and(|until| until > now) {
                return false;
            }
            entry.until = Some(now + Duration::from_millis(120));
        } else {
            self.entries.push(Entry {
                pane,
                view,
                space: space.map(str::to_owned),
                slot,
                until: Some(now + Duration::from_millis(120)),
                underlay: BorderUnderlay::default(),
                captured: false,
            });
        }
        true
    }

    /// Cancel invisible/replaced owners and expired holds. Keep captured pixels
    /// until raster restores them; cancellation itself never discards cleanup.
    pub(crate) fn settle(
        &mut self,
        now: Instant,
        space: Option<&str>,
        mut visible_slot: impl FnMut(u64) -> Option<(ViewerId, PixelRect)>,
    ) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.until.is_some_and(|until| {
                now >= until
                    || entry.space.as_deref() != space
                    || visible_slot(entry.pane) != Some((entry.view, entry.slot))
            }) {
                entry.until = None;
                changed = true;
            }
        }
        self.entries
            .retain(|entry| entry.until.is_some() || entry.captured);
        changed
    }

    pub(crate) fn cancel(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            changed |= entry.until.take().is_some();
        }
        self.entries
            .retain(|entry| entry.until.is_some() || entry.captured);
        changed
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.entries.iter().filter_map(|entry| entry.until).min()
    }

    /// Called before all other underlay restoration and content/scroll paint.
    pub(crate) fn restore(&mut self, buffer: &mut [u32], stride: usize, damage: &mut FrameDamage) {
        for entry in &mut self.entries {
            entry.underlay.restore(buffer, stride, damage);
            entry.captured = false;
            if entry.until.is_some() {
                for rect in border_strips(entry.slot) {
                    damage.push_rect(rect);
                }
            }
        }
        self.entries.retain(|entry| entry.until.is_some());
    }

    /// Invert only the two-pixel perimeter. No intermediate animation frames,
    /// cell rasterization, or change to the underlying alpha channel.
    pub(crate) fn paint(&mut self, buffer: &mut [u32], stride: usize) {
        if stride == 0 {
            return;
        }
        for entry in &mut self.entries {
            entry.underlay.capture(buffer, stride, entry.slot);
            entry.captured = true;
            let slot = entry.slot;
            let top = 2.min(slot.height);
            let bottom = 2.min(slot.height - top);
            let left = 2.min(slot.width);
            let right = 2.min(slot.width - left);
            let middle = slot.height - top - bottom;
            let rects = [
                PixelRect::new(slot.x, slot.y, slot.width, top),
                PixelRect::new(slot.x, slot.y + top, left, middle),
                PixelRect::new(slot.x + slot.width - right, slot.y + top, right, middle),
                PixelRect::new(slot.x, slot.y + slot.height - bottom, slot.width, bottom),
            ];
            for rect in rects {
                let Some(rect) = rect.clipped(stride, buffer.len() / stride) else {
                    continue;
                };
                for y in rect.y..rect.y + rect.height {
                    for pixel in &mut buffer[y * stride + rect.x..y * stride + rect.x + rect.width]
                    {
                        *pixel ^= 0x00ff_ffff;
                    }
                }
            }
        }
    }
}
