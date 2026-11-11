//! Offscreen renders of the launch splash and the command palette at the
//! window sizes a tiled, fractionally scaled desktop produces.
//!
//! A 1.6x scale draws every cell 1.6 times larger, so the same pixel width
//! holds fewer columns and rows. These tests build windows from a cell count
//! at each scale and paint through the production painters. Set
//! `OVERLAY_FIT_SHOTS` to a directory to also write each frame as a PNG.

use crate::graphite;
use crate::graphite_overlays;
use crate::keybind::KeyMap;
use crate::mux::ChromeGeom;
use crate::palette::Palette;
use crate::raster::{
    self, pack_argb, FontMetrics, OverlaySurface, PaletteFrame, PaletteLayout, PaletteLayoutMode,
    PaletteSection,
};
use crate::splash;

pub(crate) const BG: [u8; 3] = [0x28, 0x28, 0x28];
pub(crate) const SCALES: [f32; 2] = [1.0, 1.6];

pub(crate) struct Window {
    pub font: FontMetrics,
    pub chrome: ChromeGeom,
    pub width: usize,
    pub height: usize,
    pub cols: usize,
    pub rows: usize,
}

/// A window that is exactly `cols` by `rows` cells at `scale`.
pub(crate) fn window(scale: f32, cols: usize, rows: usize) -> Option<Window> {
    let font = FontMetrics::load(14.0 * scale).ok()?;
    let chrome = ChromeGeom {
        graphite: true,
        scale_milli: (scale * 1000.0).round() as u32,
    };
    let (width, height) = (cols * font.cell_w, rows * font.cell_h);
    Some(Window {
        font,
        chrome,
        width,
        height,
        cols,
        rows,
    })
}

impl Window {
    /// The same font and scale at another size, without reloading the font.
    pub(crate) fn resized(mut self, cols: usize, rows: usize) -> Self {
        self.cols = cols;
        self.rows = rows;
        self.width = cols * self.font.cell_w;
        self.height = rows * self.font.cell_h;
        self
    }
}

pub(crate) fn blank(window: &Window) -> Vec<u32> {
    vec![pack_argb(255, BG); window.width * window.height]
}

pub(crate) fn write_png(name: &str, pixels: &[u32], width: usize, height: usize) {
    let Some(dir) = std::env::var_os("OVERLAY_FIT_SHOTS").map(std::path::PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("create screenshot directory");
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create screenshot");
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write PNG header");
    let rgba = pixels
        .iter()
        .flat_map(|pixel| {
            [
                ((pixel >> 16) & 0xff) as u8,
                ((pixel >> 8) & 0xff) as u8,
                (pixel & 0xff) as u8,
                255,
            ]
        })
        .collect::<Vec<_>>();
    writer.write_image_data(&rgba).expect("write PNG pixels");
}

/// Paint the Graphite splash as the host does and return the frame.
pub(crate) fn fitted_splash(window: &Window, elapsed: Option<u64>) -> splash::Fitted {
    splash::fit_to_window(
        splash::Page::Main,
        "0.3.62",
        0,
        elapsed,
        false,
        (window.width, window.height),
        (
            window.font.cell_w,
            graphite_overlays::splash_line_height(&window.font, window.chrome),
        ),
    )
}

/// Paint the Graphite splash as the host does and return the frame.
pub(crate) fn paint_splash(window: &Window, animated: bool) -> Vec<u32> {
    let elapsed = animated.then_some(prismattyc_core::splash::SETTLED_MS + 2_000);
    let fitted = fitted_splash(window, elapsed);
    let mut buffer = blank(window);
    graphite_overlays::splash(
        &window.font,
        window.chrome,
        &graphite::DARK,
        &fitted,
        elapsed,
        OverlaySurface::default(),
        &mut buffer,
        window.width,
        window.height,
        [98, 168, 255],
    );
    buffer
}

/// Paint the command palette as the host does: fixed height, with the query,
/// filter chips and every command row.
pub(crate) fn paint_palette(window: &Window) -> (Vec<u32>, Option<PaletteLayout>) {
    let palette = Palette::default();
    let view = palette.view(&KeyMap::default(), false);
    let chips = Palette::chip_labels();
    let detail = view.detail(palette.selected);
    let sections = [
        PaletteSection {
            header: "RECENT",
            subtitle: "",
            rows: &view.recent,
        },
        PaletteSection {
            header: "MATCHES",
            subtitle: &palette.query,
            rows: &view.matches,
        },
    ];
    let frame = PaletteFrame {
        layout_mode: PaletteLayoutMode::FixedHeight,
        query: Some(&palette.query),
        query_focused: true,
        chips: Some((&chips, palette.chip_index())),
        sections: &sections,
        selected: palette.selected,
        scroll: palette.scroll,
        detail: detail.as_ref(),
        footer: "Enter run · Esc close · ↑↓/wheel move · C-←/→ filter",
    };
    let mut buffer = blank(window);
    let layout = graphite_overlays::palette(
        &window.font,
        &graphite::DARK,
        window.chrome,
        [98, 168, 255],
        &frame,
        OverlaySurface::default(),
        &mut buffer,
        window.width,
        window.height,
        None,
        None,
    );
    (buffer, layout)
}

/// Columns of the window grid that hold anything but the background.
pub(crate) fn painted_extent(window: &Window, buffer: &[u32]) -> (usize, usize) {
    let bg = pack_argb(255, BG);
    let (mut left, mut right) = (usize::MAX, 0);
    for y in 0..window.height {
        let row = &buffer[y * window.width..(y + 1) * window.width];
        if let Some(first) = row.iter().position(|p| *p != bg) {
            left = left.min(first);
        }
        if let Some(last) = row.iter().rposition(|p| *p != bg) {
            right = right.max(last);
        }
    }
    (left, right)
}

/// Every size the review screenshots and the checks below walk through.
const SIZES: [(usize, usize); 8] = [
    (30, 30),
    (40, 30),
    (60, 30),
    (80, 30),
    (100, 30),
    (160, 40),
    (60, 12),
    (80, 14),
];

/// Write the review screenshots when `OVERLAY_FIT_SHOTS` names a directory.
#[test]
fn render_matrix_for_review() {
    if std::env::var_os("OVERLAY_FIT_SHOTS").is_none() {
        return;
    }
    for scale in SCALES {
        for (cols, rows) in SIZES {
            let Some(w) = window(scale, cols, rows) else {
                return;
            };
            let tag = format!("{cols}x{rows}-{scale}x");
            write_png(
                &format!("splash-{tag}"),
                &paint_splash(&w, true),
                w.width,
                w.height,
            );
            let (shot, _) = paint_palette(&w);
            write_png(&format!("palette-{tag}"), &shot, w.width, w.height);
        }
    }
}

#[test]
fn splash_text_stays_inside_the_window_at_both_scales() {
    for scale in SCALES {
        let Some(mut w) = window(scale, 1, 1) else {
            return;
        };
        for cols in [20, 30, 40, 50, 60, 70, 80, 98, 99, 100, 120, 160] {
            for rows in [6, 10, 14, 18, 24, 40] {
                w = w.resized(cols, rows);
                let fitted = fitted_splash(&w, None);
                let line_h = graphite_overlays::splash_line_height(&w.font, w.chrome);
                assert!(
                    fitted.lines.len() * line_h <= w.height,
                    "{scale}x {cols}x{rows}: {} lines of {line_h}px do not fit {}px",
                    fitted.lines.len(),
                    w.height
                );
                for (index, line) in fitted.lines.iter().enumerate() {
                    let (face, px) =
                        graphite_overlays::splash_line_style(&fitted, index, &w.font, w.chrome);
                    let painted: f32 = line
                        .iter()
                        .map(|(text, _)| {
                            graphite::text_width(face, px, graphite_overlays::splash_text(text))
                        })
                        .sum();
                    assert!(
                        painted.ceil() as usize <= w.width,
                        "{scale}x {cols}x{rows}: line {index} {:?} is {painted}px in a {}px window",
                        line.iter().map(|(t, _)| t.as_str()).collect::<String>(),
                        w.width
                    );
                }
            }
        }
    }
}

#[test]
fn the_full_wordmark_appears_only_when_it_fits_the_cells() {
    // The same pixel width holds fewer columns at 1.6x, so the same window
    // must fall back to a smaller wordmark there.
    let pixels = 100 * FontMetrics::load(14.0).map_or(8, |f| f.cell_w);
    let mut fits = Vec::new();
    for scale in SCALES {
        let Some(font) = FontMetrics::load(14.0 * scale).ok() else {
            return;
        };
        let cols = pixels / font.cell_w;
        fits.push((scale, cols, splash::fit_for(cols, 30)));
    }
    assert_eq!(fits[0].2, splash::Fit::Full, "{fits:?}");
    assert_ne!(fits[1].2, splash::Fit::Full, "{fits:?}");
    assert!(fits[1].1 < fits[0].1, "{fits:?}");
}

#[test]
fn every_wordmark_size_paints_its_own_art() {
    let Some(wide) = window(1.0, 110, 30) else {
        return;
    };
    let Some(mid) = window(1.0, 60, 30) else {
        return;
    };
    let Some(narrow) = window(1.0, 30, 30) else {
        return;
    };
    assert_eq!(fitted_splash(&wide, None).fit, splash::Fit::Full);
    assert_eq!(fitted_splash(&mid, None).fit, splash::Fit::Compact);
    assert_eq!(fitted_splash(&narrow, None).fit, splash::Fit::Wordmark);
    for (window, name) in [(&wide, "full"), (&mid, "compact"), (&narrow, "one-line")] {
        let shot = paint_splash(window, false);
        let bright = shot
            .iter()
            .filter(|p| {
                [(*p >> 16) & 0xff, (*p >> 8) & 0xff, *p & 0xff]
                    .iter()
                    .all(|c| *c > 0xc0)
            })
            .count();
        assert!(
            bright > 40,
            "{name} painted no wordmark ({bright} bright pixels)"
        );
    }
}

#[test]
fn the_palette_stays_inside_its_panel_and_the_window() {
    for scale in SCALES {
        let Some(mut w) = window(scale, 1, 1) else {
            return;
        };
        for cols in [24, 40, 60, 80, 120] {
            for rows in [14, 30] {
                w = w.resized(cols, rows);
                let (shot, layout) = paint_palette(&w);
                let layout = layout.unwrap_or_else(|| panic!("{scale}x {cols}x{rows}: no palette"));
                assert!(
                    layout.panel_x + layout.panel_w <= w.width
                        && layout.panel_y + layout.panel_h <= w.height,
                    "{scale}x {cols}x{rows}: panel {}..{} of {}",
                    layout.panel_x,
                    layout.panel_x + layout.panel_w,
                    w.width
                );
                let (left, right) = painted_extent(&w, &shot);
                let slack = w.chrome.px(32.0);
                assert!(
                    left + slack >= layout.panel_x
                        && right <= layout.panel_x + layout.panel_w + slack,
                    "{scale}x {cols}x{rows}: painted {left}..{right}, panel {}..{}",
                    layout.panel_x,
                    layout.panel_x + layout.panel_w
                );
            }
        }
    }
}

#[test]
fn a_short_window_still_lists_commands_and_gives_up_the_detail_panel() {
    for scale in SCALES {
        for rows in [12, 14, 16] {
            let Some(w) = window(scale, 60, rows) else {
                return;
            };
            let (shot, layout) = paint_palette(&w);
            let layout = layout.unwrap_or_else(|| panic!("{scale}x {rows} rows: no palette"));
            assert!(layout.detail_hidden, "{scale}x {rows} rows");
            assert!(
                !layout.rows.is_empty(),
                "{scale}x {rows} rows: no command row fits"
            );
            assert!(shot.iter().any(|p| *p != pack_argb(255, BG)));
        }
        let Some(tall) = window(scale, 60, 30) else {
            return;
        };
        let (_, layout) = paint_palette(&tall);
        assert!(!layout.expect("tall palette").detail_hidden);
    }
}

#[test]
fn a_narrow_row_drops_its_keybinding_when_the_name_would_be_squeezed() {
    // inner width, chord cells, name_x, name cells, gap
    assert!(raster::palette_chord_fits(40, 13, 0, 22, 1));
    assert!(raster::palette_chord_fits(24, 13, 0, 22, 1));
    assert!(!raster::palette_chord_fits(23, 13, 0, 22, 1));
    assert!(!raster::palette_chord_fits(14, 13, 0, 22, 1));
    assert!(
        raster::palette_chord_fits(14, 0, 0, 22, 1),
        "no chord, nothing to drop"
    );
    // A short name never needs the whole minimum column.
    assert!(raster::palette_chord_fits(10, 3, 0, 4, 1));
}
