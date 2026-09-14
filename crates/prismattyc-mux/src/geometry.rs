//! Integer-cell layout geometry (PRD §2.8.6).

use crate::ids::PaneId;
use crate::layout::{Axis, PaneLayout, Split};

/// Inclusive cell rectangle in window-local coordinates (origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

/// Defaults used when host does not override.
pub const DEFAULT_MIN_COLS: usize = 2;
pub const DEFAULT_MIN_ROWS: usize = 1;

/// Geometry / layout mutation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeometryError {
    TooSmall,
    UnknownPane(PaneId),
    InvalidRatio,
    LastPane,
    /// Coordinate or minimum-footprint arithmetic would overflow `usize`.
    Overflow,
}

impl std::fmt::Display for GeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall => write!(f, "layout cannot satisfy minimum pane geometry"),
            Self::UnknownPane(id) => write!(f, "pane {id} not in layout"),
            Self::InvalidRatio => write!(f, "split ratio must be in (0, 1)"),
            Self::LastPane => write!(f, "cannot close the last pane in a window"),
            Self::Overflow => write!(f, "geometry arithmetic overflow"),
        }
    }
}

impl std::error::Error for GeometryError {}

/// Minimum column footprint of a layout subtree at `min_cols` per leaf.
///
/// Saturates on overflow for quick capacity estimates; prefer
/// [`try_subtree_min_cols`] when assigning rectangles (structured error).
pub fn subtree_min_cols(layout: &PaneLayout, min_cols: usize) -> usize {
    try_subtree_min_cols(layout, min_cols).unwrap_or(usize::MAX)
}

/// Minimum row footprint of a layout subtree at `min_rows` per leaf.
pub fn subtree_min_rows(layout: &PaneLayout, min_rows: usize) -> usize {
    try_subtree_min_rows(layout, min_rows).unwrap_or(usize::MAX)
}

/// Checked column footprint; [`GeometryError::Overflow`] if sum exceeds `usize`.
pub fn try_subtree_min_cols(layout: &PaneLayout, min_cols: usize) -> Result<usize, GeometryError> {
    match layout {
        PaneLayout::Leaf(_) => Ok(min_cols),
        PaneLayout::Split(split) => match split.axis {
            Axis::Horizontal => {
                let a = try_subtree_min_cols(&split.first, min_cols)?;
                let b = try_subtree_min_cols(&split.second, min_cols)?;
                a.checked_add(b).ok_or(GeometryError::Overflow)
            }
            Axis::Vertical => Ok(try_subtree_min_cols(&split.first, min_cols)?
                .max(try_subtree_min_cols(&split.second, min_cols)?)),
        },
    }
}

/// Checked row footprint; [`GeometryError::Overflow`] if sum exceeds `usize`.
pub fn try_subtree_min_rows(layout: &PaneLayout, min_rows: usize) -> Result<usize, GeometryError> {
    match layout {
        PaneLayout::Leaf(_) => Ok(min_rows),
        PaneLayout::Split(split) => match split.axis {
            Axis::Vertical => {
                let a = try_subtree_min_rows(&split.first, min_rows)?;
                let b = try_subtree_min_rows(&split.second, min_rows)?;
                a.checked_add(b).ok_or(GeometryError::Overflow)
            }
            Axis::Horizontal => Ok(try_subtree_min_rows(&split.first, min_rows)?
                .max(try_subtree_min_rows(&split.second, min_rows)?)),
        },
    }
}

/// Validate that `origin + extent` does not overflow (rect is in-bounds for usize).
fn checked_extent(origin: usize, extent: usize) -> Result<(), GeometryError> {
    origin
        .checked_add(extent)
        .map(|_| ())
        .ok_or(GeometryError::Overflow)
}

/// Assign cell rectangles to every leaf. Deterministic left-to-right / top-to-bottom
/// integer split: first child gets `floor(size * ratio)` clamped so both **subtrees**
/// keep at least their nested minima when possible; if the total cannot host the
/// tree at minima, returns [`GeometryError::TooSmall`]. Never panics on extreme
/// minima/origins — returns [`GeometryError::Overflow`] instead.
pub fn layout_to_rects(
    layout: &PaneLayout,
    bounds: CellRect,
    min_cols: usize,
    min_rows: usize,
) -> Result<Vec<(PaneId, CellRect)>, GeometryError> {
    checked_extent(bounds.col, bounds.cols)?;
    checked_extent(bounds.row, bounds.rows)?;
    let need_c = try_subtree_min_cols(layout, min_cols)?;
    let need_r = try_subtree_min_rows(layout, min_rows)?;
    if bounds.cols < need_c || bounds.rows < need_r {
        return Err(GeometryError::TooSmall);
    }
    let mut out = Vec::with_capacity(layout.pane_count());
    assign(layout, bounds, min_cols, min_rows, &mut out)?;
    Ok(out)
}

fn assign(
    layout: &PaneLayout,
    bounds: CellRect,
    min_cols: usize,
    min_rows: usize,
    out: &mut Vec<(PaneId, CellRect)>,
) -> Result<(), GeometryError> {
    match layout {
        PaneLayout::Leaf(id) => {
            if bounds.cols < min_cols || bounds.rows < min_rows {
                return Err(GeometryError::TooSmall);
            }
            checked_extent(bounds.col, bounds.cols)?;
            checked_extent(bounds.row, bounds.rows)?;
            out.push((*id, bounds));
            Ok(())
        }
        PaneLayout::Split(split) => {
            let (first_bounds, second_bounds) = split_bounds(bounds, split, min_cols, min_rows)?;
            assign(&split.first, first_bounds, min_cols, min_rows, out)?;
            assign(&split.second, second_bounds, min_cols, min_rows, out)?;
            Ok(())
        }
    }
}

fn split_bounds(
    bounds: CellRect,
    split: &Split,
    min_cols: usize,
    min_rows: usize,
) -> Result<(CellRect, CellRect), GeometryError> {
    let ratio = split.ratio;
    if !(ratio > 0.0 && ratio < 1.0) {
        return Err(GeometryError::InvalidRatio);
    }
    match split.axis {
        Axis::Horizontal => {
            // Left | Right along columns — clamp using **subtree** minima.
            let first_min = try_subtree_min_cols(&split.first, min_cols)?;
            let second_min = try_subtree_min_cols(&split.second, min_cols)?;
            let need = first_min
                .checked_add(second_min)
                .ok_or(GeometryError::Overflow)?;
            if bounds.cols < need {
                return Err(GeometryError::TooSmall);
            }
            // hi is always >= first_min because cols >= first_min + second_min.
            let hi = bounds.cols - second_min;
            if first_min > hi {
                return Err(GeometryError::TooSmall);
            }
            let ideal = ((bounds.cols as f64) * ratio).floor() as usize;
            let first = ideal.clamp(first_min, hi);
            let second = bounds.cols - first;
            if second < second_min {
                return Err(GeometryError::TooSmall);
            }
            let second_col = bounds
                .col
                .checked_add(first)
                .ok_or(GeometryError::Overflow)?;
            checked_extent(second_col, second)?;
            Ok((
                CellRect {
                    col: bounds.col,
                    row: bounds.row,
                    cols: first,
                    rows: bounds.rows,
                },
                CellRect {
                    col: second_col,
                    row: bounds.row,
                    cols: second,
                    rows: bounds.rows,
                },
            ))
        }
        Axis::Vertical => {
            let first_min = try_subtree_min_rows(&split.first, min_rows)?;
            let second_min = try_subtree_min_rows(&split.second, min_rows)?;
            let need = first_min
                .checked_add(second_min)
                .ok_or(GeometryError::Overflow)?;
            if bounds.rows < need {
                return Err(GeometryError::TooSmall);
            }
            let hi = bounds.rows - second_min;
            if first_min > hi {
                return Err(GeometryError::TooSmall);
            }
            let ideal = ((bounds.rows as f64) * ratio).floor() as usize;
            let first = ideal.clamp(first_min, hi);
            let second = bounds.rows - first;
            if second < second_min {
                return Err(GeometryError::TooSmall);
            }
            let second_row = bounds
                .row
                .checked_add(first)
                .ok_or(GeometryError::Overflow)?;
            checked_extent(second_row, second)?;
            Ok((
                CellRect {
                    col: bounds.col,
                    row: bounds.row,
                    cols: bounds.cols,
                    rows: first,
                },
                CellRect {
                    col: bounds.col,
                    row: second_row,
                    cols: bounds.cols,
                    rows: second,
                },
            ))
        }
    }
}

/// Split `target` leaf into `target` + `new_pane` with `axis` / `ratio`.
/// Structural only: does not mutate the input layout on error.
pub fn split_leaf(
    layout: &PaneLayout,
    target: PaneId,
    new_pane: PaneId,
    axis: Axis,
    ratio: f64,
) -> Result<PaneLayout, GeometryError> {
    if !(ratio > 0.0 && ratio < 1.0) {
        return Err(GeometryError::InvalidRatio);
    }
    if !layout.contains_pane(target) {
        return Err(GeometryError::UnknownPane(target));
    }
    if layout.contains_pane(new_pane) {
        // Already present — refuse double place.
        return Err(GeometryError::UnknownPane(new_pane));
    }
    replace_leaf(layout, target, |id| {
        PaneLayout::Split(Split {
            axis,
            ratio,
            first: Box::new(PaneLayout::leaf(id)),
            second: Box::new(PaneLayout::leaf(new_pane)),
        })
    })
    .ok_or(GeometryError::UnknownPane(target))
}

fn replace_leaf<F>(layout: &PaneLayout, target: PaneId, f: F) -> Option<PaneLayout>
where
    F: Fn(PaneId) -> PaneLayout + Copy,
{
    match layout {
        PaneLayout::Leaf(id) if *id == target => Some(f(*id)),
        PaneLayout::Leaf(_) => None,
        PaneLayout::Split(split) => {
            if let Some(first) = replace_leaf(&split.first, target, f) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: Box::new(first),
                    second: split.second.clone(),
                }));
            }
            if let Some(second) = replace_leaf(&split.second, target, f) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: split.first.clone(),
                    second: Box::new(second),
                }));
            }
            None
        }
    }
}

/// Build a left-to-right row of `leaves` with even column shares.
///
/// Ratios are `1/k` at each remaining-k split so integer assignment stays even
/// to within one cell. Empty input is [`GeometryError::TooSmall`].
pub fn even_horizontal_row(leaves: &[PaneId]) -> Result<PaneLayout, GeometryError> {
    even_along(leaves, Axis::Horizontal)
}

/// Build a top-to-bottom column of `leaves` with even row shares.
///
/// Same ratio rule as [`even_horizontal_row`], on [`Axis::Vertical`]. Empty
/// input is [`GeometryError::TooSmall`].
pub fn even_vertical_column(leaves: &[PaneId]) -> Result<PaneLayout, GeometryError> {
    even_along(leaves, Axis::Vertical)
}

fn even_along(leaves: &[PaneId], axis: Axis) -> Result<PaneLayout, GeometryError> {
    match leaves {
        [] => Err(GeometryError::TooSmall),
        [only] => Ok(PaneLayout::leaf(*only)),
        [first, rest @ ..] => {
            let n = leaves.len() as f64;
            Ok(PaneLayout::Split(Split {
                axis,
                ratio: 1.0 / n,
                first: Box::new(PaneLayout::leaf(*first)),
                second: Box::new(even_along(rest, axis)?),
            }))
        }
    }
}

/// Named window arrangement (PT-132). Rebuilds the split tree from the
/// current leaf order. `main` is the focused pane for main-vertical /
/// main-horizontal; ignored for even/grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    MainVertical,
    MainHorizontal,
    EvenHorizontal,
    EvenVertical,
    Grid,
}

/// One large left pane; remaining leaves stacked on the right.
pub fn main_vertical(leaves: &[PaneId]) -> Result<PaneLayout, GeometryError> {
    match leaves {
        [] => Err(GeometryError::TooSmall),
        [only] => Ok(PaneLayout::leaf(*only)),
        [main, rest @ ..] => Ok(PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(*main)),
            second: Box::new(even_vertical_column(rest)?),
        })),
    }
}

/// One large top pane; remaining leaves in an even row below.
pub fn main_horizontal(leaves: &[PaneId]) -> Result<PaneLayout, GeometryError> {
    match leaves {
        [] => Err(GeometryError::TooSmall),
        [only] => Ok(PaneLayout::leaf(*only)),
        [main, rest @ ..] => Ok(PaneLayout::Split(Split {
            axis: Axis::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(*main)),
            second: Box::new(even_horizontal_row(rest)?),
        })),
    }
}

/// Rebuild `leaves` into `kind`. When `main` is set and present, it becomes
/// the main pane for main-vertical / main-horizontal.
pub fn apply_arrangement(
    kind: Arrangement,
    leaves: &[PaneId],
    main: Option<PaneId>,
) -> Result<PaneLayout, GeometryError> {
    let ordered = match (kind, main) {
        (Arrangement::MainVertical | Arrangement::MainHorizontal, Some(id))
            if leaves.contains(&id) =>
        {
            let mut order = Vec::with_capacity(leaves.len());
            order.push(id);
            order.extend(leaves.iter().copied().filter(|pane| *pane != id));
            order
        }
        _ => leaves.to_vec(),
    };
    match kind {
        Arrangement::MainVertical => main_vertical(&ordered),
        Arrangement::MainHorizontal => main_horizontal(&ordered),
        Arrangement::EvenHorizontal => even_horizontal_row(&ordered),
        Arrangement::EvenVertical => even_vertical_column(&ordered),
        Arrangement::Grid => even_two_row_grid(&ordered),
    }
}

/// Two stacked even rows (2×2 quadrants when `leaves.len() == 4`).
///
/// Top row gets `ceil(n/2)` panes, bottom the rest. Empty input is
/// [`GeometryError::TooSmall`].
pub fn even_two_row_grid(leaves: &[PaneId]) -> Result<PaneLayout, GeometryError> {
    match leaves {
        [] => Err(GeometryError::TooSmall),
        [_] => even_horizontal_row(leaves),
        _ => {
            let top_n = leaves.len().div_ceil(2);
            Ok(PaneLayout::Split(Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                first: Box::new(even_horizontal_row(&leaves[..top_n])?),
                second: Box::new(even_horizontal_row(&leaves[top_n..])?),
            }))
        }
    }
}

/// Remove `pane` and collapse its parent split to the sibling. Last pane → error.
pub fn close_pane_in_layout(
    layout: &PaneLayout,
    pane: PaneId,
) -> Result<PaneLayout, GeometryError> {
    if matches!(layout, PaneLayout::Leaf(id) if *id == pane) {
        return Err(GeometryError::LastPane);
    }
    if !layout.contains_pane(pane) {
        return Err(GeometryError::UnknownPane(pane));
    }
    close_rec(layout, pane).ok_or(GeometryError::UnknownPane(pane))
}

fn close_rec(layout: &PaneLayout, pane: PaneId) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(_) => None,
        PaneLayout::Split(split) => {
            if matches!(split.first.as_ref(), PaneLayout::Leaf(id) if *id == pane) {
                return Some(*split.second.clone());
            }
            if matches!(split.second.as_ref(), PaneLayout::Leaf(id) if *id == pane) {
                return Some(*split.first.clone());
            }
            if let Some(first) = close_rec(&split.first, pane) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: Box::new(first),
                    second: split.second.clone(),
                }));
            }
            if let Some(second) = close_rec(&split.second, pane) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: split.first.clone(),
                    second: Box::new(second),
                }));
            }
            None
        }
    }
}

/// Change ratio of the parent split of leaf `pane` if parent exists.
pub fn set_parent_ratio(
    layout: &PaneLayout,
    pane: PaneId,
    ratio: f64,
) -> Result<PaneLayout, GeometryError> {
    if !(ratio > 0.0 && ratio < 1.0) {
        return Err(GeometryError::InvalidRatio);
    }
    set_ratio_rec(layout, pane, ratio).ok_or(GeometryError::UnknownPane(pane))
}

fn set_ratio_rec(layout: &PaneLayout, pane: PaneId, ratio: f64) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(_) => None,
        PaneLayout::Split(split) => {
            let first_is = matches!(split.first.as_ref(), PaneLayout::Leaf(id) if *id == pane);
            let second_is = matches!(split.second.as_ref(), PaneLayout::Leaf(id) if *id == pane);
            if first_is || second_is {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio,
                    first: split.first.clone(),
                    second: split.second.clone(),
                }));
            }
            if let Some(first) = set_ratio_rec(&split.first, pane, ratio) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: Box::new(first),
                    second: split.second.clone(),
                }));
            }
            if let Some(second) = set_ratio_rec(&split.second, pane, ratio) {
                return Some(PaneLayout::Split(Split {
                    axis: split.axis,
                    ratio: split.ratio,
                    first: split.first.clone(),
                    second: Box::new(second),
                }));
            }
            None
        }
    }
}

/// Deterministic suggested focus after closing `closed` (for client view state).
///
/// Prefers the prior focus when it still exists; otherwise the first leaf of the
/// remaining layout (stable DFS order).
pub fn suggested_focus_after_close(
    prior_focus: PaneId,
    closed: PaneId,
    new_layout: &PaneLayout,
) -> PaneId {
    if prior_focus != closed && new_layout.contains_pane(prior_focus) {
        prior_focus
    } else {
        new_layout.panes()[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PaneId;

    fn p(n: u64) -> PaneId {
        PaneId::from_raw(n)
    }

    #[test]
    fn three_pane_horizontal_rects() {
        // (1 | 2) | 3  via nested splits
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Split(Split {
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(PaneLayout::leaf(p(1))),
                second: Box::new(PaneLayout::leaf(p(2))),
            })),
            second: Box::new(PaneLayout::leaf(p(3))),
        });
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 24,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let total_cols: usize = rects.iter().map(|(_, r)| r.cols).sum();
        assert_eq!(total_cols, 80);
        for (_, r) in &rects {
            assert_eq!(r.rows, 24);
            assert!(r.cols >= 2);
        }
    }

    #[test]
    fn min_geometry_rejects() {
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p(1))),
            second: Box::new(PaneLayout::leaf(p(2))),
        });
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 3,
            rows: 10,
        };
        // min_cols=2 each => need 4
        assert_eq!(
            layout_to_rects(&layout, bounds, 2, 1),
            Err(GeometryError::TooSmall)
        );
    }

    #[test]
    fn nested_minima_extreme_ratio_feasible() {
        // (p1|p2)|p3  — six columns, min_cols=2, root ratio=0.1 must yield [2,2,2]
        // rather than clamping the first subtree to a single-leaf minimum.
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.1,
            first: Box::new(PaneLayout::Split(Split {
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(PaneLayout::leaf(p(1))),
                second: Box::new(PaneLayout::leaf(p(2))),
            })),
            second: Box::new(PaneLayout::leaf(p(3))),
        });
        assert_eq!(subtree_min_cols(&layout, 2), 6);
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 6,
            rows: 10,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let cols: Vec<usize> = rects.iter().map(|(_, r)| r.cols).collect();
        assert_eq!(cols, vec![2, 2, 2]);
        assert_eq!(rects.iter().map(|(_, r)| r.cols).sum::<usize>(), 6);
    }

    #[test]
    fn nested_mixed_axis_conservation_and_non_overlap() {
        // Horizontal root; first child vertical split of two leaves.
        //   [ p1 ]
        //   [ p2 ] | p3
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Split(Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                first: Box::new(PaneLayout::leaf(p(1))),
                second: Box::new(PaneLayout::leaf(p(2))),
            })),
            second: Box::new(PaneLayout::leaf(p(3))),
        });
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 40,
            rows: 20,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);

        // Area conservation: union covers bounds exactly (no gaps in this tree).
        let mut cells = std::collections::HashSet::new();
        for (_, r) in &rects {
            assert!(r.cols >= 2 && r.rows >= 1);
            for c in r.col..r.col + r.cols {
                for row in r.row..r.row + r.rows {
                    assert!(cells.insert((c, row)), "overlap at ({c},{row})");
                }
            }
        }
        assert_eq!(cells.len(), bounds.cols * bounds.rows);

        // Minima on nested vertical subtree.
        assert_eq!(subtree_min_rows(&layout, 1), 2);
        assert_eq!(subtree_min_cols(&layout, 2), 4);
    }

    #[test]
    fn split_and_close_roundtrip() {
        let root = PaneLayout::leaf(p(1));
        let split = split_leaf(&root, p(1), p(2), Axis::Vertical, 0.5).unwrap();
        assert_eq!(split.pane_count(), 2);
        let again = split_leaf(&split, p(2), p(3), Axis::Horizontal, 0.4).unwrap();
        assert_eq!(again.pane_count(), 3);
        let closed = close_pane_in_layout(&again, p(2)).unwrap();
        assert_eq!(closed.pane_count(), 2);
        assert!(!closed.contains_pane(p(2)));
        assert!(closed.contains_pane(p(1)));
        assert!(closed.contains_pane(p(3)));
    }

    #[test]
    fn main_vertical_puts_first_leaf_on_the_left() {
        let layout = main_vertical(&[p(1), p(2), p(3)]).unwrap();
        assert_eq!(layout.panes(), vec![p(1), p(2), p(3)]);
        let PaneLayout::Split(split) = &layout else {
            panic!("expected split");
        };
        assert_eq!(split.axis, Axis::Horizontal);
        assert!(matches!(split.first.as_ref(), PaneLayout::Leaf(id) if *id == p(1)));
    }

    #[test]
    fn apply_arrangement_moves_focused_pane_to_main() {
        let layout =
            apply_arrangement(Arrangement::MainVertical, &[p(1), p(2), p(3)], Some(p(3))).unwrap();
        assert_eq!(layout.panes(), vec![p(3), p(1), p(2)]);
        let PaneLayout::Split(split) = &layout else {
            panic!("expected split");
        };
        assert_eq!(split.axis, Axis::Horizontal);
        assert!(matches!(split.first.as_ref(), PaneLayout::Leaf(id) if *id == p(3)));
    }

    #[test]
    fn apply_arrangement_empty_is_too_small() {
        for kind in [
            Arrangement::MainVertical,
            Arrangement::MainHorizontal,
            Arrangement::EvenHorizontal,
            Arrangement::EvenVertical,
            Arrangement::Grid,
        ] {
            assert_eq!(
                apply_arrangement(kind, &[], None),
                Err(GeometryError::TooSmall),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn main_vertical_three_panes_rect_sums_cover_bounds() {
        let layout = main_vertical(&[p(1), p(2), p(3)]).unwrap();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 24,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let find = |id| {
            rects
                .iter()
                .find(|(pane, _)| *pane == id)
                .map(|(_, r)| *r)
                .unwrap()
        };
        let main = find(p(1));
        let top = find(p(2));
        let bottom = find(p(3));
        assert_eq!(main.col, 0);
        assert!(main.col + main.cols == top.col);
        assert_eq!(main.cols + top.cols, 80);
        assert_eq!(main.rows, 24);
        assert_eq!(top.row, 0);
        assert_eq!(top.row + top.rows, bottom.row);
        assert_eq!(top.rows + bottom.rows, 24);
        assert_eq!(top.cols, bottom.cols);
        assert_eq!(top.col, bottom.col);
        assert!(main.col < top.col);
    }

    #[test]
    fn main_vertical_tiny_bounds_are_too_small() {
        let layout = main_vertical(&[p(1), p(2), p(3)]).unwrap();
        let too_narrow = CellRect {
            col: 0,
            row: 0,
            cols: 3,
            rows: 24,
        };
        assert_eq!(
            layout_to_rects(&layout, too_narrow, DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS),
            Err(GeometryError::TooSmall)
        );
        let too_short = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 1,
        };
        assert_eq!(
            layout_to_rects(&layout, too_short, DEFAULT_MIN_COLS, DEFAULT_MIN_ROWS),
            Err(GeometryError::TooSmall)
        );
    }

    #[test]
    fn even_horizontal_row_three_panes_are_within_one_cell() {
        let layout = even_horizontal_row(&[p(1), p(2), p(3)]).unwrap();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 90,
            rows: 24,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let widths: Vec<usize> = rects.iter().map(|(_, r)| r.cols).collect();
        assert_eq!(widths.iter().sum::<usize>(), 90);
        let min = *widths.iter().min().unwrap();
        let max = *widths.iter().max().unwrap();
        assert!(max - min <= 1, "uneven widths {widths:?}");
        assert_eq!(rects[0].0, p(1));
        assert_eq!(rects[1].0, p(2));
        assert_eq!(rects[2].0, p(3));
    }

    #[test]
    fn even_vertical_column_three_panes_are_within_one_cell() {
        let layout = even_vertical_column(&[p(1), p(2), p(3)]).unwrap();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 30,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let heights: Vec<usize> = rects.iter().map(|(_, r)| r.rows).collect();
        assert_eq!(heights.iter().sum::<usize>(), 30);
        let min = *heights.iter().min().unwrap();
        let max = *heights.iter().max().unwrap();
        assert!(max - min <= 1, "uneven heights {heights:?}");
        assert_eq!(rects[0].0, p(1));
        assert_eq!(rects[1].0, p(2));
        assert_eq!(rects[2].0, p(3));
        assert!(rects[0].1.row < rects[1].1.row);
        assert!(rects[1].1.row < rects[2].1.row);
    }

    fn layout_shape(layout: &PaneLayout) -> String {
        match layout {
            PaneLayout::Leaf(_) => "L".into(),
            PaneLayout::Split(split) => {
                let axis = match split.axis {
                    Axis::Horizontal => "H",
                    Axis::Vertical => "V",
                };
                format!(
                    "[{axis} {} {}]",
                    layout_shape(&split.first),
                    layout_shape(&split.second)
                )
            }
        }
    }

    #[test]
    fn preset_builders_tree_shape_one_to_four_panes() {
        let ids = [p(1), p(2), p(3), p(4)];
        for n in 1..=4 {
            let leaves = &ids[..n];
            let split_h = even_horizontal_row(leaves).unwrap();
            let split_v = even_vertical_column(leaves).unwrap();
            let grid = even_two_row_grid(leaves).unwrap();
            let expected_h = match n {
                1 => "L",
                2 => "[H L L]",
                3 => "[H L [H L L]]",
                4 => "[H L [H L [H L L]]]",
                _ => unreachable!(),
            };
            let expected_v = expected_h.replace('H', "V");
            let expected_grid = match n {
                1 => "L",
                2 => "[V L L]",
                3 => "[V [H L L] L]",
                4 => "[V [H L L] [H L L]]",
                _ => unreachable!(),
            };
            assert_eq!(layout_shape(&split_h), expected_h, "split-h n={n}");
            assert_eq!(layout_shape(&split_v), expected_v, "split-v n={n}");
            assert_eq!(layout_shape(&grid), expected_grid, "grid n={n}");
            assert_eq!(split_h.panes(), leaves);
            assert_eq!(split_v.panes(), leaves);
            assert_eq!(grid.panes(), leaves);
        }
    }

    #[test]
    fn even_two_row_grid_three_panes_are_two_plus_one() {
        let layout = even_two_row_grid(&[p(1), p(2), p(3)]).unwrap();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 24,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 3);
        let find = |id| {
            rects
                .iter()
                .find(|(p, _)| *p == id)
                .map(|(_, r)| *r)
                .unwrap()
        };
        let a = find(p(1));
        let b = find(p(2));
        let c = find(p(3));
        assert_eq!(a.row, b.row);
        assert!(a.row < c.row);
        assert_eq!(a.col + a.cols, b.col);
        assert_eq!(a.cols + b.cols, 80);
        assert_eq!(c.cols, 80);
        assert_eq!(a.rows + c.rows, 24);
    }

    #[test]
    fn even_two_row_grid_four_panes_are_quadrants() {
        let layout = even_two_row_grid(&[p(1), p(2), p(3), p(4)]).unwrap();
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: 80,
            rows: 24,
        };
        let rects = layout_to_rects(&layout, bounds, 2, 1).unwrap();
        assert_eq!(rects.len(), 4);
        let find = |id| {
            rects
                .iter()
                .find(|(p, _)| *p == id)
                .map(|(_, r)| *r)
                .unwrap()
        };
        let a = find(p(1));
        let b = find(p(2));
        let c = find(p(3));
        let d = find(p(4));
        assert_eq!(a.row, b.row);
        assert_eq!(c.row, d.row);
        assert!(a.row < c.row);
        assert_eq!(a.col, c.col);
        assert_eq!(b.col, d.col);
        assert!(a.col < b.col);
        assert!((a.cols as i32 - b.cols as i32).abs() <= 1);
        assert!((a.rows as i32 - c.rows as i32).abs() <= 1);
        assert_eq!(a.cols + b.cols, 80);
        assert_eq!(a.rows + c.rows, 24);
    }

    #[test]
    fn close_last_pane_fails() {
        let root = PaneLayout::leaf(p(1));
        assert_eq!(
            close_pane_in_layout(&root, p(1)),
            Err(GeometryError::LastPane)
        );
    }

    #[test]
    fn extreme_minima_returns_error_not_panic() {
        // min_cols=usize::MAX on a two-leaf tree must not panic in clamp.
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p(1))),
            second: Box::new(PaneLayout::leaf(p(2))),
        });
        let bounds = CellRect {
            col: 0,
            row: 0,
            cols: usize::MAX,
            rows: 10,
        };
        let err = layout_to_rects(&layout, bounds, usize::MAX, 1).unwrap_err();
        assert!(
            matches!(err, GeometryError::Overflow | GeometryError::TooSmall),
            "got {err:?}"
        );

        // Vertical analogue with extreme min_rows.
        let layout_v = PaneLayout::Split(Split {
            axis: Axis::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p(1))),
            second: Box::new(PaneLayout::leaf(p(2))),
        });
        let bounds_v = CellRect {
            col: 0,
            row: 0,
            cols: 10,
            rows: usize::MAX,
        };
        let err_v = layout_to_rects(&layout_v, bounds_v, 2, usize::MAX).unwrap_err();
        assert!(
            matches!(err_v, GeometryError::Overflow | GeometryError::TooSmall),
            "got {err_v:?}"
        );
    }

    #[test]
    fn origin_overflow_returns_error_not_panic() {
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p(1))),
            second: Box::new(PaneLayout::leaf(p(2))),
        });
        let bounds = CellRect {
            col: usize::MAX,
            row: 0,
            cols: 2,
            rows: 10,
        };
        assert_eq!(
            layout_to_rects(&layout, bounds, 1, 1),
            Err(GeometryError::Overflow)
        );

        let layout_v = PaneLayout::Split(Split {
            axis: Axis::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p(1))),
            second: Box::new(PaneLayout::leaf(p(2))),
        });
        let bounds_v = CellRect {
            col: 0,
            row: usize::MAX,
            cols: 10,
            rows: 2,
        };
        assert_eq!(
            layout_to_rects(&layout_v, bounds_v, 1, 1),
            Err(GeometryError::Overflow)
        );
    }
}
