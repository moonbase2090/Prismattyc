// SPDX-License-Identifier: MPL-2.0
//! Retain only pixels covered by a transient border, including its head.
use crate::frame_damage::{border_strips, FrameDamage, PixelRect};

/// Pixels outside a Graphite slot covered by the focus ring and sweep head.
/// The settled ring starts one pixel outside the slot and shades one pixel
/// past that; the head stamp reaches about two pixels past its sample.
const GRAPHITE_RING_OUTSET: usize = 3;

#[derive(Default)]
pub(crate) struct BorderUnderlay {
    rects: Vec<PixelRect>,
    pixels: Vec<u32>,
    stride: usize,
    len: usize,
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
        }
        self.store_slot(buffer, stride, slot, GRAPHITE_RING_OUTSET);
    }

    fn reset(&mut self, stride: usize, len: usize) {
        self.rects.clear();
        self.pixels.clear();
        self.stride = stride;
        self.len = len;
    }

    fn store_slot(&mut self, buffer: &[u32], stride: usize, slot: PixelRect, outset: usize) {
        if stride == 0 {
            return;
        }
        for rect in border_strips(slot) {
            self.store_rect(buffer, stride, rect);
        }
        if outset > 0 {
            for rect in outer_frame(slot, outset) {
                self.store_rect(buffer, stride, rect);
            }
        }
    }

    fn store_rect(&mut self, buffer: &[u32], stride: usize, rect: PixelRect) {
        let Some(rect) = rect.clipped(stride, buffer.len() / stride) else {
            return;
        };
        self.rects.push(rect);
        for y in rect.y..rect.y + rect.height {
            let start = y * stride + rect.x;
            self.pixels
                .extend_from_slice(&buffer[start..start + rect.width]);
        }
    }

    /// Erase the previous border before content updates, and publish the erased
    /// strips even when the sweep has already ended. Full paints replace the
    /// entire surface, so stale layout/theme/size pixels must be discarded.
    pub(crate) fn restore(&mut self, buffer: &mut [u32], stride: usize, damage: &mut FrameDamage) {
        if !matches!(damage, FrameDamage::Full) && stride == self.stride && buffer.len() == self.len
        {
            let mut offset = 0;
            for rect in &self.rects {
                for y in rect.y..rect.y + rect.height {
                    let start = y * stride + rect.x;
                    buffer[start..start + rect.width]
                        .copy_from_slice(&self.pixels[offset..offset + rect.width]);
                    offset += rect.width;
                }
                damage.push_rect(*rect);
            }
        }
        self.rects.clear();
        self.pixels.clear();
    }
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
        let tok = graphite::bar_tokens(
            crate::theme::ThemeVariant::Dark,
            crate::config::BarColor::Graphite,
        );
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
        let tok = graphite::bar_tokens(
            crate::theme::ThemeVariant::Dark,
            crate::config::BarColor::Graphite,
        );
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
                    .rects
                    .iter()
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
        let first = underlay.pixels.len();
        assert!(first > 0);
        buffer.fill(0x22);
        underlay.capture(&buffer, stride, PixelRect::new(0, 0, 12, 12));
        assert_eq!(underlay.pixels.len(), first);
        assert!(underlay.pixels.iter().all(|pixel| *pixel == 0x22));
    }

    #[test]
    fn full_repaint_discards_old_surface_and_disabled_path_allocates_nothing() {
        let mut cache = BorderUnderlay::default();
        let mut buffer = vec![0x12345678; 100 * 80];
        cache.restore(&mut buffer, 100, &mut FrameDamage::rects());
        assert_eq!(cache.pixels.capacity(), 0);
        cache.capture(&buffer, 100, PixelRect::new(0, 0, 100, 80));
        buffer.fill(0xabcdef01);
        cache.restore(&mut buffer, 100, &mut FrameDamage::Full);
        assert!(buffer.iter().all(|pixel| *pixel == 0xabcdef01));
        cache.restore(&mut buffer, 100, &mut FrameDamage::rects());
        assert!(buffer.iter().all(|pixel| *pixel == 0xabcdef01));
    }
}
