//! Exact damage history for the three IOSurface presentation buffers.

use crate::frame_damage::{FrameDamage, PixelRect};

pub(crate) const SURFACE_COUNT: usize = 3;
const MAX_REPLAY_RECTS: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct SurfaceDamageHistory {
    bounds: PixelRect,
    stale: [Vec<PixelRect>; SURFACE_COUNT],
    current: Option<usize>,
}

impl SurfaceDamageHistory {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let bounds = PixelRect::new(0, 0, width, height);
        Self {
            bounds,
            stale: std::array::from_fn(|_| vec![bounds]),
            current: None,
        }
    }

    pub(crate) fn next_free(&self, mut is_busy: impl FnMut(usize) -> bool) -> Option<usize> {
        let start = self
            .current
            .map_or(0, |current| (current + 1) % SURFACE_COUNT);
        (0..SURFACE_COUNT)
            .map(|step| (start + step) % SURFACE_COUNT)
            .find(|&index| !is_busy(index))
    }

    pub(crate) fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// Rectangles needed to bring `index` up to the current framebuffer.
    pub(crate) fn plan(&self, index: usize, damage: &FrameDamage) -> Vec<PixelRect> {
        let mut current = clipped_damage(damage, self.bounds);
        if current.is_empty() && self.stale[index].is_empty() {
            return current;
        }

        let mut plan = self.stale[index].clone();
        plan.append(&mut current);
        if plan.len() > MAX_REPLAY_RECTS || plan.contains(&self.bounds) {
            vec![self.bounds]
        } else {
            plan
        }
    }

    /// Record a write only after the surface contents have been committed.
    pub(crate) fn committed(&mut self, index: usize, damage: &FrameDamage) {
        let current = clipped_damage(damage, self.bounds);
        self.stale[index].clear();
        for (other, stale) in self.stale.iter_mut().enumerate() {
            if other != index {
                append_bounded(stale, &current, self.bounds);
            }
        }
        self.current = Some(index);
    }
}

fn append_bounded(stale: &mut Vec<PixelRect>, damage: &[PixelRect], bounds: PixelRect) {
    if stale.contains(&bounds)
        || damage.contains(&bounds)
        || stale.len().saturating_add(damage.len()) > MAX_REPLAY_RECTS
    {
        stale.clear();
        stale.push(bounds);
    } else {
        stale.extend_from_slice(damage);
    }
}

fn clipped_damage(damage: &FrameDamage, bounds: PixelRect) -> Vec<PixelRect> {
    match damage {
        FrameDamage::Full => vec![bounds],
        FrameDamage::Rects(rects) => rects
            .iter()
            .filter_map(|rect| clip(*rect, bounds))
            .collect(),
    }
}

fn clip(rect: PixelRect, bounds: PixelRect) -> Option<PixelRect> {
    let right = rect.x.saturating_add(rect.width).min(bounds.width);
    let bottom = rect.y.saturating_add(rect.height).min(bounds.height);
    (rect.x < right && rect.y < bottom)
        .then(|| PixelRect::new(rect.x, rect.y, right - rect.x, bottom - rect.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_reappears_when_a_free_surface_is_reused_two_presents_later() {
        let (width, height) = (8, 5);
        let mut history = SurfaceDamageHistory::new(width, height);
        let mut source = vec![0xff000000; width * height];
        let mut surfaces = vec![vec![0; width * height]; SURFACE_COUNT];

        let first = history.next_free(|_| false).unwrap();
        assert_eq!(first, 0);
        publish(
            &mut history,
            &mut surfaces,
            first,
            &source,
            &FrameDamage::Full,
            width,
        );

        let damage_n = PixelRect::new(1, 1, 2, 1);
        source[width + 1..width + 3].fill(0xff12ab34);
        let frame_n = FrameDamage::Rects(vec![damage_n]);
        let index_n = history.next_free(|index| index == 2).unwrap();
        assert_eq!(index_n, 1);
        publish(
            &mut history,
            &mut surfaces,
            index_n,
            &source,
            &frame_n,
            width,
        );

        let damage_n_plus_1 = PixelRect::new(5, 3, 1, 1);
        source[3 * width + 5] = 0xffabcdef;
        let frame_n_plus_1 = FrameDamage::Rects(vec![damage_n_plus_1]);
        let index_n_plus_1 = history.next_free(|index| index == 0).unwrap();
        assert_eq!(index_n_plus_1, 2);
        publish(
            &mut history,
            &mut surfaces,
            index_n_plus_1,
            &source,
            &frame_n_plus_1,
            width,
        );

        // Reuse the surface from the frame before N. Its N+2 readback must
        // replay the N and N+1 damage it missed while the other slots showed.
        let damage_n_plus_2 = PixelRect::new(6, 0, 1, 1);
        source[6] = 0xfffedcba;
        let frame_n_plus_2 = FrameDamage::Rects(vec![damage_n_plus_2]);
        let index_n_plus_2 = history.next_free(|_| false).unwrap();
        assert_eq!(index_n_plus_2, 0);
        publish(
            &mut history,
            &mut surfaces,
            index_n_plus_2,
            &source,
            &frame_n_plus_2,
            width,
        );

        assert_eq!(surfaces[index_n_plus_2], source);
        assert_eq!(history.current_index(), Some(index_n_plus_2));
    }

    #[test]
    fn stale_rect_cap_and_full_frames_collapse_to_one_full_copy() {
        let bounds = PixelRect::new(0, 0, 32, 32);
        let stale: Vec<PixelRect> = (0..260)
            .map(|step| PixelRect::new(step % 32, step / 32, 1, 1))
            .collect();
        let history = SurfaceDamageHistory {
            bounds,
            stale: [Vec::new(), stale.clone(), stale],
            current: Some(0),
        };
        assert_eq!(history.plan(1, &FrameDamage::rects()), [bounds]);
        assert_eq!(history.plan(2, &FrameDamage::Full), [bounds]);
    }

    #[test]
    fn stale_damage_history_stays_bounded_while_a_surface_is_busy() {
        let mut history = SurfaceDamageHistory::new(32, 32);
        let bounds = history.bounds;
        for stale in &mut history.stale {
            stale.clear();
        }
        history.current = Some(0);

        for frame in 0..=MAX_REPLAY_RECTS {
            let index = history.next_free(|slot| slot == 0).unwrap();
            let damage = FrameDamage::Rects(vec![PixelRect::new(frame % 32, 0, 1, 1)]);
            history.committed(index, &damage);
        }
        assert_eq!(history.stale[0].len(), 1);
        assert_eq!(history.stale[0][0], bounds);

        for frame in MAX_REPLAY_RECTS + 1..10_000 {
            let index = history.next_free(|slot| slot == 0).unwrap();
            let damage = FrameDamage::Rects(vec![PixelRect::new(frame % 32, 1, 1, 1)]);
            history.committed(index, &damage);
        }

        assert_eq!(history.stale[0].len(), 1);
        assert_eq!(history.stale[0][0], bounds);
        assert!(history
            .stale
            .iter()
            .all(|rects| rects.len() <= MAX_REPLAY_RECTS));
        assert_eq!(history.plan(0, &FrameDamage::rects()), [bounds]);
    }

    #[test]
    fn rectangles_are_clipped_without_overflow() {
        let mut history = SurfaceDamageHistory::new(4, 3);
        history.stale[0].clear();
        let clipped = PixelRect::new(3, 2, 1, 1);
        let plan = history.plan(
            0,
            &FrameDamage::Rects(vec![
                PixelRect::new(3, 2, usize::MAX, usize::MAX),
                PixelRect::new(4, 0, 1, 1),
                PixelRect::new(0, 0, 0, 1),
            ]),
        );
        assert_eq!(plan, [clipped]);
    }

    fn publish(
        history: &mut SurfaceDamageHistory,
        surfaces: &mut [Vec<u32>],
        index: usize,
        source: &[u32],
        damage: &FrameDamage,
        width: usize,
    ) {
        for rect in history.plan(index, damage) {
            for y in rect.y..rect.y + rect.height {
                let row = y * width + rect.x..y * width + rect.x + rect.width;
                surfaces[index][row.clone()].copy_from_slice(&source[row]);
            }
        }
        history.committed(index, damage);
    }
}
