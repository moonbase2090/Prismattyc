//! Keep rows contiguous while moving row identities instead of screenfuls of cells.
use super::Cell;
use std::ops::{Index, IndexMut, Range};

#[derive(Debug, Clone)]
pub(super) struct CellGrid {
    cells: Vec<Cell>,
    rows: Vec<usize>,
    columns: usize,
}

impl PartialEq for CellGrid {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.len() == other.len()
            && self.chunks(self.columns).eq(other.chunks(other.columns))
    }
}
impl Eq for CellGrid {}

impl CellGrid {
    pub(super) fn columns(&self) -> usize {
        self.columns
    }

    pub(super) fn from_flat(cells: Vec<Cell>, columns: usize) -> Self {
        assert!(columns > 0 && cells.len().is_multiple_of(columns));
        Self {
            rows: (0..cells.len() / columns).collect(),
            cells,
            columns,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.cells.len()
    }

    fn physical(&self, index: usize) -> usize {
        self.rows[index / self.columns] * self.columns + index % self.columns
    }

    pub(super) fn row(&self, row: usize) -> &[Cell] {
        let start = self.rows[row] * self.columns;
        &self.cells[start..start + self.columns]
    }

    pub(super) fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        let start = self.rows[row] * self.columns;
        &mut self.cells[start..start + self.columns]
    }

    pub(super) fn get(&self, range: Range<usize>) -> Option<&[Cell]> {
        if range.end > self.len() || range.start > range.end {
            return None;
        }
        if range.is_empty() {
            return Some(&self.cells[..0]);
        }
        let start = self.physical(range.start);
        let len = range.end - range.start;
        (len <= self.columns - range.start % self.columns).then(|| &self.cells[start..start + len])
    }

    pub(super) fn chunks(
        &self,
        columns: usize,
    ) -> impl DoubleEndedIterator<Item = &[Cell]> + ExactSizeIterator {
        assert_eq!(columns, self.columns);
        self.rows
            .iter()
            .map(move |&row| &self.cells[row * columns..(row + 1) * columns])
    }

    // Cluster compaction visits every cell; its traversal order is immaterial.
    pub(super) fn iter_mut(&mut self) -> std::slice::IterMut<'_, Cell> {
        self.cells.iter_mut()
    }

    pub(super) fn fill(&mut self, cell: Cell) {
        self.cells.fill(cell);
    }

    pub(super) fn fill_range(&mut self, range: Range<usize>, cell: Cell) {
        let mut start = range.start;
        while start < range.end {
            let count = (self.columns - start % self.columns).min(range.end - start);
            let physical = self.physical(start);
            self.cells[physical..physical + count].fill(cell);
            start += count;
        }
    }

    pub(super) fn scroll_up(&mut self, top: usize, bottom: usize, count: usize) {
        let rows = &mut self.rows[top..=bottom];
        rows.rotate_left(count.min(rows.len()));
    }

    pub(super) fn scroll_down(&mut self, top: usize, bottom: usize, count: usize) {
        let rows = &mut self.rows[top..=bottom];
        rows.rotate_right(count.min(rows.len()));
    }

    pub(super) fn copy_within(&mut self, source: Range<usize>, dest: usize) {
        assert!(source.start <= source.end && source.end <= self.len());
        let count = source.end - source.start;
        assert!(dest <= self.len() - count);
        if count == 0 {
            return;
        }
        if source.start / self.columns == (source.end - 1) / self.columns
            && dest / self.columns == (dest + count - 1) / self.columns
        {
            let src = self.physical(source.start);
            let dst = self.physical(dest);
            self.cells.copy_within(src..src + count, dst);
        } else if dest < source.start {
            for offset in 0..count {
                self[dest + offset] = self[source.start + offset];
            }
        } else {
            for offset in (0..count).rev() {
                self[dest + offset] = self[source.start + offset];
            }
        }
    }
}

impl Index<usize> for CellGrid {
    type Output = Cell;
    fn index(&self, index: usize) -> &Cell {
        &self.cells[self.physical(index)]
    }
}

impl IndexMut<usize> for CellGrid {
    fn index_mut(&mut self, index: usize) -> &mut Cell {
        let physical = self.physical(index);
        &mut self.cells[physical]
    }
}

impl Index<Range<usize>> for CellGrid {
    type Output = [Cell];
    fn index(&self, range: Range<usize>) -> &[Cell] {
        self.get(range).expect("contiguous row range")
    }
}

impl IndexMut<Range<usize>> for CellGrid {
    fn index_mut(&mut self, range: Range<usize>) -> &mut [Cell] {
        assert!(range.start <= range.end && range.end <= self.len());
        if range.is_empty() {
            return &mut self.cells[..0];
        }
        let len = range.end - range.start;
        assert!(len <= self.columns - range.start % self.columns);
        let start = self.physical(range.start);
        &mut self.cells[start..start + len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordered_rows_match_flat_grid_after_mixed_mutations() {
        for columns in [1, 2, 7, 80] {
            let count = columns * 9;
            let mut flat: Vec<Cell> = (0..count)
                .map(|i| Cell::glyph(char::from_u32(33 + i as u32).unwrap(), Default::default()))
                .collect();
            let mut grid = CellGrid::from_flat(flat.clone(), columns);
            let mut seed = 41usize;
            for turn in 0..500 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let top = seed % 9;
                let bottom = top + (seed / 9) % (9 - top);
                let lines = (seed / 81) % (bottom - top + 2);
                match turn % 4 {
                    0 => {
                        grid.scroll_up(top, bottom, lines);
                        flat[top * columns..(bottom + 1) * columns]
                            .rotate_left(lines.min(bottom - top + 1) * columns);
                    }
                    1 => {
                        grid.scroll_down(top, bottom, lines);
                        flat[top * columns..(bottom + 1) * columns]
                            .rotate_right(lines.min(bottom - top + 1) * columns);
                    }
                    2 => {
                        let start = seed % count;
                        let len = (seed / count) % (count - start + 1);
                        let dest = (seed / 7) % (count - len + 1);
                        grid.copy_within(start..start + len, dest);
                        flat.copy_within(start..start + len, dest);
                    }
                    _ => {
                        let start = seed % count;
                        let end = start + seed % (count - start + 1);
                        grid.fill_range(start..end, Cell::default());
                        flat[start..end].fill(Cell::default());
                    }
                }
                let logical: Vec<Cell> = grid.chunks(columns).flatten().copied().collect();
                assert_eq!(logical, flat, "columns={columns}, turn={turn}");
                assert_eq!(grid, CellGrid::from_flat(flat.clone(), columns));
            }
        }
    }
}
