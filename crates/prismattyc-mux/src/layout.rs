//! Binary pane layout tree (geometry algorithms land in).

use crate::ids::PaneId;

/// Split orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Left | Right
    Horizontal,
    /// Top / Bottom
    Vertical,
}

/// Interior node of a pane layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    pub axis: Axis,
    /// Fraction of space given to `first` (geometry minima enforced in).
    pub ratio: f64,
    pub first: Box<PaneLayout>,
    pub second: Box<PaneLayout>,
}

/// Immutable binary split tree for one window.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneLayout {
    Leaf(PaneId),
    Split(Split),
}

impl PaneLayout {
    pub fn leaf(pane: PaneId) -> Self {
        Self::Leaf(pane)
    }

    /// Depth-first leaf order (stable child order for presentation).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split(split) => {
                split.first.collect_panes(out);
                split.second.collect_panes(out);
            }
        }
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split(split) => {
                split.first.contains_pane(pane) || split.second.contains_pane(pane)
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split(split) => split.first.pane_count() + split.second.pane_count(),
        }
    }

    /// The same tree with leaves `a` and `b` exchanged; splits and ratios
    /// stay (tmux `swap-pane`). `None` when either pane is absent or they
    /// are the same pane (PT-125).
    pub fn swap_leaves(&self, a: PaneId, b: PaneId) -> Option<Self> {
        if a == b || !self.contains_pane(a) || !self.contains_pane(b) {
            return None;
        }
        Some(self.map_leaves(&|id| {
            if id == a {
                b
            } else if id == b {
                a
            } else {
                id
            }
        }))
    }

    /// The same tree with every leaf moved `delta` slots along the
    /// depth-first order, wrapping (tmux `rotate-window`: +1 = -D, -1 = -U).
    /// `None` for a single pane or a zero delta.
    pub fn rotate_leaves(&self, delta: i32) -> Option<Self> {
        let order = self.panes();
        let n = order.len();
        if n < 2 || delta == 0 {
            return None;
        }
        let shift = delta.rem_euclid(n as i32) as usize;
        if shift == 0 {
            return None;
        }
        // Slot i receives the pane that sat `shift` slots earlier.
        let rotated: Vec<PaneId> = (0..n).map(|i| order[(i + n - shift) % n]).collect();
        Some(self.map_leaves(&|id| {
            let slot = order.iter().position(|p| *p == id).expect("leaf in order");
            rotated[slot]
        }))
    }

    fn map_leaves(&self, f: &dyn Fn(PaneId) -> PaneId) -> Self {
        match self {
            Self::Leaf(id) => Self::Leaf(f(*id)),
            Self::Split(split) => Self::Split(Split {
                axis: split.axis,
                ratio: split.ratio,
                first: Box::new(split.first.map_leaves(f)),
                second: Box::new(split.second.map_leaves(f)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(n: u64) -> PaneLayout {
        PaneLayout::Leaf(PaneId::from_raw(n))
    }

    fn tree() -> PaneLayout {
        // (1 | (2 / 3))
        PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.4,
            first: Box::new(leaf(1)),
            second: Box::new(PaneLayout::Split(Split {
                axis: Axis::Vertical,
                ratio: 0.6,
                first: Box::new(leaf(2)),
                second: Box::new(leaf(3)),
            })),
        })
    }

    fn ids(layout: &PaneLayout) -> Vec<u64> {
        layout.panes().into_iter().map(|p| p.get()).collect()
    }

    #[test]
    fn swap_leaves_exchanges_two_panes_and_keeps_the_shape() {
        let swapped = tree()
            .swap_leaves(PaneId::from_raw(1), PaneId::from_raw(3))
            .unwrap();
        assert_eq!(ids(&swapped), vec![3, 2, 1]);
        let PaneLayout::Split(root) = &swapped else {
            panic!("root split")
        };
        assert_eq!(root.ratio, 0.4, "ratios untouched");
        assert!(tree()
            .swap_leaves(PaneId::from_raw(1), PaneId::from_raw(1))
            .is_none());
        assert!(tree()
            .swap_leaves(PaneId::from_raw(1), PaneId::from_raw(9))
            .is_none());
    }

    #[test]
    fn rotate_leaves_shifts_the_order_and_wraps() {
        assert_eq!(ids(&tree().rotate_leaves(1).unwrap()), vec![3, 1, 2]);
        assert_eq!(ids(&tree().rotate_leaves(-1).unwrap()), vec![2, 3, 1]);
        assert_eq!(
            ids(&tree().rotate_leaves(4).unwrap()),
            vec![3, 1, 2],
            "wraps"
        );
        assert!(tree().rotate_leaves(3).is_none(), "full turn is a no-op");
        assert!(tree().rotate_leaves(0).is_none());
        assert!(leaf(1).rotate_leaves(1).is_none());
    }
}
