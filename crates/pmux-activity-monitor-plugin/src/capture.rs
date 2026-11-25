// SPDX-License-Identifier: MIT

use lunatui_core::{CellWidth, ClusterTable, Color, DecodedGlyph, Grid, Rect, UnderlineStyle};
use lunatui_image::{ImageRequest, ImageSink, ImageSource, PixelSize, PixelTarget};
use lunatui_runtime::RenderMode;
use pmux_plugin_api::guest::pmux::view::frame;
use pmux_plugin_api::{tile_limit, MAX_FRAME_BYTES, MAX_TILE_DIMENSION};
use std::cell::Cell;

use crate::app::Monitor;

/// Raster budgets are checked before the widget allocates its target image.
struct Capture {
    pixels: Option<frame::PixelSize>,
    background: [u8; 3],
    remaining: usize,
    /// What this host accepts; an older host grants less and takes fewer tiles.
    max_tiles: usize,
    tiles: Vec<frame::Tile>,
    /// Set when a pixel area was turned away for the budget or the tile limit.
    refused: Cell<bool>,
}

impl Capture {
    fn geometry(&self, area: Rect) -> Option<(u32, u32, usize)> {
        let fits = self.fit(area);
        if fits.is_none() && self.pixels.is_some() {
            self.refused.set(true);
        }
        fits
    }

    fn fit(&self, area: Rect) -> Option<(u32, u32, usize)> {
        let pixels = self.pixels.as_ref()?;
        if self.tiles.len() >= self.max_tiles {
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
    let budget = (byte_limit as usize)
        .min(MAX_FRAME_BYTES)
        .saturating_sub(cell_budget);
    let pixels = viewport.pixels.is_some();
    monitor.load_fonts(pixels);
    let mut mode = monitor.mode(pixels);
    let capture = loop {
        let mut capture = Capture {
            pixels: viewport.pixels,
            background: monitor.palette.ground,
            remaining: budget,
            max_tiles: tile_limit(byte_limit),
            tiles: Vec::new(),
            refused: Cell::new(false),
        };
        monitor.render(
            &mut grid,
            &mut clusters,
            Some(&mut capture),
            area,
            mono_ms,
            pixels,
            mode,
        );
        // A Graphite frame is all or nothing: when part of it does not fit,
        // draw the whole frame again in cells rather than mixing the two.
        if mode == RenderMode::Graphite && capture.refused.get() {
            mode = RenderMode::Cell;
            grid = Grid::new(viewport.columns, viewport.rows);
            clusters = ClusterTable::new();
            continue;
        }
        break capture;
    };
    if mode == RenderMode::Graphite {
        // Graphite blanks the cells it covers. Put back what Cell mode draws
        // there, which has the same layout, so copy, search and saved pane text
        // still read the monitor. The opaque tiles are placed above the text.
        grid = Grid::new(viewport.columns, viewport.rows);
        clusters = ClusterTable::new();
        monitor.render(
            &mut grid,
            &mut clusters,
            None,
            area,
            mono_ms,
            pixels,
            RenderMode::Cell,
        );
    }
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
    monitor.opened = true;
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

    /// The fleet the reviewed comps were drawn from: four busy seats on this
    /// machine, and a disconnected second host with one stale seat.
    pub(crate) fn comps_fleet() -> anyhow::Result<Monitor> {
        use crate::model::tests::record_detail;
        use prismattyc_core::AgentId;
        use serde_json::json;

        const NOW: u64 = 1_800_000_000_000;
        type Script = Vec<(u64, &'static str, serde_json::Value)>;
        fn cycle(task: &str, start: u64, turns: u64, period: u64, end: Script) -> Script {
            let mut out = Vec::new();
            for turn in 0..turns {
                let t = start - turn * period;
                out.push((t, "turn_started", json!({"task": task})));
                out.push((t - 8, "tool_started", json!({"task": task, "tool": "Read"})));
                out.push((
                    t - 20,
                    "edit_finished",
                    json!({"task": task, "file": "src/ui.rs"}),
                ));
                out.push((
                    t - 38,
                    "command_started",
                    json!({"task": task, "command": "cargo test"}),
                ));
                out.push((t - 58, "turn_finished", json!({"task": task})));
            }
            out.extend(end);
            out
        }
        let seats = [
            (
                "builder",
                cycle(
                    "release-notes",
                    590,
                    7,
                    75,
                    vec![(
                        44,
                        "test_failed",
                        json!({"task": "release-notes", "command": "cargo test", "exit_code": 101}),
                    )],
                ),
            ),
            (
                "reviewer",
                cycle(
                    "review-queue",
                    600,
                    7,
                    70,
                    vec![(58, "waiting", json!({"task": "review-queue"}))],
                ),
            ),
            (
                "worker-a",
                cycle(
                    "lane-layout",
                    580,
                    8,
                    68,
                    vec![(
                        3,
                        "command_started",
                        json!({"task": "lane-layout", "command": "cargo clippy"}),
                    )],
                ),
            ),
            (
                "docs",
                cycle(
                    "guide",
                    900,
                    3,
                    120,
                    vec![(
                        400,
                        "tool_started",
                        json!({"task": "guide", "tool": "Grep"}),
                    )],
                ),
            ),
        ];
        let mut all: Vec<_> = seats
            .iter()
            .flat_map(|(seat, script)| {
                script
                    .iter()
                    .map(move |(ago, kind, detail)| (*ago, *seat, *kind, detail.clone()))
            })
            .collect();
        all.sort_by_key(|record| std::cmp::Reverse(record.0));
        let mut records = Vec::new();
        for (sequence, (ago, seat, kind, detail)) in all.into_iter().enumerate() {
            let run = format!("run-{seat}");
            records.push(record_detail(
                seat,
                sequence as u64 + 1,
                kind,
                NOW - ago * 1000,
                &run,
                detail,
            )?);
        }
        let mut monitor = Monitor::new(vec!["local".into(), "lab".into()], Palette::DARK, NOW);
        monitor.hosts[0].apply(page_in("e1", records, None)?, 0);
        monitor.hosts[1].apply(
            page_in(
                "e2",
                vec![record_detail(
                    "nightly",
                    1,
                    "tool_started",
                    NOW - 19_000,
                    "run-n1",
                    json!({"task": "nightly", "tool": "Bash"}),
                )?],
                None,
            )?,
            0,
        );
        monitor.hosts[1].disconnect(NOW - 12_000);
        monitor.ui.motion = false;
        monitor.ui.selected = Some((0, AgentId::new("reviewer")?));
        // Already on screen, so the next frame loads the fonts and draws Graphite.
        monitor.opened = true;
        Ok(monitor)
    }

    /// The default macOS cell: the bundled monospace font at 30 px on a 2x display.
    const RETINA_CELL: frame::PixelSize = frame::PixelSize {
        width: 18,
        height: 41,
    };

    fn viewport(columns: u16, rows: u16, pixels: Option<frame::PixelSize>) -> frame::Viewport {
        frame::Viewport {
            columns,
            rows,
            pixels,
        }
    }

    fn text_of(content: &frame::Content) -> String {
        content
            .cells
            .chunks(usize::from(content.columns))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn covering(content: &frame::Content, column: u16, row: u16) -> Option<&frame::Tile> {
        content.tiles.iter().find(|tile| {
            (tile.x..tile.x + tile.columns).contains(&column)
                && (tile.y..tile.y + tile.rows).contains(&row)
        })
    }

    /// Pixels that differ from the pane ground: what was actually drawn.
    fn ink(tile: &frame::Tile, ground: [u8; 3]) -> usize {
        tile.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[..3] != ground)
            .count()
    }

    fn wide(monitor: &mut Monitor, byte_limit: u32) -> anyhow::Result<frame::Content> {
        render(
            monitor,
            &viewport(132, 30, Some(RETINA_CELL)),
            0,
            byte_limit,
        )
        .map_err(anyhow::Error::msg)
    }

    /// The cell where `needle` starts.
    fn position(content: &frame::Content, needle: &str) -> (u16, u16) {
        let text = text_of(content);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not in the cells:\n{text}"));
        let column = line[..line.find(needle).unwrap()].chars().count();
        (column as u16, row as u16)
    }

    #[test]
    fn graphite_draws_type_chips_and_keycaps_as_pixels_over_their_text() -> anyhow::Result<()> {
        let mut monitor = comps_fleet()?;
        let content = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        // The title, a host note, a flag chip, a footer key and a seat name: each
        // stays in the cells for copy and search, under a tile that draws it.
        for words in [
            "Activity monitor",
            "2 need you",
            "stale",
            "q  quit",
            "reviewer",
        ] {
            let (column, row) = position(&content, words);
            let tile = covering(&content, column + 1, row)
                .unwrap_or_else(|| panic!("{words:?} has no tile over it"));
            assert!(
                ink(tile, monitor.palette.ground) > 50,
                "{words:?} tile is empty"
            );
        }
        let title = covering(&content, 1, 0).expect("the title is a tile");
        // Text tiles are one row tall and as wide as the run they replace.
        assert_eq!((title.rows, title.pixel_height), (1, 41));
        assert_eq!(title.pixel_width, u32::from(title.columns) * 18);
        Ok(())
    }

    #[test]
    fn the_reviewed_views_fit_one_graphite_frame_at_the_default_cell() -> anyhow::Result<()> {
        use crate::ui::Screen;
        for (columns, rows, screen) in [
            (132, 30, Screen::List),
            (100, 26, Screen::Timeline),
            (64, 12, Screen::List),
        ] {
            let mut monitor = comps_fleet()?;
            monitor.ui.screen = screen;
            let content = render(
                &mut monitor,
                &viewport(columns, rows, Some(RETINA_CELL)),
                0,
                MAX_FRAME_BYTES as u32,
            )
            .map_err(anyhow::Error::msg)?;
            assert!(
                covering(&content, 1, 0).is_some(),
                "{columns}x{rows} fell back to cells:\n{}",
                text_of(&content)
            );
            assert!(content.tiles.len() <= pmux_plugin_api::MAX_TILES);
        }
        Ok(())
    }

    #[test]
    fn turning_pixels_off_draws_the_whole_monitor_in_cells() -> anyhow::Result<()> {
        let mut monitor = comps_fleet()?;
        crate::ui::tests_support::press_char(&mut monitor, 'p');
        let content = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        let text = text_of(&content);
        assert!(text.contains("Activity monitor"), "{text}");
        assert!(text.contains("quit"), "{text}");
        assert!(content.tiles.is_empty(), "{} tiles", content.tiles.len());
        Ok(())
    }

    #[test]
    fn the_monitor_opens_in_cells_and_draws_graphite_from_the_next_frame() -> anyhow::Result<()> {
        let mut monitor = comps_fleet()?;
        monitor.opened = false;
        // Only the activity lanes, which need no fonts, are pixels at first.
        let first = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        let (column, row) = position(&first, "reviewer");
        assert!(covering(&first, column, row).is_none());
        assert!(monitor.fonts_pending(true));
        let second = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        assert!(!monitor.fonts_pending(true));
        let (column, row) = position(&second, "reviewer");
        assert!(covering(&second, column, row).is_some());
        assert!(second.tiles.len() > first.tiles.len() * 10);
        Ok(())
    }

    #[test]
    fn fonts_are_not_loaded_while_pixels_are_off_or_not_shown() -> anyhow::Result<()> {
        let mut monitor = comps_fleet()?;
        render(
            &mut monitor,
            &viewport(132, 30, None),
            0,
            MAX_FRAME_BYTES as u32,
        )
        .map_err(anyhow::Error::msg)?;
        assert!(!monitor.fonts_pending(false));
        monitor.ui.pixels = false;
        let off = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        let (column, row) = position(&off, "reviewer");
        assert!(covering(&off, column, row).is_none());
        monitor.ui.pixels = true;
        assert!(
            monitor.fonts_pending(true),
            "no frame so far loaded the fonts"
        );
        let on = wide(&mut monitor, MAX_FRAME_BYTES as u32)?;
        assert!(covering(&on, column, row).is_some());
        Ok(())
    }

    #[test]
    fn an_older_host_gets_only_frames_it_accepts() -> anyhow::Result<()> {
        use pmux_plugin_api::{LEGACY_FRAME_BYTES, LEGACY_MAX_TILES};
        // The narrow view's 39 tiles fit an older host's 2 MiB but not its
        // 32-tile limit: the whole frame is drawn in cells instead.
        let mut monitor = comps_fleet()?;
        let content = render(
            &mut monitor,
            &viewport(64, 12, Some(RETINA_CELL)),
            0,
            LEGACY_FRAME_BYTES as u32,
        )
        .map_err(anyhow::Error::msg)?;
        assert!(content.tiles.len() <= LEGACY_MAX_TILES);
        assert!(text_of(&content).contains("reviewer"));
        // The same view on a current host is one Graphite frame.
        let content = render(
            &mut monitor,
            &viewport(64, 12, Some(RETINA_CELL)),
            0,
            MAX_FRAME_BYTES as u32,
        )
        .map_err(anyhow::Error::msg)?;
        assert!(content.tiles.len() > LEGACY_MAX_TILES);
        Ok(())
    }

    #[test]
    fn a_graphite_frame_that_does_not_fit_is_drawn_whole_in_cells() -> anyhow::Result<()> {
        let mut monitor = comps_fleet()?;
        // Room for the cells and about one MiB of tiles: the title would fit,
        // the rows below it would not.
        let cells = 132 * 30 * (32 + 128) + 256;
        let content = wide(&mut monitor, cells + (1 << 20))?;
        let text = text_of(&content);
        assert!(text.contains("Activity monitor"), "{text}");
        assert!(text.contains("2 need you"), "{text}");
        assert!(text.contains("quit"), "{text}");
        assert!(covering(&content, 1, 0).is_none());
        Ok(())
    }

    #[test]
    fn actual_monitor_cells_and_rgba_lanes_cross_the_typed_adapter() -> anyhow::Result<()> {
        let mut monitor = Monitor::new(vec!["fixture".into()], Palette::DARK, 1_700_000_000_000);
        monitor.opened = true;
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
        assert_eq!(content.cells.len(), 2400);
        // Every tile carries exactly its cells' pixels.
        for tile in &content.tiles {
            assert_eq!(
                (tile.pixel_width, tile.pixel_height, tile.rgba.len() as u32),
                (
                    u32::from(tile.columns) * 8,
                    u32::from(tile.rows) * 16,
                    u32::from(tile.columns) * 8 * u32::from(tile.rows) * 16 * 4
                )
            );
        }
        // The host and seat lanes: 48 cells each, the source column takes the rest.
        let lanes = content.tiles.iter().filter(|tile| tile.columns == 48);
        assert_eq!(lanes.count(), 2);
        // The seat's name and state are drawn as pixels over the same words in cells.
        let (column, row) = position(&content, "worker-a");
        assert!(covering(&content, column, row).is_some());
        assert!(text_of(&content).contains("waiting"));
        Ok(())
    }

    #[test]
    fn the_guest_frame_path_lets_time_pass_for_a_quiet_source() -> anyhow::Result<()> {
        use crate::model::SeatState;

        const WALL: u64 = 1_700_000_000_000;
        let mut monitor = Monitor::new(vec!["remote".into()], Palette::DARK, WALL);
        monitor.hosts[0].apply(
            page_in(
                "epoch",
                vec![record_run("busy", 1, "tool_started", 1_000, "run")?],
                None,
            )?,
            0,
        );
        let viewport = frame::Viewport {
            columns: 100,
            rows: 8,
            pixels: None,
        };
        let state = |monitor: &Monitor| {
            monitor.hosts[0]
                .seats
                .values()
                .next()
                .map(|seat| seat.state)
        };
        // The first frame learns the source's time; no poll follows.
        render(&mut monitor, &viewport, 10, MAX_FRAME_BYTES as u32).map_err(anyhow::Error::msg)?;
        assert_eq!(state(&monitor), Some(SeatState::Working));
        // Five quiet minutes of viewer time later, the next frame shows the turn stuck.
        monitor.now_ms = WALL + 5 * 60_000;
        let content = render(&mut monitor, &viewport, 20, MAX_FRAME_BYTES as u32)
            .map_err(anyhow::Error::msg)?;
        assert_eq!(state(&monitor), Some(SeatState::Stuck));
        let text = content
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.contains("stuck"), "{text}");
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
            max_tiles: tile_limit(MAX_FRAME_BYTES as u32),
            tiles: Vec::new(),
            refused: Cell::new(false),
        };
        assert!(capture.pixel_target(Rect::new(0, 0, 500, 1)).is_none());
        assert!(capture.pixel_target(Rect::new(0, 0, 1, 1)).is_some());
    }
}
