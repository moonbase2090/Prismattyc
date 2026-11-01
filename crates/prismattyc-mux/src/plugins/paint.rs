// SPDX-License-Identifier: MIT
//! Trusted conversion of fully validated frames to terminal encoder input.

use lunatui_core::{diff_runs, Attrs, ClusterTable, Color, Grid, Rect, Style, UnderlineStyle};
use lunatui_image::{ImageSink, ImageSource};
use lunatui_term::{ColorMode, Encoder, PixelLayer};
use pmux_plugin_api::host::pmux::view::frame;
use pmux_plugin_api::ValidatedFrame;

pub(super) struct Output {
    pub upload: Vec<u8>,
    pub paint: Vec<u8>,
}

pub(super) struct Painter {
    images: PixelLayer,
    encoder: Encoder,
    previous: Option<ValidatedFrame>,
}

impl Painter {
    pub(super) fn new(images: PixelLayer) -> Self {
        Self {
            images,
            encoder: Encoder::new(),
            previous: None,
        }
    }

    /// None asks the frontend to negotiate a fresh cell-only frame.
    pub(super) fn encode(&mut self, frame: ValidatedFrame) -> Option<Output> {
        let content = frame.content();
        let resized = self.previous.as_ref().is_none_or(|previous| {
            previous.content().columns != content.columns || previous.content().rows != content.rows
        });
        // Both grids share one fresh table. Cluster IDs remain comparable while
        // guest-chosen graphemes cannot accumulate across an unbounded session.
        let mut clusters = ClusterTable::new();
        let mut previous = self
            .previous
            .as_ref()
            .filter(|_| !resized)
            .map(|previous| grid(previous, &mut clusters))
            .unwrap_or_else(|| Grid::new(content.columns, content.rows));
        let next = grid(&frame, &mut clusters);
        self.images.begin_frame();
        for tile in &content.tiles {
            let source = ImageSource::rgba8(
                tile.pixel_width,
                tile.pixel_height,
                tile.rgba.clone().into_boxed_slice(),
            )
            .ok()?;
            if !self.images.submit_raster(
                &source,
                Rect::new(tile.x, tile.y, tile.columns, tile.rows),
                0,
            ) {
                return None;
            }
        }
        if self.images.needs_cell_repaint() {
            previous = Grid::new(content.columns, content.rows);
        }
        let changed = !diff_runs(&previous, &next).is_empty();
        let cells = self
            .encoder
            .encode_diff(&previous, &next, &clusters, ColorMode::TrueColor);
        let (upload, placements) = self.images.finish_frame_parts();
        let upload = upload.to_vec();
        let cursor_changed = self
            .previous
            .as_ref()
            .is_none_or(|previous| previous.content().cursor != content.cursor);
        let mut paint = Vec::new();
        if changed || resized || !placements.is_empty() || cursor_changed {
            if resized {
                paint.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            }
            paint.extend_from_slice(cells);
            paint.extend_from_slice(placements);
            match &content.cursor {
                Some(cursor) => paint.extend_from_slice(
                    format!(
                        "\x1b[{};{}H\x1b[?25h",
                        u32::from(cursor.row) + 1,
                        u32::from(cursor.column) + 1
                    )
                    .as_bytes(),
                ),
                None => paint.extend_from_slice(b"\x1b[?25l"),
            }
        }
        self.previous = Some(frame);
        Some(Output { upload, paint })
    }

    pub(super) fn clear(&mut self) -> Vec<u8> {
        self.previous = None;
        self.images.clear().to_vec()
    }
}

fn grid(frame: &ValidatedFrame, clusters: &mut ClusterTable) -> Grid {
    let content = frame.content();
    let mut grid = Grid::new(content.columns, content.rows);
    for (index, cell) in content.cells.iter().enumerate() {
        if cell.width == 0 {
            continue;
        }
        let x = (index % usize::from(content.columns)) as u16;
        let y = (index / usize::from(content.columns)) as u16;
        let style = Style::new()
            .fg(color(&cell.foreground))
            .bg(color(&cell.background))
            .with_attrs(attributes(cell));
        grid.put_str(x, y, style, &cell.text, clusters);
        grid.set_style(Rect::new(x, y, u16::from(cell.width), 1), style);
    }
    grid
}

fn color(color: &frame::Color) -> Color {
    match color {
        frame::Color::Default => Color::Default,
        frame::Color::Indexed(index) => Color::Indexed(*index),
        frame::Color::Rgb(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn attributes(cell: &frame::Cell) -> Attrs {
    let attributes = &cell.attributes;
    Attrs::default()
        .set_bold(attributes.bold)
        .set_dim(attributes.dim)
        .set_italic(attributes.italic)
        .set_blink(attributes.blink)
        .set_reverse(attributes.reverse)
        .set_hidden(attributes.hidden)
        .set_strikethrough(attributes.strike)
        .set_overline(attributes.overline)
        .set_underline(match cell.underline {
            frame::Underline::None => UnderlineStyle::None,
            frame::Underline::Single => UnderlineStyle::Single,
            frame::Underline::Double => UnderlineStyle::Double,
            frame::Underline::Curly => UnderlineStyle::Curly,
            frame::Underline::Dotted => UnderlineStyle::Dotted,
            frame::Underline::Dashed => UnderlineStyle::Dashed,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunatui_term::PixelMode;

    fn glyph(text: &str) -> ValidatedFrame {
        let viewport = frame::Viewport {
            columns: 1,
            rows: 1,
            pixels: None,
        };
        ValidatedFrame::new(
            frame::Content {
                columns: 1,
                rows: 1,
                cursor: None,
                tiles: Vec::new(),
                cells: vec![frame::Cell {
                    text: text.into(),
                    width: 1,
                    foreground: frame::Color::Default,
                    background: frame::Color::Default,
                    attributes: frame::Attributes {
                        bold: false,
                        dim: false,
                        italic: false,
                        blink: false,
                        reverse: false,
                        hidden: false,
                        strike: false,
                        overline: false,
                    },
                    underline: frame::Underline::None,
                }],
            },
            &viewport,
        )
        .unwrap()
    }

    #[test]
    fn different_graphemes_never_alias_across_frame_local_cluster_tables() {
        let mut painter = Painter::new(PixelLayer::new(PixelMode::Cells));
        let first = painter.encode(glyph("a\u{301}")).unwrap();
        assert!(first
            .paint
            .windows(3)
            .any(|bytes| bytes == "a\u{301}".as_bytes()));
        let second = painter.encode(glyph("b\u{301}")).unwrap();
        assert!(second
            .paint
            .windows(3)
            .any(|bytes| bytes == "b\u{301}".as_bytes()));
        assert!(painter.encode(glyph("b\u{301}")).unwrap().paint.is_empty());
    }
}
