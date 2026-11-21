// SPDX-License-Identifier: MIT
//! Read one displayed source without changing the retained ANSI screen.

use prismattyc_core::{CellRange, Cursor, HistoryMatch, Screen};

#[derive(Clone, Copy)]
pub(crate) enum Content<'a> {
    Classic {
        screen: &'a Screen,
        scroll: usize,
    },
    #[cfg(native_transport)]
    Native(&'a crate::native::scene::NativeScene),
}

#[derive(Clone, Copy)]
pub(crate) enum Cell<'a> {
    Classic(prismattyc_core::CellView<'a>),
    #[cfg(native_transport)]
    Native(crate::native::grid::CellView<'a>),
}

impl Cell<'_> {
    #[cfg(native_transport)]
    fn first_character(self) -> Option<char> {
        match self {
            Self::Classic(cell) => Some(cell.character),
            #[cfg(native_transport)]
            Self::Native(cell) => match cell.text {
                crate::native::grid::Text::Scalar(character) => Some(character),
                crate::native::grid::Text::Cluster(text) => text.chars().next(),
                crate::native::grid::Text::Continuation => None,
            },
        }
    }

    pub(crate) fn append_to(self, output: &mut String) {
        match self {
            Self::Classic(cell) => cell.write_grapheme_into(output),
            #[cfg(native_transport)]
            Self::Native(cell) => cell.text.append_to(output),
        }
    }

    pub(crate) fn width(self) -> usize {
        match self {
            Self::Classic(cell) => cell.display_width(),
            #[cfg(native_transport)]
            Self::Native(cell) => usize::from(cell.cell.width()),
        }
    }
}

impl<'a> Content<'a> {
    pub(crate) fn select(
        screen: &'a Screen,
        scroll: usize,
        #[cfg(native_transport)] native: Option<&'a crate::native::scene::NativeScene>,
    ) -> Self {
        #[cfg(native_transport)]
        if let Some(scene) = native {
            return Self::Native(scene);
        }
        Self::Classic {
            screen,
            scroll: scroll.min(screen.max_view_scroll()),
        }
    }

    pub(crate) fn is_native(self) -> bool {
        match self {
            Self::Classic { .. } => false,
            #[cfg(native_transport)]
            Self::Native(_) => true,
        }
    }

    pub(crate) fn columns(self) -> usize {
        match self {
            Self::Classic { screen, .. } => screen.columns(),
            #[cfg(native_transport)]
            Self::Native(scene) => scene.grid().columns(),
        }
    }

    pub(crate) fn rows(self) -> usize {
        match self {
            Self::Classic { screen, .. } => screen.rows(),
            #[cfg(native_transport)]
            Self::Native(scene) => scene.grid().rows(),
        }
    }

    pub(crate) fn scroll(self) -> usize {
        match self {
            Self::Classic { scroll, .. } => scroll,
            #[cfg(native_transport)]
            Self::Native(_) => 0,
        }
    }

    pub(crate) fn max_scroll(self) -> usize {
        match self {
            Self::Classic { screen, .. } => screen.max_view_scroll(),
            #[cfg(native_transport)]
            Self::Native(_) => 0,
        }
    }

    pub(crate) fn cursor(self) -> Option<Cursor> {
        match self {
            Self::Classic { screen, .. } => Some(screen.cursor()),
            #[cfg(native_transport)]
            Self::Native(scene) => scene
                .grid()
                .cursor()
                .map(|[column, row]| Cursor { row, column }),
        }
    }

    pub(crate) fn absolute_cursor(self) -> Cursor {
        let caret = self.cursor().unwrap_or(Cursor { row: 0, column: 0 });
        match self {
            Self::Classic { screen, .. } => Cursor {
                row: screen.abs_row_at_view(0, caret.row),
                column: caret.column,
            },
            #[cfg(native_transport)]
            Self::Native(_) => caret,
        }
    }

    pub(crate) fn viewport_range(self) -> Option<CellRange> {
        Some(CellRange {
            start_row: self.abs_row_at_view(0),
            start_col: 0,
            end_row: self.abs_row_at_view(self.rows().checked_sub(1)?),
            end_col: self.columns().checked_sub(1)?,
        })
    }

    /// Column bounds for double-click selection, including a wide cell's tail.
    pub(crate) fn word_columns(self, row: usize, column: usize) -> Option<(usize, usize)> {
        match self {
            Self::Classic { screen, scroll } => screen
                .word_range_at_view(scroll, row, column)
                .map(|word| (word.start_col, word.end_col)),
            #[cfg(native_transport)]
            Self::Native(_) => {
                if row >= self.rows() || self.columns() == 0 {
                    return None;
                }
                let class = |character: char| {
                    if character.is_whitespace() {
                        0
                    } else if character.is_ascii_alphanumeric() || character == '_' {
                        1
                    } else {
                        2
                    }
                };
                let lead_at = |mut column| {
                    while column > 0 && self.cell(row, column).unwrap().width() == 0 {
                        column -= 1;
                    }
                    column
                };
                let mut start = lead_at(column.min(self.columns() - 1));
                let target = class(self.cell(row, start)?.first_character()?);
                let mut end = start + self.cell(row, start)?.width() - 1;
                while start > 0 {
                    let previous = lead_at(start - 1);
                    if self.cell(row, previous)?.first_character().map(class) != Some(target) {
                        break;
                    }
                    start = previous;
                }
                while end + 1 < self.columns() {
                    let next = self.cell(row, end + 1)?;
                    if next.first_character().map(class) != Some(target) {
                        break;
                    }
                    end += next.width();
                }
                Some((start, end))
            }
        }
    }

    pub(crate) fn cell(self, row: usize, column: usize) -> Option<Cell<'a>> {
        if row >= self.rows() || column >= self.columns() {
            return None;
        }
        match self {
            Self::Classic { screen, scroll } => {
                Some(Cell::Classic(screen.view_cell(scroll, row, column)))
            }
            #[cfg(native_transport)]
            Self::Native(scene) => scene.grid().cell(column, row).map(Cell::Native),
        }
    }

    pub(crate) fn row_text(self, row: usize) -> String {
        #[cfg(native_transport)]
        if let Self::Native(scene) = self {
            return scene.grid().row_text(row);
        }
        if row >= self.rows() {
            return String::new();
        }
        let mut output = String::new();
        for column in 0..self.columns() {
            self.cell(row, column).unwrap().append_to(&mut output);
        }
        while output.ends_with(' ') {
            output.pop();
        }
        output
    }

    pub(crate) fn abs_row_at_view(self, row: usize) -> usize {
        match self {
            Self::Classic { screen, scroll } => screen.abs_row_at_view(scroll, row),
            #[cfg(native_transport)]
            Self::Native(_) => row,
        }
    }

    pub(crate) fn extract_text(self, range: CellRange) -> String {
        match self {
            Self::Classic { screen, .. } => screen.extract_text_abs(range),
            #[cfg(native_transport)]
            Self::Native(scene) => scene.grid().extract_text(
                [range.start_col, range.start_row],
                [range.end_col, range.end_row],
            ),
        }
    }

    pub(crate) fn find(
        self,
        query: &str,
        last: Option<HistoryMatch>,
        reverse: bool,
    ) -> Option<(HistoryMatch, (usize, usize), usize)> {
        match self {
            Self::Classic { screen, .. } => {
                let found = if reverse {
                    screen.find_in_history_rev(
                        query,
                        last.map(|hit| (hit.abs_row, hit.start_col)),
                        false,
                    )
                } else {
                    screen.find_in_history(query, last.map(|hit| (hit.abs_row, hit.end_col)))
                }?;
                let rank = screen
                    .history_match_rank(query, found, false)
                    .expect("match belongs to this immutable screen");
                Some((
                    found,
                    rank,
                    screen.view_scroll_for_history_row(found.abs_row),
                ))
            }
            #[cfg(native_transport)]
            Self::Native(scene) => {
                let matches = scene.grid().matches(query, false);
                let index = if reverse {
                    matches
                        .iter()
                        .rposition(|hit| {
                            last.is_none_or(|last| {
                                (hit.row, hit.start_column) < (last.abs_row, last.start_col)
                            })
                        })
                        .or_else(|| matches.len().checked_sub(1))?
                } else {
                    matches
                        .iter()
                        .position(|hit| {
                            last.is_none_or(|last| {
                                (hit.row, hit.start_column) > (last.abs_row, last.end_col)
                            })
                        })
                        .or(if matches.is_empty() { None } else { Some(0) })?
                };
                let hit = matches[index];
                Some((
                    HistoryMatch {
                        abs_row: hit.row,
                        start_col: hit.start_column,
                        end_col: hit.end_column,
                    },
                    (index + 1, matches.len()),
                    0,
                ))
            }
        }
    }
}

#[cfg(test)]
#[path = "content_tests.rs"]
mod tests;
