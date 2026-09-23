// SPDX-License-Identifier: MPL-2.0
//! Retain only pixels covered by a transient border, including its head.
use crate::frame_damage::{border_strips, FrameDamage, PixelRect};

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
        self.rects.clear();
        self.pixels.clear();
        self.stride = stride;
        self.len = buffer.len();
        if stride == 0 {
            return;
        }
        for rect in border_strips(slot) {
            let Some(rect) = rect.clipped(stride, buffer.len() / stride) else {
                continue;
            };
            self.rects.push(rect);
            for y in rect.y..rect.y + rect.height {
                let start = y * stride + rect.x;
                self.pixels
                    .extend_from_slice(&buffer[start..start + rect.width]);
            }
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
