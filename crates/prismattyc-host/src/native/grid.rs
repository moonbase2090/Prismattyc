// SPDX-License-Identifier: MIT
//! Read the installed immutable frame without changing widths, styles or text.

use super::frame::{Cell, FrameSnapshot, Glyph};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Text<'a> {
    Scalar(char),
    Cluster(&'a str),
    Continuation,
}

impl Text<'_> {
    pub fn append_to(self, output: &mut String) {
        match self {
            Self::Scalar(character) => output.push(character),
            Self::Cluster(text) => output.push_str(text),
            Self::Continuation => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellView<'a> {
    pub text: Text<'a>,
    pub cell: &'a Cell,
}

/// A view borrows the same snapshot that owns the installed image placements.
#[derive(Clone, Copy)]
pub struct NativeGrid<'a> {
    frame: &'a FrameSnapshot,
}

impl<'a> NativeGrid<'a> {
    pub fn new(frame: &'a FrameSnapshot) -> Self {
        Self { frame }
    }
    pub fn columns(self) -> usize {
        self.frame.context.columns as usize
    }
    pub fn rows(self) -> usize {
        self.frame.context.rows as usize
    }
    pub fn cursor(self) -> Option<[usize; 2]> {
        self.frame
            .cursor
            .map(|[column, row]| [column as usize, row as usize])
    }
    pub fn cell(self, column: usize, row: usize) -> Option<CellView<'a>> {
        if column >= self.columns() || row >= self.rows() {
            return None;
        }
        let cell = &self.frame.cells[row * self.columns() + column];
        let text = match cell.glyph {
            Glyph::Scalar(character) => Text::Scalar(character),
            Glyph::Continuation => Text::Continuation,
            Glyph::Cluster(index) => Text::Cluster(&self.frame.clusters[index as usize]),
        };
        Some(CellView { text, cell })
    }

    /// Native sessions have no scrollback. Coordinates are inclusive grid cells.
    /// Continuations contribute no duplicate text. Each selected row trims spaces.
    pub fn extract_text(self, start: [usize; 2], end: [usize; 2]) -> String {
        let (start, end) = if (start[1], start[0]) <= (end[1], end[0]) {
            (start, end)
        } else {
            (end, start)
        };
        if start[1] >= self.rows() {
            return String::new();
        }
        let end_row = end[1].min(self.rows() - 1);
        let mut output = String::new();
        for row in start[1]..=end_row {
            let first = if row == start[1] { start[0] } else { 0 };
            let last = if row == end[1] {
                end[0].min(self.columns() - 1)
            } else {
                self.columns() - 1
            };
            let line_start = output.len();
            for column in first..=last {
                self.cell(column, row).unwrap().text.append_to(&mut output);
            }
            while output.len() > line_start && output.ends_with(' ') {
                output.pop();
            }
            if row != end_row {
                output.push('\n');
            }
        }
        output
    }
}

#[cfg(test)]
#[path = "grid_tests.rs"]
mod tests;
