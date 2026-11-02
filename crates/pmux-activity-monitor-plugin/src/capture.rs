// SPDX-License-Identifier: MIT

use lunatui_core::{CellWidth, ClusterTable, Color, DecodedGlyph, Grid, Rect, UnderlineStyle};
use lunatui_image::{ImageRequest, ImageSink, ImageSource, PixelSize, PixelTarget};
use pmux_plugin_api::guest::pmux::view::frame;
use pmux_plugin_api::{MAX_FRAME_BYTES, MAX_TILES, MAX_TILE_DIMENSION};

use crate::app::Monitor;

/// Raster budgets are checked before the widget allocates its target image.
struct Capture {
    pixels: Option<frame::PixelSize>,
    background: [u8; 3],
    remaining: usize,
    tiles: Vec<frame::Tile>,
}

impl Capture {
    fn geometry(&self, area: Rect) -> Option<(u32, u32, usize)> {
        let pixels = self.pixels.as_ref()?;
        if self.tiles.len() >= MAX_TILES {
            return None;
        }
        let width = u32::from(area.width).checked_mul(u32::from(pixels.width))?;
        let height = u32::from(area.height).checked_mul(u32::from(pixels.height))?;
        if width == 0 || height == 0 || width > MAX_TILE_DIMENSION || height > MAX_TILE_DIMENSION {
            return None;
        }
        let bytes = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        (bytes.checked_add(64)? <= self.remaining).then_some((width, height, bytes))
    }
}

impl ImageSink for Capture {
    fn supports_pixels(&self) -> bool {
        self.pixels.is_some()
    }

    fn pixel_target(&self, area: Rect) -> Option<PixelTarget> {
        let (width, height, _) = self.geometry(area)?;
        Some(PixelTarget::new(
            PixelSize::new(width, height)?,
            false,
            Some(self.background),
        ))
    }

    fn submit_raster(&mut self, source: &ImageSource, area: Rect, depth: i32) -> bool {
        let Some((width, height, bytes)) = self.geometry(area) else {
            return false;
        };
        if depth != 0
            || source.size().width() != width
            || source.size().height() != height
            || source.rgba8_pixels().len() != bytes
        {
            return false;
        }
        self.tiles.push(frame::Tile {
            x: area.x,
            y: area.y,
            columns: area.width,
            rows: area.height,
            pixel_width: width,
            pixel_height: height,
            rgba: source.rgba8_pixels().to_vec(),
        });
        self.remaining -= bytes + 64;
        true
    }

    fn submit(&mut self, _: ImageRequest<'_>) -> bool {
        false
    }
}

pub(crate) fn render(
    monitor: &mut Monitor,
    viewport: &frame::Viewport,
    mono_ms: u64,
    byte_limit: u32,
) -> Result<frame::Content, String> {
    let area = Rect::new(0, 0, viewport.columns, viewport.rows);
    let mut grid = Grid::new(viewport.columns, viewport.rows);
    let mut clusters = ClusterTable::new();
    // Reserve the maximum accepted grapheme bytes for each cell. If a large
    // viewport leaves no room, widgets retain their cell fallback.
    let cell_budget = usize::from(viewport.columns)
        .saturating_mul(usize::from(viewport.rows))
        .saturating_mul(32 + 128)
        .saturating_add(256);
    let mut capture = Capture {
        pixels: viewport.pixels,
        background: monitor.palette.ground,
        remaining: (byte_limit as usize)
            .min(MAX_FRAME_BYTES)
            .saturating_sub(cell_budget),
        tiles: Vec::new(),
    };
    monitor.render(
        &mut grid,
        &mut clusters,
        Some(&mut capture),
        area,
        mono_ms,
        viewport.pixels.is_some(),
    );
    let theme = monitor.palette.theme();
    let color = |color| match color {
        Color::Default => Ok(frame::Color::Default),
        Color::Indexed(index) => Ok(frame::Color::Indexed(index)),
        Color::Rgb(r, g, b) => Ok(frame::Color::Rgb(frame::Rgb { r, g, b })),
        Color::Token(_) => Err("unresolved monitor color token".to_string()),
    };
    let mut cells = Vec::with_capacity(usize::from(grid.width()) * usize::from(grid.height()));
    for y in 0..grid.height() {
        for x in 0..grid.width() {
            let cell = grid.get(x, y).ok_or("monitor grid is incomplete")?;
            let text = match cell.glyph().decode() {
                DecodedGlyph::Inline(character) => character.to_string(),
                DecodedGlyph::Cluster(id) => clusters
                    .cluster_text(id)
                    .ok_or("monitor glyph is missing")?
                    .to_owned(),
                DecodedGlyph::Continuation => String::new(),
            };
            let style = cell.style();
            let resolved = |value| {
                if let Color::Token(id) = value {
                    theme.resolve_id(id)
                } else {
                    value
                }
            };
            let attrs = style.attrs;
            cells.push(frame::Cell {
                text,
                width: match cell.meta().width {
                    CellWidth::One => 1,
                    CellWidth::Two => 2,
                    CellWidth::Continuation => 0,
                },
                foreground: color(resolved(style.fg))?,
                background: color(resolved(style.bg))?,
                attributes: frame::Attributes {
                    bold: attrs.bold(),
                    dim: attrs.dim(),
                    italic: attrs.italic(),
                    blink: attrs.blink(),
                    reverse: attrs.reverse(),
                    hidden: attrs.hidden(),
                    strike: attrs.strikethrough(),
                    overline: attrs.overline(),
                },
                underline: match attrs.underline() {
                    UnderlineStyle::None => frame::Underline::None,
                    UnderlineStyle::Single => frame::Underline::Single,
                    UnderlineStyle::Double => frame::Underline::Double,
                    UnderlineStyle::Curly => frame::Underline::Curly,
                    UnderlineStyle::Dotted => frame::Underline::Dotted,
                    UnderlineStyle::Dashed => frame::Underline::Dashed,
                },
            });
        }
    }
    Ok(frame::Content {
        columns: viewport.columns,
        rows: viewport.rows,
        cells,
        tiles: capture.tiles,
        cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{page_in, record_run};
    use crate::ui::Palette;

    #[test]
    fn actual_monitor_cells_and_rgba_lanes_cross_the_typed_adapter() -> anyhow::Result<()> {
        let mut monitor = Monitor::new(vec!["fixture".into()], Palette::DARK, 1_700_000_000_000);
        monitor.hosts[0].apply(
            page_in(
                "e",
                vec![record_run(
                    "worker-a",
                    1,
                    "waiting",
                    monitor.now_ms,
                    "run-1",
                )?],
                None,
            )?,
            0,
        );
        let viewport = frame::Viewport {
            columns: 100,
            rows: 24,
            pixels: Some(frame::PixelSize {
                width: 8,
                height: 16,
            }),
        };
        let content = render(&mut monitor, &viewport, 0, MAX_FRAME_BYTES as u32)
            .map_err(anyhow::Error::msg)?;
        let text = content
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.contains("worker-a"));
        assert!(text.contains("waiting"));
        assert_eq!(content.cells.len(), 2400);
        assert_eq!(content.tiles.len(), 2);
        for tile in content.tiles {
            assert_eq!(
                (tile.pixel_width, tile.pixel_height, tile.rgba.len()),
                (456, 16, 29184)
            );
        }
        Ok(())
    }

    #[test]
    fn oversized_pixel_targets_keep_the_cell_fallback_before_allocation() {
        let capture = Capture {
            pixels: Some(frame::PixelSize {
                width: 256,
                height: 256,
            }),
            background: [0; 3],
            remaining: MAX_FRAME_BYTES,
            tiles: Vec::new(),
        };
        assert!(capture.pixel_target(Rect::new(0, 0, 500, 1)).is_none());
        assert!(capture.pixel_target(Rect::new(0, 0, 1, 1)).is_some());
    }
}
