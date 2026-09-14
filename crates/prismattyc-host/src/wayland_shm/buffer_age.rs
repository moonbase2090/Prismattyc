//! Straight-alpha canonical framebuffer and bounded shm buffer-age repair.

use std::collections::VecDeque;

use crate::frame_damage::{FrameDamage, PixelRect};

const DAMAGE_HISTORY_LIMIT: usize = 32;

pub(super) struct BufferAge {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    generation: u64,
    history: VecDeque<(u64, FrameDamage)>,
}

impl BufferAge {
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width.saturating_mul(height)],
            generation: 0,
            history: VecDeque::new(),
        }
    }

    /// Resize the canonical framebuffer. Callers must invalidate every slot
    /// generation when this returns true.
    pub(super) fn resize(&mut self, width: usize, height: usize) -> bool {
        if self.width == width && self.height == height {
            return false;
        }
        self.width = width;
        self.height = height;
        self.pixels.resize(width.saturating_mul(height), 0);
        self.history.clear();
        true
    }

    pub(super) fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    /// Repair one rotating presentation slot from the canonical framebuffer.
    /// Returns the generation now stored in `target`.
    pub(super) fn repair_slot(
        &mut self,
        slot_generation: Option<u64>,
        damage: FrameDamage,
        target: &mut [u32],
    ) -> u64 {
        let next = match self.generation.checked_add(1) {
            Some(next) => next,
            None => {
                self.history.clear();
                1
            }
        };

        let repair = accumulated_damage(
            &self.history,
            self.generation,
            slot_generation,
            next,
            &damage,
        );
        copy_premultiplied(&self.pixels, target, self.width, self.height, &repair);

        self.generation = next;
        self.history.push_back((next, damage));
        while self.history.len() > DAMAGE_HISTORY_LIMIT {
            self.history.pop_front();
        }
        next
    }
}

/// Accumulate every frame that a rotating slot missed.
///
/// This decision is pure. Any unknown age, evicted or non-contiguous
/// generation, crossed full marker, or invalid generation order requires a
/// full repair.
fn accumulated_damage(
    history: &VecDeque<(u64, FrameDamage)>,
    current_generation: u64,
    slot_generation: Option<u64>,
    next: u64,
    current: &FrameDamage,
) -> FrameDamage {
    let Some(slot_generation) = slot_generation else {
        return FrameDamage::Full;
    };
    if slot_generation > current_generation || next <= slot_generation {
        return FrameDamage::Full;
    }

    let first_needed = slot_generation.saturating_add(1);
    let first_available = history.front().map_or(next, |(generation, _)| *generation);
    if first_needed < first_available {
        return FrameDamage::Full;
    }

    let mut expected = first_needed;
    let mut rects = Vec::new();
    for (generation, damage) in history
        .iter()
        .filter(|(generation, _)| *generation >= first_needed)
    {
        if *generation != expected {
            return FrameDamage::Full;
        }
        if !append_damage(&mut rects, damage) {
            return FrameDamage::Full;
        }
        expected = expected.saturating_add(1);
    }
    if expected != next || !append_damage(&mut rects, current) {
        return FrameDamage::Full;
    }
    FrameDamage::Rects(rects)
}

fn append_damage(rects: &mut Vec<PixelRect>, damage: &FrameDamage) -> bool {
    match damage {
        FrameDamage::Full => false,
        FrameDamage::Rects(current) => {
            rects.extend(current.iter().copied());
            true
        }
    }
}

fn copy_premultiplied(
    source: &[u32],
    target: &mut [u32],
    width: usize,
    height: usize,
    damage: &FrameDamage,
) {
    debug_assert_eq!(source.len(), width.saturating_mul(height));
    debug_assert_eq!(target.len(), source.len());
    match damage {
        FrameDamage::Full => {
            for (target, source) in target.iter_mut().zip(source.iter().copied()) {
                *target = premultiply(source);
            }
        }
        FrameDamage::Rects(rects) => {
            for rect in rects {
                let Some(rect) = rect.clipped(width, height) else {
                    continue;
                };
                for y in rect.y..rect.y + rect.height {
                    let start = y * width + rect.x;
                    let end = start + rect.width;
                    for (target, source) in target[start..end]
                        .iter_mut()
                        .zip(source[start..end].iter().copied())
                    {
                        *target = premultiply(source);
                    }
                }
            }
        }
    }
}

fn premultiply(pixel: u32) -> u32 {
    let alpha = pixel >> 24;
    if alpha == 255 {
        return pixel;
    }
    let red = ((pixel >> 16) & 0xff) * alpha / 255;
    let green = ((pixel >> 8) & 0xff) * alpha / 255;
    let blue = (pixel & 0xff) * alpha / 255;
    u32::from_be_bytes([alpha as u8, red as u8, green as u8, blue as u8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_damage::{
        compose_frame_damage, ChromeSnapshot, LayoutSnapshot, PaneDamageSnapshot,
        PaneLayoutSnapshot,
    };

    fn rect(x: usize, y: usize, width: usize, height: usize) -> FrameDamage {
        FrameDamage::Rects(vec![PixelRect::new(x, y, width, height)])
    }

    #[test]
    fn unknown_slot_gets_full_frame_once() {
        let mut age = BufferAge::new(2, 2);
        age.pixels_mut()
            .copy_from_slice(&[0x80ff_8040, 0xff01_0203, 0x4004_080c, 0x000f_0f0f]);
        let mut target = vec![0xdead_beef; 4];
        let generation = age.repair_slot(None, rect(0, 0, 1, 1), &mut target);
        assert_eq!(generation, 1);
        assert_eq!(target, vec![0x8080_4020, 0xff01_0203, 0x4001_0203, 0]);
        assert_eq!(
            age.pixels,
            vec![0x80ff_8040, 0xff01_0203, 0x4004_080c, 0x000f_0f0f]
        );
    }

    #[test]
    fn aged_slot_accumulates_every_missed_rectangle() {
        let mut age = BufferAge::new(3, 1);
        age.pixels_mut()
            .copy_from_slice(&[0xff00_0001, 0xff00_0002, 0xff00_0003]);
        let mut first = vec![0; 3];
        let first_generation = age.repair_slot(None, FrameDamage::Full, &mut first);

        age.pixels_mut()[0] = 0xff00_0011;
        let mut other = vec![0; 3];
        age.repair_slot(None, rect(0, 0, 1, 1), &mut other);
        age.pixels_mut()[2] = 0xff00_0033;
        age.repair_slot(None, rect(2, 0, 1, 1), &mut other);

        let mut reused = first;
        age.repair_slot(
            Some(first_generation),
            FrameDamage::Rects(Vec::new()),
            &mut reused,
        );
        assert_eq!(reused, vec![0xff00_0011, 0xff00_0002, 0xff00_0033]);
    }

    #[test]
    fn composed_rows_repair_a_slot_after_age_two_and_three_misses() {
        let layout = LayoutSnapshot {
            panes: vec![PaneLayoutSnapshot {
                id: 7,
                slot: PixelRect::new(0, 0, 3, 3),
                content: PixelRect::new(0, 0, 3, 3),
            }],
        };
        let compose_row = |row| {
            compose_frame_damage(
                Some(&layout),
                &layout,
                Some(&ChromeSnapshot::default()),
                &ChromeSnapshot::default(),
                &[PaneDamageSnapshot {
                    pane_id: 7,
                    content: PixelRect::new(0, 0, 3, 3),
                    row_height: 1,
                    dirty_rows: vec![row],
                    blit: None,
                }],
                false,
            )
        };

        let mut age = BufferAge::new(3, 3);
        age.pixels_mut().copy_from_slice(&[
            0xff00_0001,
            0xff00_0002,
            0xff00_0003,
            0xff00_0004,
            0xff00_0005,
            0xff00_0006,
            0xff00_0007,
            0xff00_0008,
            0xff00_0009,
        ]);
        let mut reused = vec![0; 9];
        let first_generation = age.repair_slot(None, FrameDamage::Full, &mut reused);

        age.pixels_mut()[0] = 0xff00_0011;
        let mut scratch = vec![0; 9];
        age.repair_slot(None, compose_row(0), &mut scratch);
        age.pixels_mut()[6] = 0xff00_0033;
        age.repair_slot(None, compose_row(2), &mut scratch);

        age.repair_slot(
            Some(first_generation),
            FrameDamage::Rects(Vec::new()),
            &mut reused,
        );
        assert_eq!(
            reused,
            vec![
                0xff00_0011,
                0xff00_0002,
                0xff00_0003,
                0xff00_0004,
                0xff00_0005,
                0xff00_0006,
                0xff00_0033,
                0xff00_0008,
                0xff00_0009,
            ]
        );
    }

    #[test]
    fn accumulated_damage_rotation_table_rejects_generation_gaps() {
        let a = PixelRect::new(0, 0, 1, 1);
        let b = PixelRect::new(1, 0, 1, 1);
        let c = PixelRect::new(2, 0, 1, 1);
        struct Case {
            name: &'static str,
            history: VecDeque<(u64, FrameDamage)>,
            current_generation: u64,
            slot_generation: Option<u64>,
            next: u64,
            current: FrameDamage,
            expected: FrameDamage,
        }
        let cases = [
            Case {
                name: "slot missed two contiguous rotations",
                history: VecDeque::from([
                    (2, FrameDamage::Rects(vec![a])),
                    (3, FrameDamage::Rects(vec![b])),
                ]),
                current_generation: 3,
                slot_generation: Some(1),
                next: 4,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Rects(vec![a, b, c]),
            },
            Case {
                name: "current slot needs only this frame",
                history: VecDeque::from([
                    (2, FrameDamage::Rects(vec![a])),
                    (3, FrameDamage::Rects(vec![b])),
                ]),
                current_generation: 3,
                slot_generation: Some(3),
                next: 4,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Rects(vec![c]),
            },
            Case {
                name: "internal generation gap forces full repair",
                history: VecDeque::from([
                    (2, FrameDamage::Rects(vec![a])),
                    (4, FrameDamage::Rects(vec![b])),
                ]),
                current_generation: 4,
                slot_generation: Some(1),
                next: 5,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Full,
            },
            Case {
                name: "evicted first generation forces full repair",
                history: VecDeque::from([
                    (3, FrameDamage::Rects(vec![b])),
                    (4, FrameDamage::Rects(vec![c])),
                ]),
                current_generation: 4,
                slot_generation: Some(1),
                next: 5,
                current: FrameDamage::Rects(vec![a]),
                expected: FrameDamage::Full,
            },
            Case {
                name: "crossed full marker forces full repair",
                history: VecDeque::from([(2, FrameDamage::Full), (3, FrameDamage::Rects(vec![b]))]),
                current_generation: 3,
                slot_generation: Some(1),
                next: 4,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Full,
            },
            Case {
                name: "unknown slot age forces full repair",
                history: VecDeque::new(),
                current_generation: 3,
                slot_generation: None,
                next: 4,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Full,
            },
            Case {
                name: "future slot with nonadjacent next forces full repair",
                history: VecDeque::from([(5, FrameDamage::Rects(vec![a]))]),
                current_generation: 3,
                slot_generation: Some(4),
                next: 6,
                current: FrameDamage::Rects(vec![c]),
                expected: FrameDamage::Full,
            },
        ];

        for case in cases {
            assert_eq!(
                accumulated_damage(
                    &case.history,
                    case.current_generation,
                    case.slot_generation,
                    case.next,
                    &case.current,
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn full_marker_repairs_all_pixels_for_older_slot() {
        let mut age = BufferAge::new(2, 1);
        age.pixels_mut().copy_from_slice(&[1, 2]);
        let mut old = vec![0; 2];
        let old_generation = age.repair_slot(None, FrameDamage::Full, &mut old);
        age.pixels_mut()
            .copy_from_slice(&[0xff00_0010, 0xff00_0020]);
        let mut current = vec![0; 2];
        age.repair_slot(None, FrameDamage::Full, &mut current);
        age.pixels_mut()[0] = 0xff00_0030;
        age.repair_slot(Some(old_generation), rect(0, 0, 1, 1), &mut old);
        assert_eq!(old, vec![0xff00_0030, 0xff00_0020]);
    }

    #[test]
    fn history_gap_falls_back_to_full_repair() {
        let mut age = BufferAge::new(2, 1);
        age.pixels_mut()
            .copy_from_slice(&[0xff00_0001, 0xff00_0002]);
        let mut old = vec![0; 2];
        let old_generation = age.repair_slot(None, FrameDamage::Full, &mut old);
        let mut current = vec![0; 2];
        for value in 0..=DAMAGE_HISTORY_LIMIT {
            age.pixels_mut()[0] = 0xff00_0100 | value as u32;
            age.repair_slot(None, rect(0, 0, 1, 1), &mut current);
        }
        age.pixels_mut()[1] = 0xff00_0099;
        age.repair_slot(Some(old_generation), rect(1, 0, 1, 1), &mut old);
        assert_eq!(old, age.pixels);
    }

    #[test]
    fn translucent_pixels_are_not_premultiplied_twice_across_partial_frames() {
        let mut age = BufferAge::new(2, 1);
        age.pixels_mut()
            .copy_from_slice(&[0x80ff_8040, 0x4004_080c]);
        let mut reused = vec![0; 2];
        let reused_generation = age.repair_slot(None, FrameDamage::Full, &mut reused);
        assert_eq!(reused, vec![0x8080_4020, 0x4001_0203]);

        age.pixels_mut()[1] = 0x80c8_6432;
        let mut other = vec![0; 2];
        age.repair_slot(None, rect(1, 0, 1, 1), &mut other);
        age.repair_slot(
            Some(reused_generation),
            FrameDamage::Rects(Vec::new()),
            &mut reused,
        );

        assert_eq!(reused, vec![0x8080_4020, 0x8064_3219]);
        assert_eq!(age.pixels, vec![0x80ff_8040, 0x80c8_6432]);
    }

    #[test]
    fn resize_clears_history_and_requires_slot_invalidation() {
        let mut age = BufferAge::new(1, 1);
        assert!(!age.resize(1, 1));
        assert!(age.resize(2, 1));
        assert_eq!(age.pixels_mut().len(), 2);
        assert!(age.history.is_empty());
    }

    #[test]
    fn history_retains_exactly_the_last_32_generations() {
        let mut age = BufferAge::new(1, 1);
        let mut target = vec![0];
        for expected_generation in 1..=33 {
            assert_eq!(
                age.repair_slot(None, FrameDamage::Rects(Vec::new()), &mut target),
                expected_generation
            );
            if expected_generation == 31 {
                assert_eq!(age.history.len(), 31);
                assert_eq!(age.history.front().map(|entry| entry.0), Some(1));
                assert_eq!(age.history.back().map(|entry| entry.0), Some(31));
            }
            if expected_generation == 32 {
                assert_eq!(age.history.len(), 32);
                assert_eq!(age.history.front().map(|entry| entry.0), Some(1));
                assert_eq!(age.history.back().map(|entry| entry.0), Some(32));
            }
        }
        assert_eq!(age.history.len(), 32);
        assert_eq!(age.history.front().map(|entry| entry.0), Some(2));
        assert_eq!(age.history.back().map(|entry| entry.0), Some(33));
    }

    #[test]
    fn generation_wrap_discards_pre_wrap_history() {
        let mut age = BufferAge::new(2, 1);
        age.generation = u64::MAX;
        age.history.push_back((u64::MAX, rect(1, 0, 1, 1)));
        age.pixels_mut()
            .copy_from_slice(&[0xff00_0011, 0xff00_0022]);
        let mut target = vec![0xdead_beef; 2];

        assert_eq!(age.repair_slot(Some(0), rect(0, 0, 1, 1), &mut target), 1);
        assert_eq!(age.generation, 1);
        assert_eq!(age.history.len(), 1);
        assert_eq!(age.history.front().map(|entry| entry.0), Some(1));
        assert_eq!(target, vec![0xff00_0011, 0xdead_beef]);
    }

    #[test]
    fn partial_copy_uses_row_major_offset() {
        let source = [
            0xff00_0001,
            0xff00_0002,
            0xff00_0003,
            0xff00_0004,
            0xff00_0005,
            0xff00_0006,
        ];
        let mut target = [0xdead_beef; 6];
        copy_premultiplied(&source, &mut target, 3, 2, &rect(1, 1, 1, 1));
        assert_eq!(
            target,
            [
                0xdead_beef,
                0xdead_beef,
                0xdead_beef,
                0xdead_beef,
                0xff00_0005,
                0xdead_beef,
            ]
        );
    }

    #[test]
    fn premultiply_alpha_boundary_table() {
        let cases = [
            (0x00ff_ffff, 0x0000_0000),
            (0x01ff_ffff, 0x0101_0101),
            (0x7fff_ffff, 0x7f7f_7f7f),
            (0x8001_0305, 0x8000_0102),
            (0xfeff_ffff, 0xfefe_fefe),
            (0xffff_ffff, 0xffff_ffff),
        ];
        for (straight, expected) in cases {
            assert_eq!(premultiply(straight), expected, "{straight:#010x}");
        }
    }
}
