// SPDX-License-Identifier: MPL-2.0
//! Retain only pixels covered by a transient border, including its head.
use crate::frame_damage::{border_strips, frame_damage_intersects, FrameDamage, PixelRect};

/// Pixels outside a Graphite slot covered by the focus ring and sweep head.
/// The settled ring starts one pixel outside the slot and shades one pixel
/// past that; the head stamp reaches about two pixels past its sample.
const GRAPHITE_RING_OUTSET: usize = 3;

/// Which rings the next Graphite stroke should paint.
///
/// `All` is a full frame or the flag-off path. `Slots` is the rings whose
/// captured strips were restored on this partial frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BorderRefresh {
    All,
    Slots(Vec<PixelRect>),
}

impl Default for BorderRefresh {
    fn default() -> Self {
        Self::All
    }
}

struct CapturedSlot {
    slot: PixelRect,
    rects: Vec<PixelRect>,
    pixels: Vec<u32>,
}

#[derive(Default)]
pub(crate) struct BorderUnderlay {
    slots: Vec<CapturedSlot>,
    stride: usize,
    len: usize,
    /// Set by the last restore. `paint_retained_graphite_panes` takes it.
    pending: BorderRefresh,
    /// Slots whose strips were written back. Full frames record 0.
    #[cfg(test)]
    pub(crate) last_restored_slots: usize,
}

impl BorderUnderlay {
    /// Save the surface after content paint and before border paint. Buffers
    /// reuse their capacity across frames; disabled animation allocates nothing.
    pub(crate) fn capture(&mut self, buffer: &[u32], stride: usize, slot: PixelRect) {
        self.reset(stride, buffer.len());
        self.store_slot(buffer, stride, slot, 0);
    }

    /// Save one Graphite slot into this frame. `reset` starts a new frame;
    /// later slots append so a gap that two rings share is stored once per
    /// pane before either ring is stroked. A stride or length change always
    /// starts over, so a resized buffer cannot replay stale strips.
    pub(crate) fn capture_graphite(
        &mut self,
        buffer: &[u32],
        stride: usize,
        slot: PixelRect,
        reset: bool,
    ) {
        if reset || stride != self.stride || buffer.len() != self.len {
            self.reset(stride, buffer.len());
        } else {
            self.slots.retain(|captured| captured.slot != slot);
        }
        self.store_slot(buffer, stride, slot, GRAPHITE_RING_OUTSET);
    }

    pub(crate) fn has_slot(&self, slot: PixelRect) -> bool {
        self.slots.iter().any(|captured| captured.slot == slot)
    }

    /// Rings the last restore asked the painter to stroke. Missing a take
    /// leaves `All`, which is the flag-off stroke.
    pub(crate) fn take_refresh(&mut self) -> BorderRefresh {
        std::mem::replace(&mut self.pending, BorderRefresh::All)
    }

    fn reset(&mut self, stride: usize, len: usize) {
        self.slots.clear();
        self.stride = stride;
        self.len = len;
    }

    fn store_slot(&mut self, buffer: &[u32], stride: usize, slot: PixelRect, outset: usize) {
        if stride == 0 {
            return;
        }
        let mut captured = CapturedSlot {
            slot,
            rects: Vec::new(),
            pixels: Vec::new(),
        };
        for rect in border_strips(slot) {
            store_rect(buffer, stride, rect, &mut captured);
        }
        if outset > 0 {
            for rect in outer_frame(slot, outset) {
                store_rect(buffer, stride, rect, &mut captured);
            }
        }
        self.slots.push(captured);
    }

    fn note_restored(&mut self, count: usize) {
        #[cfg(test)]
        {
            self.last_restored_slots = count;
        }
        #[cfg(not(test))]
        {
            let _ = count;
        }
    }

    fn write_slot(
        buffer: &mut [u32],
        stride: usize,
        slot: &CapturedSlot,
        damage: &mut FrameDamage,
    ) {
        let mut offset = 0;
        for rect in &slot.rects {
            for y in rect.y..rect.y + rect.height {
                let start = y * stride + rect.x;
                buffer[start..start + rect.width]
                    .copy_from_slice(&slot.pixels[offset..offset + rect.width]);
                offset += rect.width;
            }
            damage.push_rect(*rect);
        }
    }

    /// Erase every previous border before content updates, and publish the
    /// erased strips even when the sweep has already ended. Full paints
    /// replace the entire surface, so stale layout/theme/size pixels must be
    /// discarded.
    pub(crate) fn restore(&mut self, buffer: &mut [u32], stride: usize, damage: &mut FrameDamage) {
        let writable = !matches!(damage, FrameDamage::Full)
            && stride == self.stride
            && buffer.len() == self.len;
        if writable {
            let slots = std::mem::take(&mut self.slots);
            self.note_restored(slots.len());
            for slot in &slots {
                Self::write_slot(buffer, stride, slot, damage);
            }
        } else {
            self.slots.clear();
            self.note_restored(0);
        }
        self.pending = BorderRefresh::All;
    }

    /// Restore only rings that intersect `damage` (focus, pulse, sweep, or
    /// damage under the ring). A ring that shares pixels with one of those
    /// is restored too, so erasing a shared gap does not drop the neighbor.
    /// Unchanged rings stay on the surface and out of the damage.
    pub(crate) fn restore_changed(
        &mut self,
        buffer: &mut [u32],
        stride: usize,
        damage: &mut FrameDamage,
    ) {
        let writable = !matches!(damage, FrameDamage::Full)
            && stride == self.stride
            && buffer.len() == self.len;
        if !writable {
            self.slots.clear();
            self.note_restored(0);
            self.pending = BorderRefresh::All;
            return;
        }
        let mut refresh = vec![false; self.slots.len()];
        for (index, slot) in self.slots.iter().enumerate() {
            if slot
                .rects
                .iter()
                .any(|rect| frame_damage_intersects(damage, *rect))
            {
                refresh[index] = true;
            }
        }
        let mut pending: Vec<usize> = refresh
            .iter()
            .enumerate()
            .filter_map(|(index, on)| on.then_some(index))
            .collect();
        while let Some(index) = pending.pop() {
            for (other, slot) in self.slots.iter().enumerate() {
                if refresh[other] || !slots_share_pixels(&self.slots[index], slot) {
                    continue;
                }
                refresh[other] = true;
                pending.push(other);
            }
        }
        let mut kept = Vec::new();
        let mut restored = Vec::new();
        for (slot, refresh) in self.slots.drain(..).zip(refresh) {
            if refresh {
                Self::write_slot(buffer, stride, &slot, damage);
                restored.push(slot.slot);
            } else {
                kept.push(slot);
            }
        }
        self.note_restored(restored.len());
        self.slots = kept;
        self.pending = BorderRefresh::Slots(restored);
    }
}

fn store_rect(buffer: &[u32], stride: usize, rect: PixelRect, captured: &mut CapturedSlot) {
    let Some(rect) = rect.clipped(stride, buffer.len() / stride) else {
        return;
    };
    captured.rects.push(rect);
    for y in rect.y..rect.y + rect.height {
        let start = y * stride + rect.x;
        captured
            .pixels
            .extend_from_slice(&buffer[start..start + rect.width]);
    }
}

fn rects_overlap(left: PixelRect, right: PixelRect) -> bool {
    left.x < right.x.saturating_add(right.width)
        && right.x < left.x.saturating_add(left.width)
        && left.y < right.y.saturating_add(right.height)
        && right.y < left.y.saturating_add(left.height)
}

fn slots_share_pixels(left: &CapturedSlot, right: &CapturedSlot) -> bool {
    left.rects
        .iter()
        .any(|rect| right.rects.iter().any(|other| rects_overlap(*rect, *other)))
}

/// The ring's outside band: above and below the slot, including the corners,
/// then the left and right edges between those bands.
fn outer_frame(slot: PixelRect, outset: usize) -> [PixelRect; 4] {
    let x = slot.x.saturating_sub(outset);
    let y = slot.y.saturating_sub(outset);
    let right = slot.x.saturating_add(slot.width);
    let bottom = slot.y.saturating_add(slot.height);
    let wide = right.saturating_add(outset).saturating_sub(x);
    [
        PixelRect::new(x, y, wide, slot.y.saturating_sub(y)),
        PixelRect::new(x, bottom, wide, outset),
        PixelRect::new(x, slot.y, slot.x.saturating_sub(x), slot.height),
        PixelRect::new(right, slot.y, outset, slot.height),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::rasterize_pane_chrome_with_theme;

    #[test]
    fn retained_border_matches_full_paint_through_sweep_and_cleanup() {
        for (width, height) in [(1, 1), (9, 13), (101, 63), (1025, 769)] {
            let stride = width + 12;
            let slot = PixelRect::new(5, 4, width, height);
            // Nonuniform translucent content catches stale heads and wrong alpha.
            let mut surface: Vec<u32> = (0..stride * (height + 10))
                .map(|i| 0x80000000 | ((i as u32).wrapping_mul(7919) & 0xffffff))
                .collect();
            let mut retained = surface.clone();
            let mut underlay = BorderUnderlay::default();
            for head in [true, false] {
                for step in 0..=20 {
                    let mut damage = FrameDamage::rects();
                    underlay.restore(&mut retained, stride, &mut damage);
                    // Content can change while a head covers the same pixel.
                    let index = slot.y * stride + slot.x;
                    surface[index] ^= 0x000055aa;
                    retained[index] = surface[index];
                    let progress = (step < 20).then_some(step as f32 / 20.0);
                    if progress.is_some() {
                        underlay.capture(&retained, stride, slot);
                    }
                    let paint = |buffer: &mut [u32]| {
                        rasterize_pane_chrome_with_theme(
                            crate::theme::default_theme(),
                            buffer,
                            stride,
                            slot.x,
                            slot.y,
                            slot.width,
                            slot.height,
                            true,
                            false,
                            None,
                            progress,
                            head,
                            [100, 200, 150],
                            127,
                        );
                    };
                    paint(&mut retained);
                    let mut oracle = surface.clone();
                    paint(&mut oracle);
                    assert_eq!(
                        retained, oracle,
                        "{width}x{height}, step={step}, head={head}"
                    );
                }
                // The next run starts from a new underlying frame.
                retained.clone_from(&surface);
            }
            let mut damage = FrameDamage::rects();
            underlay.restore(&mut retained, stride, &mut damage);
            assert_eq!(damage, FrameDamage::rects(), "cleanup happens once");
        }
    }

    #[test]
    fn settled_graphite_edge_matches_one_stroke_after_restore() {
        use crate::graphite::{self, PaneHeader, PaneStatus};
        use crate::mux::ChromeGeom;
        let chrome = ChromeGeom {
            graphite: true,
            scale_milli: 1000,
        };
        let tok = graphite::DARK;
        let stride = 96;
        let slot = PixelRect::new(10, 8, 70, 48);
        let surface = vec![0xff1c1e20; stride * 64];
        let paint = |buffer: &mut [u32]| {
            graphite::paint_pane_chrome(
                buffer,
                stride,
                chrome,
                &tok,
                [90, 140, 180],
                graphite::Rect::new(slot.x, slot.y, slot.width, slot.height),
                [28, 30, 32],
                &PaneHeader {
                    name: "shell",
                    meta: None,
                    dot: graphite::Dot::Idle,
                    status: PaneStatus::Quiet,
                    focused: true,
                    handle_hover: false,
                },
                false,
                None,
                false,
            );
        };
        let mut underlay = BorderUnderlay::default();
        underlay.capture(&surface, stride, slot);
        let mut retained = surface.clone();
        paint(&mut retained);
        let once = retained.clone();
        let mut doubled = retained.clone();
        paint(&mut doubled);
        assert_ne!(
            doubled, once,
            "a second hairline blend must not be treated as idempotent"
        );
        let mut damage = FrameDamage::rects();
        underlay.restore(&mut retained, stride, &mut damage);
        paint(&mut retained);
        assert_eq!(retained, once);
    }

    #[test]
    fn graphite_focus_ring_matches_one_stroke_after_restore() {
        use crate::graphite::{self, PaneHeader, PaneStatus};
        use crate::mux::ChromeGeom;
        let chrome = ChromeGeom {
            graphite: true,
            scale_milli: 1000,
        };
        let tok = graphite::DARK;
        let stride = 180;
        let height = 72;
        let left = PixelRect::new(1, 1, 70, 48);
        let right = PixelRect::new(79, 1, 70, 48);
        let surface = vec![0xff1c1e20; stride * height];
        for cycle in [None, Some(0.25)] {
            let paint = |buffer: &mut [u32], slot: PixelRect, focused: bool| {
                graphite::paint_pane_chrome(
                    buffer,
                    stride,
                    chrome,
                    &tok,
                    [90, 140, 180],
                    graphite::Rect::new(slot.x, slot.y, slot.width, slot.height),
                    [28, 30, 32],
                    &PaneHeader {
                        name: "shell",
                        meta: None,
                        dot: graphite::Dot::Idle,
                        status: if focused {
                            PaneStatus::Focused
                        } else {
                            PaneStatus::Quiet
                        },
                        focused,
                        handle_hover: false,
                    },
                    true,
                    cycle.filter(|_| focused),
                    true,
                );
            };
            let mut underlay = BorderUnderlay::default();
            underlay.capture_graphite(&surface, stride, left, true);
            underlay.capture_graphite(&surface, stride, right, false);
            assert!(
                underlay
                    .slots
                    .iter()
                    .flat_map(|slot| slot.rects.iter())
                    .any(|rect| rect.x < left.x || rect.y < left.y),
                "graphite capture must keep the ring outside the slot"
            );
            let mut retained = surface.clone();
            paint(&mut retained, left, true);
            paint(&mut retained, right, false);
            let once = retained.clone();
            let mut doubled = retained.clone();
            paint(&mut doubled, left, true);
            paint(&mut doubled, right, false);
            assert_ne!(
                doubled, once,
                "a second ring blend must not be treated as idempotent, cycle={cycle:?}"
            );
            let mut damage = FrameDamage::rects();
            underlay.restore(&mut retained, stride, &mut damage);
            paint(&mut retained, left, true);
            paint(&mut retained, right, false);
            assert_eq!(
                retained, once,
                "restore then one stroke must match the first ring, cycle={cycle:?}"
            );
        }
    }

    #[test]
    fn capture_replaces_the_previous_slot() {
        let stride = 30;
        let mut buffer = vec![0x11u32; stride * 20];
        let mut underlay = BorderUnderlay::default();
        underlay.capture(&buffer, stride, PixelRect::new(0, 0, 12, 12));
        let first: usize = underlay.slots.iter().map(|slot| slot.pixels.len()).sum();
        assert!(first > 0);
        buffer.fill(0x22);
        underlay.capture(&buffer, stride, PixelRect::new(0, 0, 12, 12));
        let pixels: Vec<u32> = underlay
            .slots
            .iter()
            .flat_map(|slot| slot.pixels.iter().copied())
            .collect();
        assert_eq!(pixels.len(), first);
        assert!(pixels.iter().all(|pixel| *pixel == 0x22));
    }

    #[test]
    fn full_repaint_discards_old_surface_and_disabled_path_allocates_nothing() {
        let mut cache = BorderUnderlay::default();
        let mut buffer = vec![0x12345678; 100 * 80];
        cache.restore(&mut buffer, 100, &mut FrameDamage::rects());
        assert_eq!(cache.slots.capacity(), 0);
        cache.capture(&buffer, 100, PixelRect::new(0, 0, 100, 80));
        buffer.fill(0xabcdef01);
        cache.restore(&mut buffer, 100, &mut FrameDamage::Full);
        assert!(buffer.iter().all(|pixel| *pixel == 0xabcdef01));
        cache.restore(&mut buffer, 100, &mut FrameDamage::rects());
        assert!(buffer.iter().all(|pixel| *pixel == 0xabcdef01));
    }

    fn damaged_tile_count(width: usize, height: usize, damage: &FrameDamage) -> usize {
        let grid = crate::present_tiles::tiles(width, height);
        crate::present_tiles::damaged_tiles(&grid, damage).len()
    }

    fn percentile_50(samples: &[usize]) -> usize {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// Present-cost scenario (c) frame: 3642×1716, eight even columns, four of
    /// them writing an inset band. Restoring every ring adds 68 tiles at the
    /// median. Restoring only rings that intersect the damage adds none.
    fn scenario_column_slots() -> Vec<PixelRect> {
        const WIDTH: usize = 3642;
        const HEIGHT: usize = 1716;
        const LEFT: usize = 8;
        const TOP: usize = 132;
        const BOTTOM: usize = 56;
        const GAP: usize = 8;
        const COLS: usize = 8;
        let slot_w = (WIDTH - LEFT * 2 - (COLS - 1) * GAP) / COLS;
        let slot_h = HEIGHT - TOP - BOTTOM;
        (0..COLS)
            .map(|index| PixelRect::new(LEFT + index * (slot_w + GAP), TOP, slot_w, slot_h))
            .collect()
    }

    fn scenario_chatter(slots: &[PixelRect]) -> FrameDamage {
        let mut damage = FrameDamage::rects();
        for slot in slots.iter().step_by(2) {
            damage.push_rect(PixelRect::new(
                slot.x + 12,
                slot.y + 28 + 12,
                slot.width - 24,
                20 * 18,
            ));
        }
        damage
    }

    #[test]
    fn scenario_underlay_tiles_move_from_68_toward_0() {
        const WIDTH: usize = 3642;
        const HEIGHT: usize = 1716;
        const FRAMES: usize = 31;
        let slots = scenario_column_slots();
        let buffer = vec![0xff1c1e20u32; WIDTH * HEIGHT];
        let chatter = scenario_chatter(&slots);
        let chatter_tiles = damaged_tile_count(WIDTH, HEIGHT, &chatter);
        let mut scratch = buffer.clone();
        let mut baseline = Vec::with_capacity(FRAMES);
        let mut selective = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let mut all = BorderUnderlay::default();
            for (index, slot) in slots.iter().enumerate() {
                all.capture_graphite(&buffer, WIDTH, *slot, index == 0);
            }
            scratch.copy_from_slice(&buffer);
            let mut damage = chatter.clone();
            let before = damaged_tile_count(WIDTH, HEIGHT, &damage);
            all.restore(&mut scratch, WIDTH, &mut damage);
            baseline.push(damaged_tile_count(WIDTH, HEIGHT, &damage) - before);

            let mut some = BorderUnderlay::default();
            for (index, slot) in slots.iter().enumerate() {
                some.capture_graphite(&buffer, WIDTH, *slot, index == 0);
            }
            scratch.copy_from_slice(&buffer);
            let mut damage = chatter.clone();
            let before = damaged_tile_count(WIDTH, HEIGHT, &damage);
            some.restore_changed(&mut scratch, WIDTH, &mut damage);
            assert_eq!(some.last_restored_slots, 0);
            selective.push(damaged_tile_count(WIDTH, HEIGHT, &damage) - before);
        }
        let baseline_p50 = percentile_50(&baseline);
        let selective_p50 = percentile_50(&selective);
        eprintln!(
            "damage.added_tiles.underlay frames={FRAMES} compose_tiles={chatter_tiles} baseline_p50={baseline_p50} selective_p50={selective_p50}"
        );
        assert_eq!(baseline_p50, 68, "full-ring restore is the measured 68");
        assert_eq!(
            selective_p50, 0,
            "unchanged rings must not add underlay tiles"
        );

        let mut focus = chatter.clone();
        focus.push_rect(slots[0]);
        focus.push_rect(slots[1]);
        let mut moved = BorderUnderlay::default();
        for (index, slot) in slots.iter().enumerate() {
            moved.capture_graphite(&buffer, WIDTH, *slot, index == 0);
        }
        let before = damaged_tile_count(WIDTH, HEIGHT, &focus);
        scratch.copy_from_slice(&buffer);
        moved.restore_changed(&mut scratch, WIDTH, &mut focus);
        let added = damaged_tile_count(WIDTH, HEIGHT, &focus) - before;
        eprintln!(
            "damage.added_tiles.underlay focus_slots={} added={added}",
            moved.last_restored_slots
        );
        assert!(
            moved.last_restored_slots >= 2,
            "focus damage restores the old and new rings"
        );
        assert!(
            added < baseline_p50,
            "focus slots already cover the ring tiles, so underlay adds {added}"
        );
    }

    #[test]
    fn unchanged_interior_matches_full_restroke_and_ring_damage_is_not_missed() {
        use crate::frame_damage::frame_damage_intersects;
        use crate::graphite::{self, PaneHeader, PaneStatus};
        use crate::mux::ChromeGeom;
        let chrome = ChromeGeom {
            graphite: true,
            scale_milli: 1000,
        };
        let tok = graphite::DARK;
        let stride = 180;
        let height = 80;
        let left = PixelRect::new(8, 8, 70, 52);
        let right = PixelRect::new(86, 8, 70, 52);
        let surface = vec![0xff1c1e20u32; stride * height];
        let paint = |buffer: &mut [u32], slot: PixelRect, focused: bool| {
            graphite::paint_pane_chrome(
                buffer,
                stride,
                chrome,
                &tok,
                [90, 140, 180],
                graphite::Rect::new(slot.x, slot.y, slot.width, slot.height),
                [28, 30, 32],
                &PaneHeader {
                    name: "shell",
                    meta: None,
                    dot: graphite::Dot::Idle,
                    status: if focused {
                        PaneStatus::Focused
                    } else {
                        PaneStatus::Quiet
                    },
                    focused,
                    handle_hover: false,
                },
                true,
                None,
                false,
            );
        };
        let capture_both = |underlay: &mut BorderUnderlay, buffer: &[u32]| {
            underlay.capture_graphite(buffer, stride, left, true);
            underlay.capture_graphite(buffer, stride, right, false);
        };
        let mut retained = surface.clone();
        let mut underlay = BorderUnderlay::default();
        capture_both(&mut underlay, &retained);
        paint(&mut retained, left, true);
        paint(&mut retained, right, false);
        let stroked = retained.clone();

        // Below the 28px title row and inside the 7px edge strips.
        let interior = PixelRect::new(left.x + 20, left.y + 36, 6, 4);
        let mut selective = retained.clone();
        let mut oracle = retained.clone();
        let index = (interior.y + 1) * stride + interior.x + 1;
        selective[index] ^= 0x0000_44aa;
        oracle[index] ^= 0x0000_44aa;
        let mut sel_under = BorderUnderlay::default();
        let mut ora_under = BorderUnderlay::default();
        capture_both(&mut sel_under, &surface);
        capture_both(&mut ora_under, &surface);
        let mut sel_damage = FrameDamage::rects();
        sel_damage.push_rect(interior);
        sel_under.restore_changed(&mut selective, stride, &mut sel_damage);
        assert_eq!(sel_under.last_restored_slots, 0);
        let mut ora_damage = FrameDamage::rects();
        ora_damage.push_rect(interior);
        ora_under.restore(&mut oracle, stride, &mut ora_damage);
        capture_both(&mut ora_under, &oracle);
        paint(&mut oracle, left, true);
        paint(&mut oracle, right, false);
        assert_eq!(
            selective, oracle,
            "an interior cell must not restroke rings"
        );
        assert_eq!(sel_under.take_refresh(), BorderRefresh::Slots(Vec::new()));

        let top = border_strips(left)[0];
        let mut selective = stroked.clone();
        let mut oracle = stroked.clone();
        let mut sel_under = BorderUnderlay::default();
        let mut ora_under = BorderUnderlay::default();
        capture_both(&mut sel_under, &surface);
        capture_both(&mut ora_under, &surface);
        let mut sel_damage = FrameDamage::rects();
        sel_damage.push_rect(top);
        sel_under.restore_changed(&mut selective, stride, &mut sel_damage);
        assert_eq!(sel_under.last_restored_slots, 1);
        let mut ora_damage = FrameDamage::rects();
        ora_damage.push_rect(top);
        ora_under.restore(&mut oracle, stride, &mut ora_damage);
        let content = top.y * stride + top.x + 4;
        selective[content] = 0xff20_c060;
        oracle[content] = 0xff20_c060;
        sel_under.capture_graphite(&selective, stride, left, false);
        paint(&mut selective, left, true);
        capture_both(&mut ora_under, &oracle);
        paint(&mut oracle, left, true);
        paint(&mut oracle, right, false);
        assert_eq!(
            selective, oracle,
            "damage under one ring restrokes that ring only"
        );
        for (index, (before, after)) in stroked.iter().zip(&selective).enumerate() {
            if before == after {
                continue;
            }
            let pixel = PixelRect::new(index % stride, index / stride, 1, 1);
            assert!(
                frame_damage_intersects(&sel_damage, pixel),
                "missed damage at {},{}",
                pixel.x,
                pixel.y
            );
        }
    }
}
