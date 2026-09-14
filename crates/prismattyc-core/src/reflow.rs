//! Repack primary logical lines; the alternate screen remains a cell grid.
use super::*;

#[derive(Clone, Copy, Default)]
struct Position {
    row: usize,
    column: usize,
    pending: bool,
}

impl Screen {
    /// Resize an interactive terminal. Soft-wrapped primary lines are joined
    /// before repacking. Explicit line breaks and alternate-screen rows remain
    /// distinct. Selection is invalidated through the ordinary resize epoch.
    pub fn resize_reflow(&mut self, columns: usize, rows: usize) {
        self.resize_impl(columns, rows, true);
    }

    pub(super) fn reflow_primary(&mut self, columns: usize, rows: usize) {
        let history_len = self.scrollback.len();
        let cursor = self.primary.cursor;
        let saved = self.primary.saved_cursor;
        let last_used = self
            .primary
            .cells
            .chunks(self.columns)
            .rposition(|row| row.iter().any(|c| *c != Cell::default()))
            .unwrap_or(0);
        let last_used = last_used.max(cursor.row).max(saved.cursor.row);
        let mut input: Vec<_> = self
            .scrollback
            .drain(..)
            .zip(self.scrollback_wrapped.drain(..))
            .collect();
        input.extend(
            self.primary
                .cells
                .chunks(self.columns)
                .take(last_used + 1)
                .enumerate()
                .map(|(i, row)| (row.to_vec(), self.primary.wrapped[i])),
        );
        let cursor_row = history_len + cursor.row;
        let saved_row = history_len + saved.cursor.row;
        let limit =
            history_budget::row_limit(columns, self.max_scrollback, self.max_scrollback_bytes)
                + rows;
        let mut output = Output {
            rows: VecDeque::new(),
            removed: 0,
            produced: 0,
            visible_end: None,
            viewport_rows: rows,
            limit,
        };
        let mut logical = Vec::new();
        let mut marks = [None, None];
        let mut positions = [Position::default(); 2];
        for (row_index, (row, wrapped)) in input.into_iter().enumerate() {
            let mut used = if wrapped {
                row.len()
            } else {
                row.iter()
                    .rposition(|c| *c != Cell::default())
                    .map_or(0, |i| i + 1)
            };
            for (i, (at, column, pending)) in [
                (cursor_row, cursor.column, self.primary.wrap_pending),
                (saved_row, saved.cursor.column, saved.wrap_pending),
            ]
            .into_iter()
            .enumerate()
            {
                if row_index == at {
                    let offset = column + usize::from(pending);
                    let padding = row.iter().take(offset).filter(|c| c.wrap_padding).count();
                    used = used.max(offset);
                    marks[i] = Some(logical.len() + offset - padding);
                }
            }
            logical.extend(
                row[..used.min(row.len())]
                    .iter()
                    .filter(|c| !c.wrap_padding)
                    .copied(),
            );
            if !wrapped {
                pack(&logical, columns, &mut output, marks, &mut positions);
                logical.clear();
                marks = [None, None];
            }
        }
        if !logical.is_empty() {
            pack(&logical, columns, &mut output, marks, &mut positions);
        }
        // Keep the insertion point visible when a cursor-addressed application
        // leaves content below it. Rows below the resized viewport are clipped.
        let visible_end = (positions[0].row + rows).saturating_sub(output.removed);
        output.rows.truncate(visible_end);
        let start = output.rows.len().saturating_sub(rows);
        let absolute_start = start + output.removed;
        let primary = GridBuffer {
            cells: output
                .rows
                .iter()
                .skip(start)
                .flat_map(|(row, _)| row.iter().copied())
                .chain(std::iter::repeat(Cell::default()))
                .take(columns * rows)
                .collect(),
            wrapped: output
                .rows
                .iter()
                .skip(start)
                .map(|(_, wrap)| *wrap)
                .chain(std::iter::repeat(false))
                .take(rows)
                .collect(),
            cursor: Cursor {
                row: positions[0]
                    .row
                    .saturating_sub(absolute_start)
                    .min(rows - 1),
                column: positions[0].column,
            },
            saved_cursor: SavedCursor {
                cursor: Cursor {
                    row: positions[1]
                        .row
                        .saturating_sub(absolute_start)
                        .min(rows - 1),
                    column: positions[1].column,
                },
                wrap_pending: positions[1].pending,
                style: saved.style,
            },
            style: self.primary.style,
            wrap_pending: positions[0].pending,
            scroll_top: 0,
            scroll_bottom: rows - 1,
        };
        self.primary = primary;
        self.scrollback = output
            .rows
            .drain(..start)
            .map(|(row, wrap)| {
                self.scrollback_wrapped.push_back(wrap);
                row
            })
            .collect();
    }
}

struct Output {
    rows: VecDeque<(Vec<Cell>, bool)>,
    removed: usize,
    produced: usize,
    visible_end: Option<usize>,
    viewport_rows: usize,
    limit: usize,
}
impl Output {
    fn len(&self) -> usize {
        self.produced
    }
    fn push(&mut self, row: (Vec<Cell>, bool)) {
        let index = self.produced;
        self.produced += 1;
        // Once the insertion point is known, content below its viewport
        // cannot evict it from a small (or disabled) history budget.
        if self.visible_end.is_some_and(|end| index >= end) {
            return;
        }
        if self.rows.len() == self.limit {
            self.rows.pop_front();
            self.removed += 1;
        }
        self.rows.push_back(row);
    }
}

fn pack(
    cells: &[Cell],
    columns: usize,
    output: &mut Output,
    marks: [Option<usize>; 2],
    positions: &mut [Position; 2],
) {
    let mut row = vec![Cell::default(); columns];
    let mut column = 0;
    let mut index = 0;
    loop {
        let width = cells.get(index).map_or(0, |cell| {
            let wide =
                cells.get(index + 1).is_some_and(|c| c.wide_cont) || cell.display_width() == 2;
            if wide {
                2.min(columns)
            } else {
                1
            }
        });
        if index < cells.len() && column + width > columns {
            for cell in row.iter_mut().skip(column) {
                cell.wrap_padding = true;
            }
            output.push((row, true));
            row = vec![Cell::default(); columns];
            column = 0;
        }
        for i in 0..2 {
            if marks[i] == Some(index) {
                if i == 0 {
                    output.visible_end = Some(output.len() + output.viewport_rows);
                }
                positions[i] = Position {
                    row: output.len(),
                    column: column.min(columns - 1),
                    pending: column == columns,
                };
            }
        }
        if index == cells.len() {
            break;
        }
        let cell = cells[index];
        if cell.wide_cont {
            index += 1;
            continue;
        }
        row[column] = cell;
        let has_cont = cells.get(index + 1).is_some_and(|c| c.wide_cont);
        if width == 2 {
            let mut continuation = Cell::wide_continuation(cell.style);
            continuation.hyperlink = cell.hyperlink;
            row[column + 1] = continuation;
        }
        if has_cont {
            for i in 0..2 {
                if marks[i] == Some(index + 1) {
                    if i == 0 {
                        output.visible_end = Some(output.len() + output.viewport_rows);
                    }
                    positions[i] = Position {
                        row: output.len(),
                        column: (column + 1).min(columns - 1),
                        pending: false,
                    };
                }
            }
        }
        column += width;
        index += if has_cont { 2 } else { 1 };
    }
    output.push((row, false));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(s: &mut Screen, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                s.carriage_return();
                s.line_feed();
            } else {
                s.put_char(c);
            }
        }
    }
    fn logical(s: &Screen) -> String {
        s.extract_text_abs(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: s.history_line_count() - 1,
            end_col: s.columns - 1,
        })
        .trim_end_matches('\n')
        .to_owned()
    }

    #[test]
    fn narrow_then_widen_preserves_paragraphs_and_explicit_breaks() {
        let mut s = Screen::new(12, 4, 100);
        write(&mut s, "abcdefghijklmnop\nsecond line");
        let before = logical(&s);
        s.resize_reflow(5, 4);
        assert_eq!(logical(&s), before);
        s.resize_reflow(20, 4);
        assert_eq!(logical(&s), before);
        write(&mut s, "!");
        assert!(logical(&s).ends_with("second line!"));
        assert_eq!(s, Screen::import_state(s.export_state()).unwrap());
    }
    #[test]
    fn exact_width_pending_cursor_continues_on_the_correct_line() {
        let mut s = Screen::new(8, 3, 100);
        write(&mut s, "abcdefgh");
        s.resize_reflow(4, 3);
        write(&mut s, "i");
        assert_eq!(logical(&s), "abcdefghi");
        s.resize_reflow(12, 3);
        write(&mut s, "j");
        assert_eq!(logical(&s), "abcdefghij");
    }
    #[test]
    fn wide_glyph_and_combining_cluster_survive_one_column() {
        let mut s = Screen::new(10, 4, 100);
        write(&mut s, "A中e\u{301}B");
        let before = logical(&s);
        s.resize_reflow(1, 5);
        assert_eq!(logical(&s), before);
        s.resize_reflow(10, 4);
        assert_eq!(logical(&s), before);
        assert_eq!(s, Screen::import_state(s.export_state()).unwrap());
    }
    #[test]
    fn alternate_grid_is_not_reflowed_and_primary_returns() {
        let mut s = Screen::new(8, 3, 100);
        write(&mut s, "abcdefghijk");
        s.enter_alt_screen(AltScreenMode::Mode1049);
        write(&mut s, "12345678");
        s.resize_reflow(4, 3);
        assert_eq!(s.history_line_text(s.scrollback.len()), "1234");
        s.leave_alt_screen(AltScreenMode::Mode1049);
        assert_eq!(logical(&s), "abcdefghijk");
    }
    #[test]
    fn reflow_keeps_history_within_its_byte_and_row_limits() {
        let mut s = Screen::new(40, 4, 8);
        for _ in 0..20 {
            write(&mut s, "abcdefghijklmnopqrstuvwxyz\n");
        }
        s.resize_reflow(2, 4);
        assert!(s.scrollback.len() <= 8);
        assert!(s.scrollback_bytes() <= s.scrollback_byte_budget());
        assert_eq!(s, Screen::import_state(s.export_state()).unwrap());
    }
    #[test]
    fn wide_wrap_padding_is_not_copied_but_authored_spaces_are() {
        let mut s = Screen::new(12, 5, 100);
        write(&mut s, "abc中d  ef中gh");
        let before = logical(&s);
        for width in [4, 3, 2, 1, 7, 12] {
            s.resize_reflow(width, 8);
            assert_eq!(logical(&s), before, "width {width}");
            s = Screen::import_state(s.export_state()).unwrap();
            assert_eq!(logical(&s), before);
        }
        let mut s = Screen::new(4, 4, 100);
        write(&mut s, "abc中d");
        assert_eq!(logical(&s), "abc中d");
        s.resize_reflow(12, 4);
        assert_eq!(logical(&s), "abc中d");
    }
    #[test]
    fn disabled_history_cannot_evict_an_addressed_cursor() {
        let mut s = Screen::new(12, 4, 0);
        write(&mut s, "first\nabcdefghijkl\nthird");
        s.set_cursor_position(0, 2);
        s.resize_reflow(1, 4);
        let row = s.cursor().row;
        assert_eq!(s.history_line_text(row), "r");
        write(&mut s, "!");
        assert_eq!(s.history_line_text(row), "!");
        assert!(s.scrollback.is_empty());
    }
    #[test]
    fn addressed_cursor_stays_at_its_text_when_lines_below_expand() {
        let mut s = Screen::new(12, 4, 100);
        write(&mut s, "first\nabcdefghijkl\nthird");
        s.set_cursor_position(0, 2);
        s.resize_reflow(6, 4);
        write(&mut s, "!");
        assert!(logical(&s).starts_with("fi!st"), "{}", logical(&s));
    }
}
