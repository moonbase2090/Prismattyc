//! Per-frame grid damage for the host paint path (PT-242).
//!
//! Cell dirty and row dirty track writes. Scroll events are recorded
//! separately so a consumer can blit without walking the grid.

/// One region-scroll recorded this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollDamage {
    /// Inclusive scroll-region top row.
    pub top: usize,
    /// Inclusive scroll-region bottom row.
    pub bottom: usize,
    /// Content moved toward the top when positive (LF at the bottom margin).
    pub delta: i32,
}

/// Viewport damage accumulated since the last [`GridDamage::take`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridDamage {
    rows: usize,
    columns: usize,
    dirty_rows: Vec<u64>,
    dirty_cells: Vec<u64>,
    scroll: Vec<ScrollDamage>,
    scroll_overflowed: bool,
    retired_rows: usize,
}

// Keep exact small-batch scrolls. Larger batches repaint the viewport.
const MAX_SCROLL_EVENTS: usize = 256;

fn words_for(bits: usize) -> usize {
    bits.div_ceil(64)
}

fn set_bit(bits: &mut [u64], index: usize) {
    let word = index / 64;
    let bit = index % 64;
    if word < bits.len() {
        bits[word] |= 1 << bit;
    }
}

fn clear_bit(bits: &mut [u64], index: usize) {
    let word = index / 64;
    let bit = index % 64;
    if word < bits.len() {
        bits[word] &= !(1 << bit);
    }
}

fn test_bit(bits: &[u64], index: usize) -> bool {
    let word = index / 64;
    let bit = index % 64;
    word < bits.len() && (bits[word] & (1 << bit)) != 0
}

fn count_bits(bits: &[u64]) -> usize {
    bits.iter().map(|w| w.count_ones() as usize).sum()
}

impl GridDamage {
    /// Empty damage for a viewport of `rows` by `columns`.
    pub fn empty(rows: usize, columns: usize) -> Self {
        let rows = rows.max(1);
        let columns = columns.max(1);
        Self {
            dirty_rows: vec![0; words_for(rows)],
            dirty_cells: vec![0; words_for(rows.saturating_mul(columns))],
            rows,
            columns,
            scroll: Vec::new(),
            scroll_overflowed: false,
            retired_rows: 0,
        }
    }

    /// Every row and cell marked dirty (first frame, resize, import).
    pub fn full(rows: usize, columns: usize) -> Self {
        let mut damage = Self::empty(rows, columns);
        damage.mark_all();
        damage
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn mark_cell(&mut self, row: usize, column: usize) {
        if row >= self.rows || column >= self.columns {
            return;
        }
        self.mark_row(row);
        set_bit(
            &mut self.dirty_cells,
            row.saturating_mul(self.columns).saturating_add(column),
        );
    }

    pub fn mark_row(&mut self, row: usize) {
        if row < self.rows {
            set_bit(&mut self.dirty_rows, row);
        }
    }

    /// Mark every cell on `row` (new blank line after a scroll).
    pub fn mark_row_cells(&mut self, row: usize) {
        if row >= self.rows {
            return;
        }
        for col in 0..self.columns {
            self.mark_cell(row, col);
        }
    }

    pub fn mark_all(&mut self) {
        for row in 0..self.rows {
            set_bit(&mut self.dirty_rows, row);
        }
        for cell in 0..self.rows.saturating_mul(self.columns) {
            set_bit(&mut self.dirty_cells, cell);
        }
    }

    /// Record a region scroll. Dirty bits move with the cells so they stay in
    /// post-scroll viewport coordinates (write then IL/SD still names the
    /// glyph's new cell). Callers mark the newly blank rows after this.
    pub fn push_scroll(&mut self, event: ScrollDamage) {
        self.retired_rows = self
            .retired_rows
            .saturating_add(event.delta.unsigned_abs() as usize);
        if self.scroll_overflowed {
            return;
        }
        if self.scroll.len() == MAX_SCROLL_EVENTS {
            self.mark_all();
            self.scroll = Vec::new();
            self.scroll_overflowed = true;
            return;
        }
        self.shift_dirty_with_scroll(event);
        self.scroll.push(event);
    }

    fn shift_dirty_with_scroll(&mut self, event: ScrollDamage) {
        if event.delta == 0 || event.bottom < event.top {
            return;
        }
        let last = self.rows.saturating_sub(1);
        let top = event.top.min(last);
        let bottom = event.bottom.min(last);
        if bottom < top {
            return;
        }
        let height = bottom - top + 1;
        let n = (event.delta.unsigned_abs() as usize).min(height);
        if n == 0 || n >= height {
            return;
        }
        // Shifting all-ones (or all-zero) bits is a no-op. A PTY flood of LF
        // fills the grid dirty within one region-height, then this path
        // keeps process_rich_chunk under its stall budget.
        let cells = self.rows.saturating_mul(self.columns);
        let dirty = self.dirty_cell_count();
        if dirty == 0 || dirty == cells {
            return;
        }
        if event.delta > 0 {
            for row in top..=bottom - n {
                self.copy_row_dirty(row + n, row);
            }
        } else {
            for row in (top + n..=bottom).rev() {
                self.copy_row_dirty(row - n, row);
            }
        }
    }

    fn copy_row_dirty(&mut self, src_row: usize, dst_row: usize) {
        if test_bit(&self.dirty_rows, src_row) {
            set_bit(&mut self.dirty_rows, dst_row);
        } else {
            clear_bit(&mut self.dirty_rows, dst_row);
        }
        let cols = self.columns;
        for col in 0..cols {
            let src = src_row.saturating_mul(cols).saturating_add(col);
            let dst = dst_row.saturating_mul(cols).saturating_add(col);
            if test_bit(&self.dirty_cells, src) {
                set_bit(&mut self.dirty_cells, dst);
            } else {
                clear_bit(&mut self.dirty_cells, dst);
            }
        }
    }

    pub fn is_row_dirty(&self, row: usize) -> bool {
        row < self.rows && test_bit(&self.dirty_rows, row)
    }

    pub fn is_cell_dirty(&self, row: usize, column: usize) -> bool {
        if row >= self.rows || column >= self.columns {
            return false;
        }
        test_bit(
            &self.dirty_cells,
            row.saturating_mul(self.columns).saturating_add(column),
        )
    }

    pub fn dirty_row_count(&self) -> usize {
        count_bits(&self.dirty_rows)
    }

    pub fn dirty_cell_count(&self) -> usize {
        count_bits(&self.dirty_cells)
    }

    /// Dirty row indices in order. PT-243 can walk this instead of the grid.
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.rows).filter(move |row| self.is_row_dirty(*row))
    }

    /// Exact scroll optimizations for small batches. An oversized batch has
    /// no scroll records and marks the whole viewport dirty until `take`.
    pub fn scroll_events(&self) -> &[ScrollDamage] {
        &self.scroll
    }

    /// True after the 257th scroll event in a frame. The exact scroll list is
    /// empty and all viewport rows and cells are dirty. `take` and `resize`
    /// reset the accumulator; a value returned by `take` keeps this marker.
    pub const fn scroll_overflowed(&self) -> bool {
        self.scroll_overflowed
    }

    pub(crate) const fn retired_rows(&self) -> usize {
        self.retired_rows
    }

    /// Swap out this frame's damage and leave an empty accumulator.
    pub fn take(&mut self) -> Self {
        let empty = Self::empty(self.rows, self.columns);
        std::mem::replace(self, empty)
    }

    pub fn resize(&mut self, rows: usize, columns: usize) {
        *self = Self::full(rows, columns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_clears_accumulator() {
        let mut damage = GridDamage::empty(2, 2);
        damage.mark_cell(0, 1);
        let taken = damage.take();
        assert_eq!(taken.dirty_cell_count(), 1);
        assert_eq!(damage.dirty_cell_count(), 0);
        assert_eq!(damage.dirty_row_count(), 0);
    }

    #[test]
    fn push_scroll_down_moves_dirty_cell_with_content() {
        let mut damage = GridDamage::empty(4, 4);
        damage.mark_cell(0, 1);
        damage.push_scroll(ScrollDamage {
            top: 0,
            bottom: 3,
            delta: -1,
        });
        assert!(
            damage.is_cell_dirty(1, 1),
            "write-then-SD dirty bit follows the glyph"
        );
        assert!(damage.scroll_events().len() == 1);
    }

    #[test]
    fn push_scroll_up_moves_dirty_cell_with_content() {
        let mut damage = GridDamage::empty(4, 4);
        damage.mark_cell(2, 0);
        damage.push_scroll(ScrollDamage {
            top: 0,
            bottom: 3,
            delta: 1,
        });
        assert!(damage.is_cell_dirty(1, 0));
        assert!(!damage.is_cell_dirty(2, 0));
    }
    #[test]
    fn overflow_bounds_allocations_and_full_damage_survives_later_scrolls() {
        let mut damage = GridDamage::empty(4, 8);
        let event = ScrollDamage {
            top: 0,
            bottom: 3,
            delta: 1,
        };
        for _ in 0..MAX_SCROLL_EVENTS {
            damage.push_scroll(event);
        }
        assert_eq!(damage.scroll_events().len(), MAX_SCROLL_EVENTS);
        assert!(damage.scroll.capacity() <= MAX_SCROLL_EVENTS);
        assert_eq!(damage.dirty_cell_count(), 0);
        damage.push_scroll(event);
        assert!(damage.scroll_events().is_empty());
        assert_eq!(damage.scroll.capacity(), 0);
        for _ in 0..50_000 {
            damage.push_scroll(ScrollDamage {
                top: 1,
                bottom: 2,
                delta: -1,
            });
        }
        assert_eq!(damage.scroll.capacity(), 0);
        assert_eq!(damage.dirty_cell_count(), 32);
        assert_eq!(damage.dirty_row_count(), 4);
        assert_eq!(damage.retired_rows(), MAX_SCROLL_EVENTS + 1 + 50_000);
        let taken = damage.take();
        assert_eq!(taken.dirty_cell_count(), 32);
        assert!(taken.scroll_overflowed());
        assert!(!damage.scroll_overflowed());
        assert!(taken.scroll_events().is_empty());
        assert_eq!(damage.retired_rows(), 0);
        assert_eq!(damage.dirty_cell_count(), 0);
        damage.push_scroll(event);
        assert_eq!(damage.scroll_events(), &[event]);
        assert_eq!(damage.retired_rows(), 1);
    }

    #[test]
    fn retirement_saturates_and_resize_resets_overflow() {
        let mut damage = GridDamage::empty(2, 2);
        damage.retired_rows = usize::MAX - 1;
        damage.push_scroll(ScrollDamage {
            top: 0,
            bottom: 1,
            delta: i32::MIN,
        });
        assert_eq!(damage.retired_rows(), usize::MAX);
        for _ in 0..MAX_SCROLL_EVENTS {
            damage.push_scroll(ScrollDamage {
                top: 0,
                bottom: 1,
                delta: 1,
            });
        }
        assert!(damage.scroll_overflowed());
        damage.resize(3, 4);
        assert!(!damage.scroll_overflowed());
        assert_eq!(damage.retired_rows(), 0);
        assert_eq!(damage.dirty_cell_count(), 12);
        damage.push_scroll(ScrollDamage {
            top: 0,
            bottom: 2,
            delta: -1,
        });
        assert_eq!(damage.scroll_events().len(), 1);
        assert_eq!(damage.retired_rows(), 1);
    }
}
