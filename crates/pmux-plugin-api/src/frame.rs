// SPDX-License-Identifier: MIT

use std::fmt;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::host::pmux::view::frame::{Content, Viewport};

pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_VIEW_CELLS: usize = 60_000;
const MAX_COLUMNS: u16 = 500;
const MAX_ROWS: u16 = 200;
const MAX_CLUSTER_BYTES: usize = 128;
const MAX_TILES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Viewport,
    CellCount,
    CellText,
    CellWidth,
    Cursor,
    ImagesUnavailable,
    ImageBounds,
    ImageBytes,
    Budget,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Viewport => "invalid plug-in viewport",
            Self::CellCount => "plug-in frame has an invalid cell count",
            Self::CellText => "plug-in cell must contain one printable grapheme",
            Self::CellWidth => "plug-in frame has invalid wide-cell coverage",
            Self::Cursor => "plug-in cursor is outside the viewport",
            Self::ImagesUnavailable => "plug-in images were not negotiated",
            Self::ImageBounds => "plug-in image is outside the negotiated viewport",
            Self::ImageBytes => "plug-in image must contain exact RGBA bytes",
            Self::Budget => "plug-in frame exceeds its byte budget",
        })
    }
}

impl std::error::Error for FrameError {}

pub fn validate_viewport(viewport: &Viewport) -> Result<(), FrameError> {
    if viewport.columns == 0
        || viewport.rows == 0
        || viewport.columns > MAX_COLUMNS
        || viewport.rows > MAX_ROWS
        || usize::from(viewport.columns) * usize::from(viewport.rows) > MAX_VIEW_CELLS
    {
        return Err(FrameError::Viewport);
    }
    if let Some(pixels) = &viewport.pixels {
        if pixels.width == 0 || pixels.height == 0 || pixels.width > 256 || pixels.height > 256 {
            return Err(FrameError::Viewport);
        }
    }
    Ok(())
}

/// A whole frame checked before the trusted presenter can emit terminal bytes.
#[derive(Debug)]
pub struct ValidatedFrame {
    content: Content,
    byte_bound: usize,
}

impl ValidatedFrame {
    pub fn new(content: Content, viewport: &Viewport) -> Result<Self, FrameError> {
        validate_viewport(viewport)?;
        if content.columns != viewport.columns || content.rows != viewport.rows {
            return Err(FrameError::Viewport);
        }
        let columns = usize::from(content.columns);
        let count = columns * usize::from(content.rows);
        if content.cells.len() != count {
            return Err(FrameError::CellCount);
        }
        let mut bytes = count
            .checked_mul(32)
            .and_then(|size| size.checked_add(256))
            .ok_or(FrameError::Budget)?;
        for (index, cell) in content.cells.iter().enumerate() {
            bytes = bytes
                .checked_add(cell.text.len())
                .ok_or(FrameError::Budget)?;
            if bytes > MAX_FRAME_BYTES {
                return Err(FrameError::Budget);
            }
            if cell.width == 0 {
                if !cell.text.is_empty()
                    || index % columns == 0
                    || content.cells[index - 1].width != 2
                {
                    return Err(FrameError::CellWidth);
                }
                continue;
            }
            if cell.text.is_empty()
                || cell.text.len() > MAX_CLUSTER_BYTES
                || cell.text.chars().any(char::is_control)
                || cell.text.graphemes(true).count() != 1
            {
                return Err(FrameError::CellText);
            }
            if !matches!(cell.width, 1 | 2)
                || UnicodeWidthStr::width(cell.text.as_str()) != usize::from(cell.width)
            {
                return Err(FrameError::CellWidth);
            }
            if cell.width == 2
                && (index % columns + 1 == columns || content.cells[index + 1].width != 0)
            {
                return Err(FrameError::CellWidth);
            }
        }
        if content
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.column >= content.columns || cursor.row >= content.rows)
        {
            return Err(FrameError::Cursor);
        }
        if content.tiles.len() > MAX_TILES {
            return Err(FrameError::Budget);
        }
        for tile in &content.tiles {
            let pixels = viewport
                .pixels
                .as_ref()
                .ok_or(FrameError::ImagesUnavailable)?;
            if tile.columns == 0
                || tile.rows == 0
                || u32::from(tile.x) + u32::from(tile.columns) > u32::from(content.columns)
                || u32::from(tile.y) + u32::from(tile.rows) > u32::from(content.rows)
                || tile.pixel_width != u32::from(tile.columns) * u32::from(pixels.width)
                || tile.pixel_height != u32::from(tile.rows) * u32::from(pixels.height)
            {
                return Err(FrameError::ImageBounds);
            }
            let image_bytes = u64::from(tile.pixel_width)
                .checked_mul(u64::from(tile.pixel_height))
                .and_then(|size| size.checked_mul(4))
                .ok_or(FrameError::ImageBytes)?;
            if image_bytes != tile.rgba.len() as u64 {
                return Err(FrameError::ImageBytes);
            }
            bytes = bytes
                .checked_add(tile.rgba.len())
                .and_then(|size| size.checked_add(64))
                .ok_or(FrameError::Budget)?;
            if bytes > MAX_FRAME_BYTES {
                return Err(FrameError::Budget);
            }
        }
        Ok(Self {
            content,
            byte_bound: bytes,
        })
    }

    pub fn content(&self) -> &Content {
        &self.content
    }

    pub fn into_content(self) -> Content {
        self.content
    }

    pub fn byte_bound(&self) -> usize {
        self.byte_bound
    }

    pub fn text(&self) -> String {
        let mut text = String::new();
        for row in self.content.cells.chunks(usize::from(self.content.columns)) {
            for cell in row {
                text.push_str(&cell.text);
            }
            text.push('\n');
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::pmux::view::frame::{Attributes, Cell, Color, PixelSize, Tile, Underline};

    fn cell(text: &str, width: u8) -> Cell {
        Cell {
            text: text.into(),
            width,
            foreground: Color::Default,
            background: Color::Default,
            attributes: Attributes {
                bold: false,
                dim: false,
                italic: false,
                blink: false,
                reverse: false,
                hidden: false,
                strike: false,
                overline: false,
            },
            underline: Underline::None,
        }
    }

    fn viewport() -> Viewport {
        Viewport {
            columns: 3,
            rows: 1,
            pixels: None,
        }
    }

    fn content() -> Content {
        Content {
            columns: 3,
            rows: 1,
            cells: vec![cell("A", 1), cell("界", 2), cell("", 0)],
            tiles: vec![],
            cursor: None,
        }
    }

    #[test]
    fn a_complete_wide_frame_preserves_its_printable_text() {
        assert_eq!(
            ValidatedFrame::new(content(), &viewport()).unwrap().text(),
            "A界\n"
        );
    }

    #[test]
    fn controls_and_multiple_graphemes_never_become_terminal_output() {
        for text in ["\x1b]52;c;payload\x07", "\n", "\u{009b}", "ab"] {
            let mut raw = content();
            raw.cells[0].text = text.into();
            assert_eq!(
                ValidatedFrame::new(raw, &viewport()).unwrap_err(),
                FrameError::CellText
            );
        }
    }

    #[test]
    fn wide_glyphs_require_their_own_continuation_in_the_same_row() {
        let mut raw = content();
        raw.cells[2] = cell("B", 1);
        assert_eq!(
            ValidatedFrame::new(raw, &viewport()).unwrap_err(),
            FrameError::CellWidth
        );
        let mut raw = content();
        raw.cells[0] = cell("", 0);
        assert_eq!(
            ValidatedFrame::new(raw, &viewport()).unwrap_err(),
            FrameError::CellWidth
        );
    }

    #[test]
    fn images_require_negotiated_geometry_and_exact_bytes() {
        let mut raw = content();
        raw.tiles.push(Tile {
            x: 0,
            y: 0,
            columns: 1,
            rows: 1,
            pixel_width: 1,
            pixel_height: 1,
            rgba: vec![1, 2, 3, 4],
        });
        assert_eq!(
            ValidatedFrame::new(raw.clone(), &viewport()).unwrap_err(),
            FrameError::ImagesUnavailable
        );
        let mut viewport = viewport();
        viewport.pixels = Some(PixelSize {
            width: 1,
            height: 1,
        });
        assert_eq!(
            ValidatedFrame::new(raw.clone(), &viewport)
                .unwrap()
                .content()
                .tiles[0]
                .rgba,
            vec![1, 2, 3, 4]
        );
        raw.tiles[0].rgba.pop();
        assert_eq!(
            ValidatedFrame::new(raw, &viewport).unwrap_err(),
            FrameError::ImageBytes
        );
    }

    #[test]
    fn dimensions_and_image_integer_bounds_are_checked_before_presentation() {
        let mut raw = content();
        raw.cells.pop();
        assert_eq!(
            ValidatedFrame::new(raw, &viewport()).unwrap_err(),
            FrameError::CellCount
        );
        let mut raw = content();
        raw.tiles.push(Tile {
            x: u16::MAX,
            y: 0,
            columns: 1,
            rows: 1,
            pixel_width: u32::MAX,
            pixel_height: u32::MAX,
            rgba: vec![],
        });
        let mut viewport = viewport();
        viewport.pixels = Some(PixelSize {
            width: 1,
            height: 1,
        });
        assert_eq!(
            ValidatedFrame::new(raw, &viewport).unwrap_err(),
            FrameError::ImageBounds
        );
    }
}
