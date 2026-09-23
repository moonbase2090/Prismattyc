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

#[cfg(test)]
mod tests {
    use super::*;

    fn view(n: u8) -> ViewerId {
        ViewerId::from_bytes([n; 16])
    }

    #[test]
    fn deadlines_coalesce_and_expire_independently() {
        let mut bells = PaneBells::default();
        let now = Instant::now();
        let slot = PixelRect::new(0, 0, 30, 20);
        assert!(bells.is_empty());
        assert_eq!(bells.deadline(), None);
        assert!(bells.ring(1, view(1), Some("space"), slot, now));
        assert!(!bells.ring(
            1,
            view(1),
            Some("space"),
            slot,
            now + Duration::from_millis(50)
        ));
        assert!(bells.ring(
            2,
            view(2),
            Some("space"),
            slot,
            now + Duration::from_millis(60)
        ));
        assert_eq!(bells.deadline(), Some(now + Duration::from_millis(120)));
        assert!(
            !bells.settle(now + Duration::from_millis(119), Some("space"), |id| Some(
                (view(id as u8), slot)
            ))
        );
        assert!(
            bells.settle(now + Duration::from_millis(120), Some("space"), |id| Some(
                (view(id as u8), slot)
            ))
        );
        assert_eq!(bells.entries.len(), 1);
        assert_eq!(bells.deadline(), Some(now + Duration::from_millis(180)));
        assert!(
            bells.settle(now + Duration::from_millis(180), Some("space"), |id| Some(
                (view(id as u8), slot)
            ))
        );
        assert!(bells.is_empty());
    }

    #[test]
    fn cancelled_pixels_survive_until_cleanup_for_every_owner_change() {
        let slot = PixelRect::new(3, 4, 20, 15);
        for reason in 0..6 {
            let now = Instant::now();
            let mut bells = PaneBells::default();
            let original = vec![0x80335577; 40 * 30];
            let mut pixels = original.clone();
            bells.ring(1, view(1), Some("space"), slot, now);
            bells.paint(&mut pixels, 40);
            assert_ne!(pixels, original);
            let changed = match reason {
                0 => bells.cancel(),
                1 => bells.settle(now + Duration::from_millis(120), Some("space"), |_| {
                    Some((view(1), slot))
                }),
                2 => bells.settle(now, Some("other"), |_| Some((view(1), slot))),
                3 => bells.settle(now, Some("space"), |_| None),
                4 => bells.settle(now, Some("space"), |_| Some((view(2), slot))),
                _ => bells.settle(now, Some("space"), |_| {
                    Some((view(1), PixelRect::new(4, 4, 20, 15)))
                }),
            };
            assert!(changed);
            assert!(
                !bells.is_empty(),
                "captured cleanup must survive cancellation"
            );
            assert_eq!(bells.deadline(), None);
            assert!(!bells.cancel());
            let mut damage = FrameDamage::rects();
            bells.restore(&mut pixels, 40, &mut damage);
            assert_eq!(pixels, original);
            assert!(!damage.rects_slice().is_empty());
            assert!(bells.is_empty());
            let mut idle = FrameDamage::rects();
            bells.restore(&mut pixels, 40, &mut idle);
            assert!(idle.rects_slice().is_empty());
        }
    }

    #[test]
    fn perimeter_pixels_match_independent_oracle_and_keep_alpha() {
        for (w, h) in [(0, 0), (1, 1), (2, 3), (3, 4), (9, 13), (101, 63)] {
            let slot = PixelRect::new(5, 4, w, h);
            let stride = w + 12;
            let original: Vec<u32> = (0..stride * (h + 10))
                .map(|i| 0x80000000 | ((i as u32 * 7919) & 0xffffff))
                .collect();
            let mut pixels = original.clone();
            let mut bells = PaneBells::default();
            bells.ring(1, view(1), None, slot, Instant::now());
            let mut damage = FrameDamage::rects();
            bells.restore(&mut pixels, stride, &mut damage);
            bells.paint(&mut pixels, stride);
            for y in 0..h + 10 {
                for x in 0..stride {
                    let inside = x >= 5 && x < 5 + w && y >= 4 && y < 4 + h;
                    let edge = inside && (x < 7 || x + 2 >= 5 + w || y < 6 || y + 2 >= 4 + h);
                    let index = y * stride + x;
                    assert_eq!(
                        pixels[index],
                        original[index] ^ if edge { 0xffffff } else { 0 },
                        "{w}x{h} at {x},{y}"
                    );
                }
            }
            bells.cancel();
            bells.restore(&mut pixels, stride, &mut FrameDamage::rects());
            assert_eq!(pixels, original);
        }
    }

    #[test]
    fn full_repaint_and_resize_discard_stale_captured_pixels() {
        for resize in [false, true] {
            let mut bells = PaneBells::default();
            let mut pixels = vec![0x12345678; 40 * 30];
            bells.ring(
                1,
                view(1),
                None,
                PixelRect::new(2, 2, 20, 15),
                Instant::now(),
            );
            bells.paint(&mut pixels, 40);
            bells.cancel();
            pixels.fill(0xabcdef01);
            let mut damage = if resize {
                FrameDamage::rects()
            } else {
                FrameDamage::Full
            };
            bells.restore(&mut pixels, if resize { 30 } else { 40 }, &mut damage);
            assert!(pixels.iter().all(|pixel| *pixel == 0xabcdef01));
            assert!(bells.is_empty());
        }
    }
}
