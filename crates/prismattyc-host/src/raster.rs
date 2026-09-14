//! Software cell-grid rasterizer for the windowed host.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fontdue::{Font, FontSettings};
use prismattyc_core::splash::{
    art_frame_width, flare_intensities, ART, ART_MARGIN, ART_MARGIN_LEFT, FLARE_GLYPH,
    FLARE_PEAK_GLYPH, RAY_GLYPH, STREAK_CORE_GLYPH, STREAK_GLYPH,
};
use prismattyc_core::{CellRange, Color, Screen, Style, UnderlineStyle};
use prismattyc_emulator::{CursorShape, Emulator, PlacedImage, KITTY_PLACEHOLDER};
use prismattyc_render::CellRectOverlay;
use ttf_parser::GlyphId;

use crate::config::PaneTitlesMode;
#[cfg(test)]
use crate::theme::default_theme;
use crate::theme::Theme;
use crate::title_row::title_row_decision;

/// Extra title-row inputs for [`rasterize_tab_strip_with_theme`] (PT-190).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TitleRowStyle<'a> {
    pub mode: PaneTitlesMode,
    pub notice: Option<(usize, usize, &'a str)>,
}

/// z1 overlay fill — distinct from the classic grid so clip tests can assert
/// pixels without depending on glyph coverage.
#[cfg(test)]
pub const OVERLAY_BG: [u8; 3] = [0x2a, 0x36, 0x4a];

/// Default dark theme (independent of outer terminal).
#[cfg(test)]
const DEFAULT_FG: [u8; 3] = [0xd0, 0xd0, 0xd0];
#[cfg(test)]
const DEFAULT_BG: [u8; 3] = [0x12, 0x12, 0x14];
#[cfg(test)]
const CHROME_BG: [u8; 3] = [0x1b, 0x1e, 0x26];
const CHROME_FG: [u8; 3] = [0xe5, 0xe9, 0xf0];
/// Active-tab underline: 1px at ~70% focus over strip background (no alpha).
const TAB_MARKER_H: usize = 1;
const TAB_MARKER_OPACITY: f32 = 0.7;
pub(crate) const TAB_LABEL_INSET: usize = 4;
/// Suffix on a tab label whose pane is zoomed (PT-57), like tmux `Z`.
const TAB_ZOOM_MARK: &str = "[Z]";
#[cfg(test)]
const PANE_BORDER: [u8; 3] = [0x45, 0x4a, 0x57];
#[cfg(test)]
const UNSEEN_BADGE: [u8; 3] = [0xff, 0xb4, 0x54];
/// Mail letter. Amber is allowed (needs-you); distinct from the
/// upper-right unseen `!` by shape and corner, not by hue.
#[cfg(test)]
const MAIL_LETTER: [u8; 3] = [0xff, 0xb4, 0x54];
#[cfg(test)]
const ACTIVE_BADGE: [u8; 3] = [0x4c, 0xd1, 0x8b];
#[cfg(test)]
const ATTENTION_BADGE: [u8; 3] = [0xff, 0x6b, 0x6b];
/// Inset from the pane slot origin so the glyph sits inside the 1px focus
/// ring (and the 3px raster gap), not on the ring itself.
const MAIL_RING_INSET: usize = 3;
/// Envelope bitmap size (6–9px spec).
const MAIL_GLYPH_W: usize = 8;
const MAIL_GLYPH_H: usize = 6;
/// 1 = envelope ink. Upper-left of the pane; not a text ✉ cell.
const MAIL_ENVELOPE: [[u8; MAIL_GLYPH_W]; MAIL_GLYPH_H] = [
    [0, 1, 1, 1, 1, 1, 1, 0],
    [1, 1, 0, 0, 0, 0, 1, 1],
    [1, 0, 1, 1, 1, 1, 0, 1],
    [1, 0, 0, 1, 1, 0, 0, 1],
    [1, 0, 1, 1, 1, 1, 0, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];

/// Brand “Continuous beam” facet colors (`assets/brand/README.md`).
/// Default focus index is blue (matches the historic host accent).
pub const FOCUS_BORDER_PALETTE: &[(&str, [u8; 3])] = &[
    ("coral", [0xff, 0x6e, 0x63]),
    ("amber", [0xff, 0xb4, 0x54]),
    ("yellow", [0xff, 0xe0, 0x66]),
    ("green", [0x7b, 0xd8, 0x8f]),
    ("blue", [0x62, 0xa8, 0xff]),
    ("indigo", [0x7b, 0x8c, 0xfa]),
    ("violet", [0x9b, 0x8c, 0xf5]),
    ("ink", [0xd0, 0xd0, 0xd0]),
];

pub const DEFAULT_FOCUS_BORDER_INDEX: usize = 4; // blue

pub fn focus_border_name(index: usize) -> &'static str {
    FOCUS_BORDER_PALETTE[index % FOCUS_BORDER_PALETTE.len()].0
}

pub fn focus_border_rgb(index: usize) -> [u8; 3] {
    FOCUS_BORDER_PALETTE[index % FOCUS_BORDER_PALETTE.len()].1
}

/// Parse name (`blue`, `coral`, …) or 0-based index. Unknown → `None`.
pub fn parse_focus_border(spec: &str) -> Option<usize> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<usize>() {
        if n < FOCUS_BORDER_PALETTE.len() {
            return Some(n);
        }
        return None;
    }
    let lower = s.to_ascii_lowercase();
    FOCUS_BORDER_PALETTE
        .iter()
        .position(|(name, _)| *name == lower)
}

pub fn cycle_focus_border(index: usize) -> usize {
    (index + 1) % FOCUS_BORDER_PALETTE.len()
}

pub fn cycle_focus_border_back(index: usize) -> usize {
    (index + FOCUS_BORDER_PALETTE.len() - 1) % FOCUS_BORDER_PALETTE.len()
}

/// xterm 256 / ANSI-ish palette (0–15 + 6×6×6 cube + grayscale).
#[cfg(test)]
fn palette_rgb(index: u8) -> [u8; 3] {
    palette_rgb_with_theme(default_theme(), index)
}

fn palette_rgb_with_theme(theme: &Theme, index: u8) -> [u8; 3] {
    match index {
        0..=15 => theme.ansi[usize::from(index)],
        16..=231 => {
            let n = index - 16;
            let r = n / 36;
            let g = (n / 6) % 6;
            let b = n % 6;
            let level = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            [level(r), level(g), level(b)]
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            [v, v, v]
        }
    }
}

fn color_rgb_with_theme(theme: &Theme, color: Color, is_fg: bool) -> [u8; 3] {
    match color {
        Color::Default => {
            if is_fg {
                theme.default_fg
            } else {
                theme.default_bg
            }
        }
        Color::Ansi(i) => palette_rgb_with_theme(theme, i.min(15)),
        Color::Indexed(i) => palette_rgb_with_theme(theme, i),
        Color::Rgb { r, g, b } => [r, g, b],
    }
}

fn resolve_style_colors_with_theme(theme: &Theme, style: &Style) -> ([u8; 3], [u8; 3]) {
    let mut fg = color_rgb_with_theme(theme, style.foreground, true);
    let mut bg = color_rgb_with_theme(theme, style.background, false);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.bold && matches!(style.foreground, Color::Ansi(i) if i < 8) {
        if let Color::Ansi(i) = style.foreground {
            fg = palette_rgb_with_theme(theme, i + 8);
            if style.inverse {
                // bold only brightens the “foreground” side of inverse.
            }
        }
    }
    (fg, bg)
}

fn resolve_underline_color_with_theme(
    theme: &Theme,
    style: &Style,
    effective_fg: [u8; 3],
) -> [u8; 3] {
    match style.underline_color {
        Color::Default => effective_fg,
        color => color_rgb_with_theme(theme, color, true),
    }
}

fn underline_style(style: &Style) -> UnderlineStyle {
    if style.underline_style == UnderlineStyle::None && style.underline {
        UnderlineStyle::Single
    } else {
        style.underline_style
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_underline(
    buffer: &mut [u32],
    stride_px: usize,
    x0: usize,
    y0: usize,
    cell_w: usize,
    cell_h: usize,
    clip_y1: usize,
    style: UnderlineStyle,
    color: [u8; 3],
) {
    if cell_w == 0 || cell_h == 0 || style == UnderlineStyle::None {
        return;
    }
    let baseline = y0.saturating_add(cell_h.saturating_sub(2));
    let paint_row = |buffer: &mut [u32], row: usize, width: usize| {
        if row < clip_y1 {
            fill_rect(buffer, stride_px, x0, row, width, 1, color);
        }
    };
    match style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => paint_row(buffer, baseline, cell_w),
        UnderlineStyle::Double => {
            paint_row(buffer, y0.saturating_add(cell_h.saturating_sub(3)), cell_w);
            paint_row(buffer, y0.saturating_add(cell_h.saturating_sub(1)), cell_w);
        }
        UnderlineStyle::Dotted => {
            if baseline >= clip_y1 {
                return;
            }
            for x in (0..cell_w).step_by(2) {
                fill_rect(
                    buffer,
                    stride_px,
                    x0.saturating_add(x),
                    baseline,
                    1,
                    1,
                    color,
                );
            }
        }
        UnderlineStyle::Dashed => {
            if baseline >= clip_y1 {
                return;
            }
            for x in (0..cell_w).step_by(5) {
                let width = 3.min(cell_w.saturating_sub(x));
                fill_rect(
                    buffer,
                    stride_px,
                    x0.saturating_add(x),
                    baseline,
                    width,
                    1,
                    color,
                );
            }
        }
        UnderlineStyle::Curly => {
            const ZIGZAG: [isize; 4] = [-1, 0, 1, 0];
            for x in 0..cell_w {
                let row = baseline.saturating_add_signed(ZIGZAG[x % 4]);
                if row < clip_y1 {
                    fill_rect(buffer, stride_px, x0 + x, row, 1, 1, color);
                }
            }
        }
    }
}

/// Outline coverage bitmap from fontdue (alpha only; tinted with cell FG).
struct CoverageGlyph {
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    bitmap: Vec<u8>,
}

/// Pre-rasterized CBDT/CBLC color emoji (own RGB; not FG-tinted).
struct ColorGlyph {
    ox: i32,
    oy: i32,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

enum GlyphPaint {
    Coverage(CoverageGlyph),
    Color(ColorGlyph),
    Empty,
}

/// Probe order from. A candidate is accepted only when its
/// rasterized ink fits in one cell — existence is not enough.
const CLOSE_GLYPH_CANDIDATES: [char; 4] = ['\u{F467}', '\u{2715}', '\u{00D7}', 'x'];

/// Right edge of outline ink relative to the cell origin, if the face
/// has a non-empty raster for `ch`.
fn close_glyph_ink_right<'a>(
    fonts: impl IntoIterator<Item = &'a Font>,
    px: f32,
    ch: char,
) -> Option<i32> {
    for font in fonts {
        let gid = font.lookup_glyph_index(ch);
        if gid == 0 {
            continue;
        }
        let (metrics, bitmap) = font.rasterize_indexed(gid, px);
        if metrics.width == 0 || metrics.height == 0 || bitmap.is_empty() {
            continue;
        }
        if !bitmap.iter().any(|&cover| cover > 0) {
            continue;
        }
        return Some(metrics.xmin + metrics.width as i32);
    }
    None
}

/// Loaded monospace face + optional symbol fallbacks + cell metrics.
pub struct FontMetrics {
    /// The eager primary face. Fallback outlines live in `lazy_fonts`.
    fonts: Vec<Font>,
    /// Ordered candidates. Parse outlines only after a cmap match.
    lazy_fonts: Vec<LazyFont>,
    /// Raw bytes and collection index for the primary face. The same face is
    /// used by rustybuzz and fontdue so shaped glyph IDs rasterize correctly.
    primary_bytes: Vec<u8>,
    primary_index: u32,
    /// CBDT color-emoji font bytes (e.g. Noto Color Emoji). Kept for `ttf_parser::Face`.
    emoji_fonts: Vec<Vec<u8>>,
    /// Ghostty-style lazy system faces (CJK, etc.). Not bundled.
    cover_cache: RefCell<HashMap<char, Option<crate::system_fonts::CoveringFace>>>,
    fallback_fonts: RefCell<HashMap<(PathBuf, u32), Font>>,
    pub px: f32,
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: f32,
    /// Cached close-tab glyph. First probe (F467 / 2715 / 00D7 / x)
    /// whose rasterized ink fits in `cell_w`.
    pub close_glyph: char,
}

impl FontMetrics {
    /// Test convenience: default chain only.
    #[cfg(test)]
    pub fn load(px: f32) -> anyhow::Result<Self> {
        Self::load_with(px, None, &[])
    }

    #[cfg(test)]
    fn load_baked(px: f32) -> anyhow::Result<Self> {
        let font = Font::from_bytes(BAKED_MONO, FontSettings::default())
            .map_err(|error| anyhow::anyhow!("parse bundled test font: {error}"))?;
        let (metrics, _) = font.rasterize('M', px);
        let line = font
            .horizontal_line_metrics(px)
            .ok_or_else(|| anyhow::anyhow!("bundled test font has no line metrics"))?;
        let cell_w = (metrics.advance_width.ceil() as usize).max(1);
        let cell_h = (line.new_line_size.ceil() as usize)
            .max(1)
            .saturating_add(1);
        let baseline = line.ascent;
        let mut metrics = Self {
            fonts: vec![font],
            lazy_fonts: Vec::new(),
            primary_bytes: BAKED_MONO.to_vec(),
            primary_index: 0,
            emoji_fonts: Vec::new(),
            cover_cache: RefCell::new(HashMap::new()),
            fallback_fonts: RefCell::new(HashMap::new()),
            px,
            cell_w,
            cell_h,
            baseline,
            close_glyph: 'x',
        };
        metrics.close_glyph = metrics.pick_close_glyph();
        Ok(metrics)
    }

    /// `PRISMATTYC_HOST_FONT*` env vars still take precedence inside the chain.
    pub fn load_with(
        px: f32,
        primary: Option<&Path>,
        extra_fallbacks: &[PathBuf],
    ) -> anyhow::Result<Self> {
        Self::load_sources(px, font_chain(primary, extra_fallbacks))
    }

    fn load_sources(px: f32, sources: Vec<FontSource>) -> anyhow::Result<Self> {
        let mut sources = sources.into_iter();
        let mut fonts = Vec::new();
        let mut names = Vec::new();
        let mut primary_bytes = None;
        for source in sources.by_ref() {
            let name = source.name();
            let Some(data) = source.read() else {
                continue;
            };
            match Font::from_bytes(data.as_ref(), FontSettings::default()) {
                Ok(font) => {
                    primary_bytes = Some(data.into_owned());
                    names.push(name);
                    fonts.push(font);
                    break;
                }
                Err(e) => eprintln!("prismattyc-host: parse font {name}: {e}"),
            }
        }
        let Some(primary_bytes) = primary_bytes else {
            anyhow::bail!("no loadable fonts from candidates (bundled font failed to parse)");
        };
        let lazy_fonts: Vec<_> = sources.map(LazyFont::new).collect();
        names.extend(lazy_fonts.iter().map(|font| font.source.name()));
        // Cell metrics from the primary mono face only.
        let primary = &fonts[0];
        let (metrics, _) = primary.rasterize('M', px);
        let line = primary
            .horizontal_line_metrics(px)
            .ok_or_else(|| anyhow::anyhow!("font has no horizontal line metrics: {}", names[0]))?;
        let cell_w = (metrics.advance_width.ceil() as usize).max(1);
        let cell_h = (line.new_line_size.ceil() as usize)
            .max(1)
            .saturating_add(1);
        let baseline = line.ascent;
        eprintln!(
            "prismattyc-host: font {} (+{} fallback)  cell {}x{}  px={px}",
            names[0],
            names.len().saturating_sub(1),
            cell_w,
            cell_h
        );
        if names.len() > 1 {
            eprintln!(
                "prismattyc-host: glyph fallback candidates: {}",
                names[1..].join(", ")
            );
        }

        let mut emoji_fonts = Vec::new();
        let mut emoji_names = Vec::new();
        for path in emoji_font_chain() {
            match std::fs::read(&path) {
                Ok(data) => {
                    if ttf_parser::Face::parse(&data, 0).is_ok() {
                        emoji_names.push(path.display().to_string());
                        emoji_fonts.push(data);
                    } else {
                        eprintln!(
                            "prismattyc-host: skip emoji font {}: parse failed",
                            path.display()
                        );
                    }
                }
                Err(e) => eprintln!("prismattyc-host: skip emoji font {}: {e}", path.display()),
            }
        }
        if !emoji_names.is_empty() {
            eprintln!("prismattyc-host: color emoji: {}", emoji_names.join(", "));
        }
        let mut metrics = Self {
            fonts,
            lazy_fonts,
            primary_bytes,
            primary_index: 0,
            emoji_fonts,
            cover_cache: RefCell::new(HashMap::new()),
            fallback_fonts: RefCell::new(HashMap::new()),
            px,
            cell_w,
            cell_h,
            baseline,
            close_glyph: 'x',
        };
        metrics.close_glyph = metrics.pick_close_glyph();
        Ok(metrics)
    }

    fn outline_fonts(&self, ch: char) -> impl Iterator<Item = &Font> {
        self.fonts.iter().chain(
            self.lazy_fonts
                .iter()
                .filter_map(move |font| font.for_char(ch)),
        )
    }

    /// First candidate whose ink fits one cell. ASCII x is the floor.
    fn pick_close_glyph(&self) -> char {
        let box_w = i32::try_from(self.cell_w.max(1)).unwrap_or(i32::MAX);
        CLOSE_GLYPH_CANDIDATES
            .into_iter()
            .find(|&ch| {
                self.close_glyph_ink_right(ch)
                    .is_some_and(|right| right <= box_w)
            })
            .unwrap_or('x')
    }

    fn close_glyph_ink_right(&self, ch: char) -> Option<i32> {
        close_glyph_ink_right(self.outline_fonts(ch), self.px, ch)
    }

    pub(crate) fn primary_covers(&self, ch: char) -> bool {
        self.fonts
            .first()
            .is_some_and(|font| font.lookup_glyph_index(ch) != 0)
    }

    fn primary_face(&self) -> Option<rustybuzz::Face<'_>> {
        rustybuzz::Face::from_slice(&self.primary_bytes, self.primary_index)
    }

    fn rasterize_primary_indexed(&self, glyph_id: usize, px: f32) -> (fontdue::Metrics, Vec<u8>) {
        self.fonts[0].rasterize_indexed(u16::try_from(glyph_id).unwrap_or(u16::MAX), px)
    }

    /// First covering outline face, else CBDT color emoji, else empty.
    ///
    /// Never draws primary `.notdef` tofu — missing codepoints stay blank.
    #[cfg(test)]
    fn paint_char(&self, ch: char) -> GlyphPaint {
        self.paint_char_at(ch, self.px)
    }

    fn paint_char_at(&self, ch: char, px: f32) -> GlyphPaint {
        // Whitespace needs no pixels and must not load a fallback face.
        if ch.is_whitespace() {
            return GlyphPaint::Empty;
        }
        for font in self.outline_fonts(ch) {
            let gid = font.lookup_glyph_index(ch);
            if gid == 0 {
                continue;
            }
            let (m, bitmap) = font.rasterize_indexed(gid, px);
            if m.width == 0 || m.height == 0 || bitmap.is_empty() {
                // A mapped glyph is covered even when it only advances the
                // cursor (braille blank) or has zero advance (BOM).
                return GlyphPaint::Empty;
            }
            if !bitmap.iter().any(|&b| b > 0) {
                return GlyphPaint::Empty;
            }
            return GlyphPaint::Coverage(CoverageGlyph {
                xmin: m.xmin,
                ymin: m.ymin,
                width: m.width,
                height: m.height,
                bitmap,
            });
        }
        // Color emoji before system outline (review on #216): dual-
        // presentation chars (U+2764, VS16) must not fall through to a
        // monochrome text face that happens to cover them. CJK has no
        // color strike and still lands on paint_system_outline.
        if (px - self.px).abs() < 0.01 {
            if let Some(color) = self.rasterize_color_emoji(ch) {
                return GlyphPaint::Color(color);
            }
        }
        if let Some(coverage) = self.paint_system_outline(ch, px) {
            return GlyphPaint::Coverage(coverage);
        }
        GlyphPaint::Empty
    }

    /// Outline from a system face that covers `ch` (CJK, etc.). Ghostty
    /// `discoverFallback` by codepoint; we cache the face.
    fn paint_system_outline(&self, ch: char, px: f32) -> Option<CoverageGlyph> {
        if ch.is_whitespace() {
            return None;
        }
        let face = {
            let mut cache = self.cover_cache.borrow_mut();
            cache
                .entry(ch)
                .or_insert_with(|| crate::system_fonts::covering_face(ch))
                .clone()
        }?;
        let mut loaded = self.fallback_fonts.borrow_mut();
        let font = match loaded.get(&(face.path.clone(), face.index)) {
            Some(font) => font,
            None => {
                let data = std::fs::read(&face.path).ok()?;
                let settings = FontSettings {
                    collection_index: face.index,
                    ..FontSettings::default()
                };
                let font = Font::from_bytes(data.as_slice(), settings).ok()?;
                eprintln!(
                    "prismattyc-host: system fallback {}#{} (U+{:04X})",
                    face.path.display(),
                    face.index,
                    ch as u32
                );
                loaded.insert((face.path.clone(), face.index), font);
                loaded.get(&(face.path.clone(), face.index))?
            }
        };
        let gid = font.lookup_glyph_index(ch);
        if gid == 0 {
            return None;
        }
        let (m, bitmap) = font.rasterize_indexed(gid, px);
        if m.width == 0 || m.height == 0 || bitmap.is_empty() {
            return None;
        }
        if !bitmap.iter().any(|&b| b > 0) {
            return None;
        }
        Some(CoverageGlyph {
            xmin: m.xmin,
            ymin: m.ymin,
            width: m.width,
            height: m.height,
            bitmap,
        })
    }

    fn coverage_origin_y(&self, glyph: &CoverageGlyph) -> i32 {
        self.baseline.round() as i32 - glyph.ymin - glyph.height as i32
    }

    /// Rasterize smaller when outline ink would clip the cell (Nerd/Claude
    /// title spinners sit above the mono baseline and lose their top).
    fn paint_char_fitting(&self, ch: char, max_w: i32, max_h: i32) -> GlyphPaint {
        let mut px = self.px;
        let mut last = self.paint_char_at(ch, px);
        for _ in 0..8 {
            match &last {
                GlyphPaint::Coverage(glyph) => {
                    // `shift_ink_into_clip` can correct bearings and baseline
                    // overhang, but it cannot preserve a bitmap larger than
                    // the paint box. Horizontal clipping is exactly `max_w`;
                    // vertically the rasterizer intentionally has one extra
                    // baseline pixel (`clip_y1 = cell_y + cell_h + 1`).
                    if glyph.width as i32 <= max_w && glyph.height as i32 <= max_h + 1 {
                        return last;
                    }
                }
                GlyphPaint::Color(_) | GlyphPaint::Empty => return last,
            }
            px *= 0.82;
            if px < 7.0 {
                break;
            }
            last = self.paint_char_at(ch, px);
        }
        last
    }

    /// CBDT/CBLC PNG strikes (fontdue cannot outline these).
    fn rasterize_color_emoji(&self, ch: char) -> Option<ColorGlyph> {
        let target = self.cell_h.max(self.px.round() as usize).max(1) as u16;
        let requests = [
            target,
            target.saturating_mul(2),
            16,
            18,
            20,
            24,
            32,
            36,
            48,
            64,
            72,
            96,
            109,
            128,
        ];
        for data in &self.emoji_fonts {
            let Ok(face) = ttf_parser::Face::parse(data, 0) else {
                continue;
            };
            let Some(gid) = face.glyph_index(ch) else {
                continue;
            };
            for &px in &requests {
                let Some(img) = face.glyph_raster_image(gid, px) else {
                    continue;
                };
                if !matches!(img.format, ttf_parser::RasterImageFormat::PNG) {
                    continue;
                }
                let Some((src_w, src_h, pixels)) = decode_png_rgba(img.data) else {
                    continue;
                };
                if src_w == 0 || src_h == 0 {
                    continue;
                }
                let max_w = self.cell_w.saturating_mul(2).max(self.cell_w);
                let max_h = self.cell_h.saturating_sub(1).max(1);
                let (dw, dh, scaled) = scale_rgba_nearest(src_w, src_h, &pixels, max_w, max_h);
                let ox = ((self.cell_w as i32 - dw as i32) / 2).max(0);
                let oy = ((self.cell_h as i32 - dh as i32) / 2).max(0);
                return Some(ColorGlyph {
                    ox,
                    oy,
                    width: dw,
                    height: dh,
                    rgba: scaled,
                });
            }
        }
        None
    }

    /// Shape a cluster (flag RI pair, ZWJ emoji) against color-emoji fonts.
    /// Ghostty/HarfBuzz: a successful flag is **one** glyph, not two RI tiles.
    fn paint_emoji_cluster(&self, cluster: &str) -> Option<ColorGlyph> {
        if !cluster_needs_emoji_shape(cluster) {
            return None;
        }
        for data in &self.emoji_fonts {
            for index in 0..4u32 {
                let Some(face) = rustybuzz::Face::from_slice(data, index) else {
                    break;
                };
                let mut buf = rustybuzz::UnicodeBuffer::new();
                buf.push_str(cluster);
                let glyphs = rustybuzz::shape(&face, &[], buf);
                let infos = glyphs.glyph_infos();
                if infos.len() != 1 {
                    continue;
                }
                let gid = infos[0].glyph_id;
                if gid == 0 {
                    continue;
                }
                if let Some(color) = self.rasterize_emoji_gid(data, index, gid) {
                    return Some(color);
                }
            }
        }
        None
    }

    fn rasterize_emoji_gid(&self, data: &[u8], index: u32, gid: u32) -> Option<ColorGlyph> {
        let face = ttf_parser::Face::parse(data, index).ok()?;
        let gid = GlyphId(u16::try_from(gid).ok()?);
        let target = self.cell_h.max(self.px.round() as usize).max(1) as u16;
        let requests = [target, target.saturating_mul(2), 16, 32, 64, 72, 96, 128];
        for &px in &requests {
            let Some(img) = face.glyph_raster_image(gid, px) else {
                continue;
            };
            if !matches!(img.format, ttf_parser::RasterImageFormat::PNG) {
                continue;
            }
            let Some((src_w, src_h, pixels)) = decode_png_rgba(img.data) else {
                continue;
            };
            if src_w == 0 || src_h == 0 {
                continue;
            }
            let max_w = self.cell_w.saturating_mul(2).max(self.cell_w);
            let max_h = self.cell_h.saturating_sub(1).max(1);
            let (dw, dh, scaled) = scale_rgba_nearest(src_w, src_h, &pixels, max_w, max_h);
            let ox = ((self.cell_w as i32 - dw as i32) / 2).max(0);
            let oy = ((self.cell_h as i32 - dh as i32) / 2).max(0);
            return Some(ColorGlyph {
                ox,
                oy,
                width: dw,
                height: dh,
                rgba: scaled,
            });
        }
        None
    }
}

fn cluster_needs_emoji_shape(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let Some(second) = chars.next() else {
        return false;
    };
    (prismattyc_core::is_regional_indicator(first)
        && prismattyc_core::is_regional_indicator(second))
        || cluster.contains(prismattyc_core::ZWJ)
}

/// Flags (RI pair) and ZWJ emoji live in a width-2 cell even when the lead
/// scalar's East Asian Width is 1.
fn cluster_occupies_two_cells(cluster: &str) -> bool {
    cluster_needs_emoji_shape(cluster)
        || cluster
            .chars()
            .next()
            .is_some_and(|ch| prismattyc_core::char_display_width(ch) >= 2)
}

/// JetBrains Mono Nerd Font (SIL OFL 1.1) — bundled primary mono + icon face.
/// License: `assets/fonts/JetBrainsMonoNerdFont-OFL.txt`.
const BAKED_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf");
const BAKED_MONO_NAME: &str = "<bundled JetBrainsMonoNerdFont-Regular.ttf>";

/// DejaVu Sans Mono (Bitstream Vera / DejaVu license) — bundled glyph fallback
/// for punctuation the Nerd face omits (e.g. U+22C5 spinner dot). License:
/// `assets/fonts/DejaVuSansMono-LICENSE.txt`.
const BAKED_FALLBACK: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
const BAKED_FALLBACK_NAME: &str = "<bundled DejaVuSansMono.ttf>";

/// Noto Sans Symbols 2 (SIL OFL 1.1) — bundled glyph fallback for Miscellaneous
/// Technical symbols the Nerd and DejaVu faces omit, notably the media controls
/// Claude Code emits for its footer mode indicator (U+23F8 pause, U+23F9 stop,
/// U+23FA record, U+23ED..U+23EF). Kept in the outline chain ahead of the
/// color-emoji fonts so these render as crisp monochrome glyphs, not emoji, and
/// on hosts without any system symbol font (macOS, bare Linux). License:
/// `assets/fonts/NotoSansSymbols2-OFL.txt`.
const BAKED_SYMBOLS: &[u8] = include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.ttf");
const BAKED_SYMBOLS_NAME: &str = "<bundled NotoSansSymbols2-Regular.ttf>";

/// A face to load: an on-disk path, or a font baked into the binary.
enum FontSource {
    Path(PathBuf),
    Baked {
        name: &'static str,
        bytes: &'static [u8],
    },
}

impl FontSource {
    fn name(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Baked { name, .. } => (*name).to_string(),
        }
    }

    fn read(&self) -> Option<Cow<'static, [u8]>> {
        match self {
            Self::Path(path) => match std::fs::read(path) {
                Ok(data) => Some(Cow::Owned(data)),
                Err(error) => {
                    eprintln!("prismattyc-host: skip font {}: {error}", path.display());
                    None
                }
            },
            Self::Baked { bytes, .. } => Some(Cow::Borrowed(bytes)),
        }
    }
}

struct LazyFont {
    source: FontSource,
    bytes: OnceCell<Option<Cow<'static, [u8]>>>,
    font: OnceCell<Option<Font>>,
    coverage: RefCell<HashMap<char, bool>>,
}

impl LazyFont {
    fn new(source: FontSource) -> Self {
        Self {
            source,
            bytes: OnceCell::new(),
            font: OnceCell::new(),
            coverage: RefCell::new(HashMap::new()),
        }
    }

    fn for_char(&self, ch: char) -> Option<&Font> {
        if let Some(font) = self.font.get() {
            return font.as_ref();
        }
        let bytes = self.bytes.get_or_init(|| self.source.read()).as_ref()?;
        // cmap lookup borrows the source. It does not expand every outline.
        let covered = *self.coverage.borrow_mut().entry(ch).or_insert_with(|| {
            ttf_parser::Face::parse(bytes, 0)
                .ok()
                .and_then(|face| face.glyph_index(ch))
                .is_some_and(|gid| gid.0 != 0)
        });
        if !covered {
            return None;
        }
        self.font
            .get_or_init(|| {
                let name = self.source.name();
                match Font::from_bytes(bytes.as_ref(), FontSettings::default()) {
                    Ok(font) => {
                        eprintln!(
                            "prismattyc-host: loaded fallback {name} (U+{:04X})",
                            ch as u32
                        );
                        Some(font)
                    }
                    Err(error) => {
                        eprintln!("prismattyc-host: parse font {name}: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }
}

/// Ordered font chain: primary mono, then symbol-rich fallbacks.
///
/// The bundled JetBrains Mono Nerd Font is always in the chain — as the primary
/// when no system mono resolves, else as the first fallback — so the host never
/// depends on installed fonts.
///
/// - `PRISMATTYC_HOST_FONT` — primary override
/// - `PRISMATTYC_HOST_FONT_FALLBACK` — `:`-separated extra faces after primary
fn font_chain(primary_override: Option<&Path>, extra_fallbacks: &[PathBuf]) -> Vec<FontSource> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Ok(p) = std::env::var("PRISMATTYC_HOST_FONT") {
        push_font(&mut out, &mut seen, PathBuf::from(p));
    }
    // Config-file primary: after the env override, before built-in candidates.
    if let Some(primary) = primary_override {
        push_font(&mut out, &mut seen, primary.to_path_buf());
    }

    // Primary monospaces (prefer full Nerd over “Mono” patch — more icons).
    const PRIMARY: &[&str] = &[
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFontMono-Regular.ttf",
        "/usr/share/fonts/TTF/HackNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/Hack-Regular.ttf",
        "/usr/share/fonts/truetype/hack/Hack-Regular.ttf",
        "/usr/share/fonts/Adwaita/AdwaitaMono-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    ];
    for c in PRIMARY {
        if out.is_empty() {
            push_font(&mut out, &mut seen, PathBuf::from(c));
        }
    }
    // Bundled mono: the primary when nothing above resolved, else the first
    // fallback. Guarantees a working face on hosts with no system fonts.
    out.push(FontSource::Baked {
        name: BAKED_MONO_NAME,
        bytes: BAKED_MONO,
    });
    // Bundled DejaVu Sans Mono: glyph fallback for punctuation the Nerd face
    // omits (spinner dots), so coverage never depends on system fonts.
    out.push(FontSource::Baked {
        name: BAKED_FALLBACK_NAME,
        bytes: BAKED_FALLBACK,
    });
    // Always try to add icon + complete-mono + symbol faces after primary.
    // Complete monos come *before* Noto Symbols: a configured Nerd primary
    // skips PRIMARY, and Nerd faces omit Grok Working-spinner punctuation
    // (U+22C5 / U+2E2C / U+2059). Noto Symbols 2 has only U+22C5 and is
    // proportional, so it clips in a cell.
    const FALLBACKS: &[&str] = &[
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/SymbolsNerdFontMono-Regular.ttf",
        "/usr/share/fonts/Adwaita/AdwaitaMono-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf",
        "/usr/share/fonts/TTF/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    for c in FALLBACKS {
        push_font(&mut out, &mut seen, PathBuf::from(c));
    }
    // Bundled Noto Sans Symbols 2: guaranteed outline coverage for Miscellaneous
    // Technical media controls (Claude Code footer U+23F8..U+23FA) that the mono
    // faces omit. After the system FALLBACKS — a complete mono Nerd/symbol face,
    // where installed, wins first (Noto Symbols 2 is proportional) — but
    // before the color-emoji fonts, so these render as monochrome glyphs, not
    // emoji or tofu, on macOS and bare Linux where no system symbol font exists.
    out.push(FontSource::Baked {
        name: BAKED_SYMBOLS_NAME,
        bytes: BAKED_SYMBOLS,
    });
    if let Ok(extra) = std::env::var("PRISMATTYC_HOST_FONT_FALLBACK") {
        for part in extra.split(':') {
            if !part.is_empty() {
                push_font(&mut out, &mut seen, PathBuf::from(part));
            }
        }
    }
    for path in extra_fallbacks {
        push_font(&mut out, &mut seen, path.clone());
    }
    out
}

fn push_font(out: &mut Vec<FontSource>, seen: &mut std::collections::HashSet<PathBuf>, p: PathBuf) {
    if p.is_file() && seen.insert(p.clone()) {
        out.push(FontSource::Path(p));
    }
}

/// Color-emoji fonts (CBDT). Not used as outline faces — fontdue cannot paint them.
fn emoji_font_chain() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |out: &mut Vec<PathBuf>, p: PathBuf| {
        if p.is_file() && seen.insert(p.clone()) {
            out.push(p);
        }
    };
    if let Ok(p) = std::env::var("PRISMATTYC_HOST_EMOJI_FONT") {
        push(&mut out, PathBuf::from(p));
    }
    const CANDIDATES: &[&str] = &[
        // macOS system color emoji (sbix PNG); best-effort, absent is harmless.
        #[cfg(target_os = "macos")]
        "/System/Library/Fonts/Apple Color Emoji.ttc",
        "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
    ];
    for c in CANDIDATES {
        push(&mut out, PathBuf::from(c));
    }
    out
}

pub(crate) fn decode_png_rgba(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let w = info.width as usize;
    let h = info.height as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let rgb = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity(w * h * 4);
            for px in rgb.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        _ => return None,
    };
    if rgba.len() < w * h * 4 {
        return None;
    }
    Some((w, h, rgba))
}

/// Nearest-neighbor scale into at most `max_w`×`max_h`, preserving aspect.
fn scale_rgba_nearest(
    src_w: usize,
    src_h: usize,
    src: &[u8],
    max_w: usize,
    max_h: usize,
) -> (usize, usize, Vec<u8>) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0, Vec::new());
    }
    let scale = (max_w as f32 / src_w as f32)
        .min(max_h as f32 / src_h as f32)
        .min(1.0);
    let dw = ((src_w as f32 * scale).round() as usize).max(1).min(max_w);
    let dh = ((src_h as f32 * scale).round() as usize).max(1).min(max_h);
    if dw == src_w && dh == src_h {
        return (src_w, src_h, src.to_vec());
    }
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let sy = y * src_h / dh;
        for x in 0..dw {
            let sx = x * src_w / dw;
            let si = (sy * src_w + sx) * 4;
            let di = (y * dw + x) * 4;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    (dw, dh, out)
}

/// Scale `src` to COVER `dst_w`×`dst_h` (max ratio) and center-crop.
pub(crate) fn cover_scale_rgba(
    src_w: usize,
    src_h: usize,
    src: &[u8],
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 || src.len() < src_w * src_h * 4 {
        return vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    }
    let src_wider = src_w.saturating_mul(dst_h) > dst_w.saturating_mul(src_h);
    let (off_x, off_y, samp_w, samp_h) = if src_wider {
        let samp_w = (src_h.saturating_mul(dst_w) / dst_h.max(1))
            .max(1)
            .min(src_w);
        let off_x = src_w.saturating_sub(samp_w) / 2;
        (off_x, 0usize, samp_w, src_h)
    } else {
        let samp_h = (src_w.saturating_mul(dst_h) / dst_w.max(1))
            .max(1)
            .min(src_h);
        let off_y = src_h.saturating_sub(samp_h) / 2;
        (0usize, off_y, src_w, samp_h)
    };
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = off_y + y * samp_h / dst_h;
        let sy = sy.min(src_h - 1);
        for x in 0..dst_w {
            let sx = off_x + x * samp_w / dst_w;
            let sx = sx.min(src_w - 1);
            let si = (sy * src_w + sx) * 4;
            let di = (y * dst_w + x) * 4;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    out
}

/// Separable box blur, three passes. `radius == 0` is a no-op.
/// Each pass is a sliding-window sum (O(w·h), independent of radius).
pub(crate) fn box_blur_rgba(rgba: &mut [u8], w: usize, h: usize, radius: u32) {
    if radius == 0 || w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return;
    }
    let radius = radius as usize;
    let mut tmp = vec![0u8; w * h * 4];
    for _ in 0..3 {
        blur_axis(rgba, &mut tmp, w, h, radius, true);
        blur_axis(&tmp, rgba, w, h, radius, false);
    }
}

fn blur_axis(src: &[u8], dst: &mut [u8], w: usize, h: usize, radius: usize, horizontal: bool) {
    let kernel = radius * 2 + 1;
    let k = kernel as u32;
    if horizontal {
        for y in 0..h {
            blur_line(src, dst, y * w * 4, 4, w, radius, k);
        }
    } else {
        for x in 0..w {
            blur_line(src, dst, x * 4, w * 4, h, radius, k);
        }
    }
}

/// Clamp-to-edge box mean along one line. `origin` is the first pixel's byte
/// offset; `stride` is the byte step to the next pixel on this axis.
fn blur_line(
    src: &[u8],
    dst: &mut [u8],
    origin: usize,
    stride: usize,
    n: usize,
    radius: usize,
    k: u32,
) {
    if n == 0 {
        return;
    }
    let last = n - 1;
    let px = |i: usize| origin + i * stride;
    let mut acc = [0u32; 4];
    for t in 0..(radius * 2 + 1) {
        let si = px(t.saturating_sub(radius).min(last));
        for c in 0..4 {
            acc[c] += u32::from(src[si + c]);
        }
    }
    let mut di = origin;
    for c in 0..4 {
        dst[di + c] = (acc[c] / k) as u8;
    }
    for i in 1..n {
        let so = px((i - 1).saturating_sub(radius));
        let si = px((i + radius).min(last));
        di += stride;
        for c in 0..4 {
            acc[c] = acc[c] - u32::from(src[so + c]) + u32::from(src[si + c]);
            dst[di + c] = (acc[c] / k) as u8;
        }
    }
}

/// Lerp `rgb` toward `theme_bg` by `1 - opacity`. Opacity 0 is the theme bg.
pub(crate) fn background_tint(rgb: [u8; 3], theme_bg: [u8; 3], opacity: f32) -> u32 {
    let opacity = opacity.clamp(0.0, 1.0);
    let weight = (opacity * 256.0).round() as u16;
    pack_rgb(mix_rgb(theme_bg, rgb, weight))
}

/// Decode, cover-scale, blur, and tint a PNG into a window-sized 0RGB layer.
pub(crate) fn build_background_layer(
    png: &[u8],
    w: usize,
    h: usize,
    theme_bg: [u8; 3],
    opacity: f32,
    blur: u32,
) -> Option<Vec<u32>> {
    if w == 0 || h == 0 {
        return None;
    }
    let (src_w, src_h, rgba) = decode_png_rgba(png)?;
    if src_w == 0 || src_h == 0 {
        return None;
    }
    let mut scaled = cover_scale_rgba(src_w, src_h, &rgba, w, h);
    drop(rgba);
    box_blur_rgba(&mut scaled, w, h, blur);
    let mut out = Vec::with_capacity(w * h);
    for px in scaled.chunks_exact(4) {
        out.push(background_tint([px[0], px[1], px[2]], theme_bg, opacity));
    }
    if out.len() != w * h {
        return None;
    }
    Some(out)
}

/// Extra paint flags for [`rasterize_screen_at_with_theme`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenPaint {
    /// Leave default-bg cells untouched so a frame-level background shows.
    pub skip_default_bg: bool,
    /// Blend weight of the cell background over the existing buffer.
    /// `256` is opaque. `0` leaves the backdrop. Glyphs stay opaque.
    pub bg_weight: u16,
    /// Straight alpha written for a *default-background* cell (PT-87
    /// `window_opacity` scaled by this pane's opacity). Cells with an explicit
    /// SGR background, inverse, or selection stay opaque, as in Ghostty.
    pub default_bg_alpha: u8,
}

impl Default for ScreenPaint {
    fn default() -> Self {
        Self {
            skip_default_bg: false,
            bg_weight: 256,
            default_bg_alpha: OPAQUE_ALPHA,
        }
    }
}

/// Map 0.0–1.0 opacity onto [`ScreenPaint::bg_weight`].
pub(crate) fn opacity_to_weight(opacity: f32) -> u16 {
    (opacity.clamp(0.0, 1.0) * 256.0).round() as u16
}

/// Paint `screen` into an RGBA softbuffer (`u32` 0RGB: 0x00RRGGBB).
///
/// `scroll_offset` is rows above the live bottom (primary history view).
/// Selection ranges use absolute history coordinates.
#[cfg(test)]
pub fn rasterize_screen(
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    scroll_offset: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
) {
    // Preserve the original single-grid contract: clear unused margins.
    let used_w = screen.columns() * font.cell_w;
    let used_h = screen.rows() * font.cell_h;
    for (i, px) in buffer.iter_mut().enumerate() {
        let x = i % stride_px;
        let y = i / stride_px;
        if x >= used_w || y >= used_h {
            *px = pack_rgb(DEFAULT_BG);
        }
    }
    rasterize_screen_at(
        screen,
        font,
        cursor_visible,
        CursorShape::Block,
        scroll_offset,
        selection,
        buffer,
        stride_px,
        0,
        0,
        usize::MAX,
        usize::MAX,
    );
}

/// Paint one pane at a pixel offset inside an already-cleared host buffer.
///
/// `clip_w`/`clip_h` cap the paint box so a guest grid wider than the pane
/// cannot draw into a sibling (split after a full-width session).
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn rasterize_screen_at(
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    scroll_offset: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
) {
    rasterize_screen_at_with_theme(
        default_theme(),
        screen,
        font,
        cursor_visible,
        cursor_shape,
        scroll_offset,
        selection,
        buffer,
        stride_px,
        origin_x,
        origin_y,
        clip_w,
        clip_h,
        ScreenPaint::default(),
    );
}

/// Themed live-host variant of [`rasterize_screen_at`].
#[allow(clippy::too_many_arguments)]
pub fn rasterize_screen_at_with_theme(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    scroll_offset: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    paint: ScreenPaint,
) {
    rasterize_screen_at_with_theme_options(
        theme,
        screen,
        font,
        cursor_visible,
        cursor_shape,
        scroll_offset,
        selection,
        buffer,
        stride_px,
        origin_x,
        origin_y,
        clip_w,
        clip_h,
        false,
        &[],
        paint,
        &[],
        0,
    );
}

/// Themed terminal-grid rasterizer without a partial row filter.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_screen_at_with_theme_options(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    scroll_offset: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    ligatures: bool,
    features: &[String],
    paint: ScreenPaint,
    images: &[PlacedImage],
    scrolled_lines: u64,
) {
    rasterize_screen_at_with_theme_options_filtered(
        theme,
        screen,
        font,
        cursor_visible,
        cursor_shape,
        scroll_offset,
        selection,
        buffer,
        stride_px,
        origin_x,
        origin_y,
        clip_w,
        clip_h,
        ligatures,
        features,
        paint,
        images,
        scrolled_lines,
        None,
    );
}

/// Themed terminal-grid rasterizer with optional host-only OpenType shaping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_screen_at_with_theme_options_filtered(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    scroll_offset: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    ligatures: bool,
    features: &[String],
    paint: ScreenPaint,
    images: &[PlacedImage],
    scrolled_lines: u64,
    paint_rows: Option<&[usize]>,
) {
    let scroll = scroll_offset.min(screen.max_view_scroll());
    let clip_x1 = origin_x.saturating_add(clip_w);
    let clip_y1 = origin_y.saturating_add(clip_h);
    let cursor = screen.cursor();

    if ligatures {
        rasterize_ligature_rows(
            theme,
            screen,
            font,
            scroll,
            selection,
            cursor,
            buffer,
            stride_px,
            origin_x,
            origin_y,
            clip_x1,
            clip_y1,
            features,
            paint,
            images,
            scrolled_lines,
            paint_rows,
        );
    } else {
        rasterize_unshaped_rows(
            theme,
            screen,
            font,
            scroll,
            selection,
            buffer,
            stride_px,
            origin_x,
            origin_y,
            clip_x1,
            clip_y1,
            paint,
            images,
            scrolled_lines,
            paint_rows,
        );
    }

    paint_live_caret(
        theme,
        screen,
        font,
        cursor_visible,
        cursor_shape,
        scroll,
        buffer,
        stride_px,
        origin_x,
        origin_y,
        clip_x1,
        clip_y1,
        images,
        scrolled_lines,
        paint_rows,
    );
}

#[allow(clippy::too_many_arguments)]
fn rasterize_unshaped_rows(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    scroll: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x1: usize,
    clip_y1: usize,
    paint: ScreenPaint,
    images: &[PlacedImage],
    scrolled_lines: u64,
    paint_rows: Option<&[usize]>,
) {
    for row in 0..screen.rows() {
        if paint_rows.is_some_and(|selected| selected.binary_search(&row).is_err()) {
            continue;
        }
        for col in 0..screen.columns() {
            if screen.view_cell(scroll, row, col).wide_cont {
                continue;
            }
            rasterize_unshaped_cell(
                theme,
                screen,
                font,
                scroll,
                selection,
                buffer,
                stride_px,
                origin_x,
                origin_y,
                clip_x1,
                clip_y1,
                paint,
                images,
                scrolled_lines,
                row,
                col,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rasterize_unshaped_cell(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    scroll: usize,
    selection: Option<CellRange>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x1: usize,
    clip_y1: usize,
    paint: ScreenPaint,
    images: &[PlacedImage],
    scrolled_lines: u64,
    row: usize,
    col: usize,
) {
    let cell = screen.view_cell(scroll, row, col);
    let covered = direct_image_covers_view_cell(images, scrolled_lines, scroll, row, col);
    let (mut fg, mut bg) = resolve_style_colors_with_theme(theme, &cell.style);
    // Host selection is paint-only: never mutate the child grid/style.
    if selection.is_some_and(|range| screen.selection_covers_abs_at_view(scroll, range, row, col)) {
        if let (Some(selection_fg), Some(selection_bg)) = (theme.selection_fg, theme.selection_bg) {
            fg = selection_fg;
            bg = selection_bg;
        } else {
            std::mem::swap(&mut fg, &mut bg);
        }
    }
    let x0 = origin_x + col * font.cell_w;
    let y0 = origin_y + row * font.cell_h;
    if x0 >= clip_x1 || y0 >= clip_y1 {
        return;
    }
    let mut grapheme = String::new();
    cell.write_grapheme_into(&mut grapheme);
    let ch_draw = grapheme.chars().next().unwrap_or(cell.character);
    let is_placeholder = cell.character == KITTY_PLACEHOLDER;
    // Mail letter is smaller than a cell. Do not fill a full-cell pad
    // under it — unused rows stay the pane default.
    let cell_bg = if ch_draw == MAIL_LETTER_GLYPH {
        theme.default_bg
    } else {
        bg
    };
    let span = if col + 1 < screen.columns() && screen.view_cell(scroll, row, col + 1).wide_cont {
        2
    } else {
        1
    };
    let cell_w = font
        .cell_w
        .saturating_mul(span)
        .min(clip_x1.saturating_sub(x0));
    let cell_h = font.cell_h.min(clip_y1.saturating_sub(y0));
    if covered {
        paint_image_covered_cell_background(
            theme, cell.style, ch_draw, buffer, stride_px, x0, y0, cell_w, cell_h, paint,
        );
        return;
    }
    let selected =
        selection.is_some_and(|range| screen.selection_covers_abs_at_view(scroll, range, row, col));
    let default_bg_cell =
        cell.style.background == Color::Default && !cell.style.inverse && !selected;
    // The envelope keeps its theme pad over a background image
    // (never skipped) but carries the window alpha like every
    // other default-bg cell, so it is not an opaque square (PT-206).
    let skip_bg = paint.skip_default_bg && default_bg_cell && ch_draw != MAIL_LETTER_GLYPH;
    paint_cell_background(
        buffer,
        stride_px,
        x0,
        y0,
        cell_w,
        cell_h,
        cell_bg,
        if default_bg_cell {
            paint.default_bg_alpha
        } else {
            OPAQUE_ALPHA
        },
        skip_bg,
        selected,
        paint.bg_weight,
    );

    // U+10EEEE is a Kitty virtual-placement cell, not a glyph. Painting
    // it hits Last Resort tofu on macOS under alpha icons (Ghostty skips).
    if !is_placeholder && ch_draw != ' ' && ch_draw != '\0' {
        blit_cluster_in(
            buffer,
            stride_px,
            font,
            &grapheme,
            x0,
            y0,
            fg,
            origin_x,
            clip_x1,
            ch_draw == MAIL_LETTER_GLYPH,
        );
    }

    if !is_placeholder {
        let decoration = underline_style(&cell.style);
        if decoration != UnderlineStyle::None {
            draw_underline(
                buffer,
                stride_px,
                x0,
                y0,
                cell_w,
                font.cell_h,
                clip_y1,
                decoration,
                resolve_underline_color_with_theme(theme, &cell.style, fg),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_live_caret(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    cursor_visible: bool,
    cursor_shape: CursorShape,
    scroll: usize,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x1: usize,
    clip_y1: usize,
    images: &[PlacedImage],
    scrolled_lines: u64,
    paint_rows: Option<&[usize]>,
) {
    let cursor = screen.cursor();
    // Live caret only — history view hides the host caret (nested parity).
    // Default is a filled block (Ghostty). DECSCUSR selects underline/bar.
    if !cursor_visible
        || scroll != 0
        || paint_rows.is_some_and(|selected| selected.binary_search(&cursor.row).is_err())
    {
        return;
    }
    let (cr, cc) = (cursor.row, cursor.column);
    if cr >= screen.rows()
        || cc >= screen.columns()
        || direct_image_covers_view_cell(images, scrolled_lines, scroll, cr, cc)
    {
        return;
    }
    let x0 = origin_x + cc * font.cell_w;
    let y0 = origin_y + cr * font.cell_h;
    if x0 >= clip_x1 || y0 >= clip_y1 {
        return;
    }
    let cell_w = font.cell_w.min(clip_x1.saturating_sub(x0));
    let cell_h = font.cell_h.min(clip_y1.saturating_sub(y0));
    let caret_bg = theme.cursor_bg.unwrap_or(theme.default_fg);
    let caret_fg = theme.cursor_fg.unwrap_or(theme.default_bg);
    match cursor_shape {
        CursorShape::Block => {
            fill_rect(buffer, stride_px, x0, y0, cell_w, cell_h, caret_bg);
            let cell = screen.view_cell(scroll, cr, cc);
            if !cell.wide_cont {
                let mut grapheme = String::new();
                cell.write_grapheme_into(&mut grapheme);
                let ch_draw = grapheme.chars().next().unwrap_or(cell.character);
                if ch_draw != ' '
                    && ch_draw != '\0'
                    && ch_draw != KITTY_PLACEHOLDER
                    && ch_draw != MAIL_LETTER_GLYPH
                {
                    blit_cluster_in(
                        buffer, stride_px, font, &grapheme, x0, y0, caret_fg, origin_x, clip_x1,
                        false,
                    );
                }
            }
        }
        CursorShape::Underline => {
            let bar_h = (font.cell_h / 8).max(2);
            fill_rect(
                buffer,
                stride_px,
                x0,
                y0 + font.cell_h.saturating_sub(bar_h),
                cell_w,
                bar_h.min(cell_h),
                caret_bg,
            );
        }
        CursorShape::Bar => {
            let bar_w = (font.cell_w / 8).max(2).min(cell_w);
            fill_rect(buffer, stride_px, x0, y0, bar_w, cell_h, caret_bg);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunCell {
    style: Style,
    selected: bool,
    cursor: bool,
    wide_cont: bool,
    special: bool,
    multi_scalar: bool,
    sprite: bool,
    primary_covered: bool,
}

/// Split one screen row into eligible, style-consistent shaping runs.
///
/// The input is precomputed from terminal cells. Keeping this function free
/// of screen and renderer state makes semantic boundaries easy to test.
fn split_runs(cells: &[RunCell]) -> Vec<Run> {
    let eligible = |cell: &RunCell| {
        !cell.selected
            && !cell.cursor
            && !cell.wide_cont
            && !cell.special
            && !cell.multi_scalar
            && !cell.sprite
            && cell.primary_covered
    };
    let mut runs = Vec::new();
    let mut index = 0;
    while index < cells.len() {
        if !eligible(&cells[index]) {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < cells.len()
            && eligible(&cells[index])
            && cells[index].style == cells[start].style
        {
            index += 1;
        }
        runs.push(Run {
            start,
            len: index - start,
        });
    }
    runs
}

#[allow(clippy::too_many_arguments)]
fn rasterize_ligature_rows(
    theme: &Theme,
    screen: &Screen,
    font: &FontMetrics,
    scroll: usize,
    selection: Option<CellRange>,
    cursor: prismattyc_core::Cursor,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x1: usize,
    clip_y1: usize,
    features: &[String],
    paint: ScreenPaint,
    images: &[PlacedImage],
    scrolled_lines: u64,
    paint_rows: Option<&[usize]>,
) {
    let cols = screen.columns();
    let rows = screen.rows();
    let cw = font.cell_w;
    let ch = font.cell_h;
    for row in 0..rows {
        if paint_rows.is_some_and(|selected| selected.binary_search(&row).is_err()) {
            continue;
        }
        let mut metadata = Vec::with_capacity(cols);
        let mut graphemes = Vec::with_capacity(cols);
        for col in 0..cols {
            let covered = direct_image_covers_view_cell(images, scrolled_lines, scroll, row, col);
            let cell = screen.view_cell(scroll, row, col);
            let selected = selection
                .is_some_and(|range| screen.selection_covers_abs_at_view(scroll, range, row, col));
            let (mut fg, mut bg) = resolve_style_colors_with_theme(theme, &cell.style);
            if selected {
                if let (Some(selection_fg), Some(selection_bg)) =
                    (theme.selection_fg, theme.selection_bg)
                {
                    fg = selection_fg;
                    bg = selection_bg;
                } else {
                    std::mem::swap(&mut fg, &mut bg);
                }
            }
            let x0 = origin_x + col * cw;
            let y0 = origin_y + row * ch;
            let mut grapheme = String::new();
            cell.write_grapheme_into(&mut grapheme);
            let ch_draw = grapheme.chars().next().unwrap_or(cell.character);
            let is_placeholder = cell.character == KITTY_PLACEHOLDER;
            let span = if col + 1 < cols && screen.view_cell(scroll, row, col + 1).wide_cont {
                2
            } else {
                1
            };
            if x0 < clip_x1 && y0 < clip_y1 {
                if covered {
                    let cell_w = cw.saturating_mul(span).min(clip_x1.saturating_sub(x0));
                    let cell_h = ch.min(clip_y1.saturating_sub(y0));
                    paint_image_covered_cell_background(
                        theme, cell.style, ch_draw, buffer, stride_px, x0, y0, cell_w, cell_h,
                        paint,
                    );
                    metadata.push(RunCell {
                        style: cell.style,
                        selected,
                        cursor: cursor.row == row && cursor.column == col,
                        wide_cont: cell.wide_cont,
                        special: true,
                        multi_scalar: grapheme.chars().count() != 1,
                        sprite: is_sprite(ch_draw),
                        primary_covered: font.primary_covers(ch_draw),
                    });
                    graphemes.push(grapheme);
                    continue;
                }
                let cell_bg = if ch_draw == MAIL_LETTER_GLYPH {
                    theme.default_bg
                } else {
                    bg
                };
                let cell_w = cw.saturating_mul(span).min(clip_x1.saturating_sub(x0));
                let cell_h = ch.min(clip_y1.saturating_sub(y0));
                let default_bg_cell =
                    cell.style.background == Color::Default && !cell.style.inverse && !selected;
                // The envelope keeps its theme pad over a background image
                // (never skipped) but carries the window alpha like every
                // other default-bg cell, so it is not an opaque square (PT-206).
                let skip_bg =
                    paint.skip_default_bg && default_bg_cell && ch_draw != MAIL_LETTER_GLYPH;
                paint_cell_background(
                    buffer,
                    stride_px,
                    x0,
                    y0,
                    cell_w,
                    cell_h,
                    cell_bg,
                    if default_bg_cell {
                        paint.default_bg_alpha
                    } else {
                        OPAQUE_ALPHA
                    },
                    skip_bg,
                    selected,
                    paint.bg_weight,
                );
                if !is_placeholder {
                    let decoration = underline_style(&cell.style);
                    if decoration != UnderlineStyle::None {
                        draw_underline(
                            buffer,
                            stride_px,
                            x0,
                            y0,
                            cell_w,
                            ch,
                            clip_y1,
                            decoration,
                            resolve_underline_color_with_theme(theme, &cell.style, fg),
                        );
                    }
                }
            }
            metadata.push(RunCell {
                style: cell.style,
                selected,
                cursor: cursor.row == row && cursor.column == col,
                wide_cont: cell.wide_cont,
                special: is_placeholder
                    || ch_draw == MAIL_LETTER_GLYPH
                    || ch_draw == '\0'
                    || covered,
                multi_scalar: grapheme.chars().count() != 1,
                sprite: is_sprite(ch_draw),
                primary_covered: font.primary_covers(ch_draw),
            });
            graphemes.push(grapheme);
        }

        let runs = split_runs(&metadata);
        let mut run_index = 0;
        let mut col = 0;
        while col < cols {
            let run = runs.get(run_index).copied();
            if run.is_some_and(|run| run.start == col) {
                let run = run.unwrap();
                if run.len >= 2 {
                    let text: String = graphemes[run.start..run.start + run.len].concat();
                    let (fg, _) = resolve_style_colors_with_theme(theme, &metadata[col].style);
                    let shaped = paint_shaped_run(
                        buffer,
                        stride_px,
                        font,
                        &text,
                        run,
                        fg,
                        origin_x,
                        origin_y + row * ch,
                        clip_x1,
                        clip_y1,
                        features,
                    );
                    if shaped {
                        col += run.len;
                        run_index += 1;
                        continue;
                    }
                }
                run_index += 1;
            }
            let cell = screen.view_cell(scroll, row, col);
            if !cell.wide_cont
                && !direct_image_covers_view_cell(images, scrolled_lines, scroll, row, col)
            {
                let (mut fg, mut bg) = resolve_style_colors_with_theme(theme, &cell.style);
                if metadata[col].selected {
                    if let (Some(selection_fg), Some(selection_bg)) =
                        (theme.selection_fg, theme.selection_bg)
                    {
                        fg = selection_fg;
                        bg = selection_bg;
                    } else {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                }
                let ch_draw = graphemes[col].chars().next().unwrap_or(cell.character);
                if ch_draw != ' ' && ch_draw != '\0' && ch_draw != KITTY_PLACEHOLDER {
                    blit_cluster_in(
                        buffer,
                        stride_px,
                        font,
                        &graphemes[col],
                        origin_x + col * cw,
                        origin_y + row * ch,
                        fg,
                        origin_x,
                        clip_x1,
                        ch_draw == MAIL_LETTER_GLYPH,
                    );
                }
                let _ = bg;
            }
            col += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_shaped_run(
    buffer: &mut [u32],
    stride: usize,
    font: &FontMetrics,
    text: &str,
    run: Run,
    fg: [u8; 3],
    origin_x: usize,
    y0: usize,
    clip_x1: usize,
    clip_y1: usize,
    feature_tags: &[String],
) -> bool {
    let Some(face) = font.primary_face() else {
        return false;
    };
    let parsed: Vec<rustybuzz::Feature> = feature_tags
        .iter()
        .filter_map(|tag| tag.parse().ok())
        .collect();
    let mut buffer_in = rustybuzz::UnicodeBuffer::new();
    buffer_in.push_str(text);
    buffer_in.guess_segment_properties();
    let shaped = rustybuzz::shape(&face, &parsed, buffer_in);
    let infos = shaped.glyph_infos();
    let positions = shaped.glyph_positions();
    if infos.is_empty() {
        return false;
    }
    let run_x0 = origin_x + run.start * font.cell_w;
    let run_x1 = origin_x
        .saturating_add((run.start + run.len) * font.cell_w)
        .min(clip_x1);
    for (info, position) in infos.iter().zip(positions.iter()) {
        let _ = position;
        let glyph_id = usize::try_from(info.glyph_id).unwrap_or(0);
        if glyph_id == 0 {
            continue;
        }
        let (metrics, bitmap) = font.rasterize_primary_indexed(glyph_id, font.px);
        if metrics.width == 0 || metrics.height == 0 || bitmap.is_empty() {
            continue;
        }
        let cluster = (info.cluster as usize).min(text.len());
        let local_cell = text[..cluster]
            .chars()
            .count()
            .min(run.len.saturating_sub(1));
        let cell_x = origin_x + (run.start + local_cell) * font.cell_w;
        let mut ox = cell_x as i32 + metrics.xmin;
        let mut oy =
            y0 as i32 + font.baseline.round() as i32 - metrics.ymin - metrics.height as i32;
        (ox, oy) = shift_ink_into_clip(
            ox,
            oy,
            metrics.width as i32,
            metrics.height as i32,
            [run_x0 as i32, y0 as i32, run_x1 as i32, clip_y1 as i32],
        );
        blit_coverage(
            buffer,
            stride,
            &bitmap,
            metrics.width,
            metrics.height,
            ox,
            oy,
            fg,
            run_x0 as i32,
            y0 as i32,
            run_x1 as i32,
            clip_y1 as i32,
        );
    }
    true
}

fn preedit_char_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text.char_indices()
        .take_while(|(index, _)| *index <= offset)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0)
}

fn preedit_display_cells(text: &str, offset: usize) -> usize {
    let boundary = preedit_char_boundary(text, offset);
    text[..boundary]
        .chars()
        .map(|ch| prismattyc_core::char_display_width(ch).max(1))
        .sum()
}

/// Paint the active input-method preedit at the terminal cursor.
///
/// The IME supplies cursor offsets as UTF-8 byte offsets. The whole preedit
/// is underlined. The selected cursor range uses inverse terminal colors.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_preedit_at(
    theme: &Theme,
    font: &FontMetrics,
    text: &str,
    cursor: Option<(usize, usize)>,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
) {
    if text.is_empty() || font.cell_w == 0 || font.cell_h == 0 || clip_w == 0 || clip_h == 0 {
        return;
    }
    let clip_right = origin_x.saturating_add(clip_w);
    let clip_bottom = origin_y.saturating_add(clip_h);
    draw_theme_text(
        buffer,
        stride_px,
        font,
        text,
        origin_x,
        origin_y,
        theme.default_fg,
        clip_right,
    );

    let mut x = origin_x;
    for ch in text.chars() {
        let cells = prismattyc_core::char_display_width(ch).max(1);
        let width = font.cell_w.saturating_mul(cells);
        if x >= clip_right {
            break;
        }
        draw_underline(
            buffer,
            stride_px,
            x,
            origin_y,
            width.min(clip_right.saturating_sub(x)),
            font.cell_h,
            clip_bottom,
            UnderlineStyle::Single,
            theme.default_fg,
        );
        x = x.saturating_add(width);
    }

    let Some((start, end)) = cursor else {
        return;
    };
    let start = preedit_char_boundary(text, start);
    let end = preedit_char_boundary(text, end);
    let (start, end) = (start.min(end), start.max(end));
    if start == end {
        return;
    }
    let start_cells = preedit_display_cells(text, start);
    let end_cells = preedit_display_cells(text, end);
    let range_x = origin_x.saturating_add(start_cells.saturating_mul(font.cell_w));
    let range_w = end_cells
        .saturating_sub(start_cells)
        .saturating_mul(font.cell_w)
        .min(clip_right.saturating_sub(range_x));
    if range_w == 0 || range_x >= clip_right {
        return;
    }
    let range_h = font.cell_h.min(clip_bottom.saturating_sub(origin_y));
    fill_rect(
        buffer,
        stride_px,
        range_x,
        origin_y,
        range_w,
        range_h,
        theme.default_fg,
    );
    if let Some(selected) = text.get(start..end) {
        draw_theme_text(
            buffer,
            stride_px,
            font,
            selected,
            range_x,
            origin_y,
            theme.default_bg,
            clip_right,
        );
    }

    // Restore the underline in inverse ink over the selected range. This
    // keeps the IME decoration visible when the cursor range covers text.
    let mut x = origin_x;
    let mut byte: usize = 0;
    for ch in text.chars() {
        let next = byte.saturating_add(ch.len_utf8());
        let cells = prismattyc_core::char_display_width(ch).max(1);
        let width = font.cell_w.saturating_mul(cells);
        if x >= clip_right {
            break;
        }
        let color = if byte >= start && byte < end {
            theme.default_bg
        } else {
            theme.default_fg
        };
        draw_underline(
            buffer,
            stride_px,
            x,
            origin_y,
            width.min(clip_right.saturating_sub(x)),
            font.cell_h,
            clip_bottom,
            UnderlineStyle::Single,
            color,
        );
        x = x.saturating_add(width);
        byte = next;
    }
}

/// Paint cell-rect overlays at z1, clipped to the pane content box.
///
/// Cells outside `clip_*` or the screen grid are skipped so an attachment
/// cannot paint into a sibling pane or host chrome (hybrid-rendering invariant 7).
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn rasterize_overlays_at(
    overlays: &[CellRectOverlay],
    font: &FontMetrics,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x: usize,
    clip_y: usize,
    clip_w: usize,
    clip_h: usize,
    screen_cols: usize,
    screen_rows: usize,
) {
    rasterize_overlays_at_with_theme(
        default_theme(),
        overlays,
        font,
        buffer,
        stride_px,
        origin_x,
        origin_y,
        clip_x,
        clip_y,
        clip_w,
        clip_h,
        screen_cols,
        screen_rows,
    );
}

/// Themed live-host variant of [`rasterize_overlays_at`].
#[allow(clippy::too_many_arguments)]
pub fn rasterize_overlays_at_with_theme(
    theme: &Theme,
    overlays: &[CellRectOverlay],
    font: &FontMetrics,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_x: usize,
    clip_y: usize,
    clip_w: usize,
    clip_h: usize,
    screen_cols: usize,
    screen_rows: usize,
) {
    if overlays.is_empty() || clip_w == 0 || clip_h == 0 || screen_cols == 0 || screen_rows == 0 {
        return;
    }
    let cw = font.cell_w;
    let ch = font.cell_h;
    for overlay in overlays {
        if overlay.rows == 0 || overlay.cols == 0 {
            continue;
        }
        let cells = overlay_cells_with_theme(theme, overlay);
        let mut cell_i = 0usize;
        if overlay.row < 0 {
            let skipped = overlay.row.unsigned_abs() as usize;
            cell_i = skipped.saturating_mul(overlay.cols);
        }
        for row_offset in 0..overlay.rows {
            let absolute = overlay
                .row
                .saturating_add(i32::try_from(row_offset).unwrap_or(0));
            if absolute < 0 {
                continue;
            }
            let row = absolute as usize;
            if row >= screen_rows {
                break;
            }
            for col_offset in 0..overlay.cols {
                let (ch_draw, fg, bg, decoration) = cells.get(cell_i).copied().unwrap_or((
                    ' ',
                    theme.default_fg,
                    theme.overlay_bg,
                    UnderlineStyle::None,
                ));
                cell_i = cell_i.saturating_add(1);
                let col = overlay.col.saturating_add(col_offset);
                if col >= screen_cols {
                    continue;
                }
                let x0 = origin_x.saturating_add(col.saturating_mul(cw));
                let y0 = origin_y.saturating_add(row.saturating_mul(ch));
                let Some((fx, fy, fw, fh)) =
                    intersect_rect((x0, y0, cw, ch), (clip_x, clip_y, clip_w, clip_h))
                else {
                    continue;
                };
                fill_rect(buffer, stride_px, fx, fy, fw, fh, bg);
                if fw == cw && fh == ch && ch_draw != ' ' && ch_draw != '\0' {
                    blit_glyph(buffer, stride_px, font, ch_draw, x0, y0, fg);
                }
                if decoration != UnderlineStyle::None && fw == cw && fh == ch {
                    draw_underline(
                        buffer,
                        stride_px,
                        x0,
                        y0,
                        cw,
                        ch,
                        clip_y.saturating_add(clip_h),
                        decoration,
                        fg,
                    );
                }
            }
        }
    }
}

fn overlay_cells_with_theme(
    theme: &Theme,
    overlay: &CellRectOverlay,
) -> Vec<(char, [u8; 3], [u8; 3], UnderlineStyle)> {
    if overlay.runs.is_empty() {
        return overlay
            .text
            .chars()
            .map(|ch| (ch, theme.default_fg, theme.overlay_bg, UnderlineStyle::None))
            .collect();
    }
    let mut out = Vec::new();
    for run in &overlay.runs {
        let style = Style {
            bold: run.bold,
            italic: run.italic,
            underline: run.underline,
            underline_style: if run.underline {
                UnderlineStyle::Single
            } else {
                UnderlineStyle::None
            },
            inverse: run.inverse,
            foreground: run.fg.map(Color::Indexed).unwrap_or(Color::Default),
            background: run.bg.map(Color::Indexed).unwrap_or(Color::Default),
            ..Style::default()
        };
        let (mut fg, mut bg) = resolve_style_colors_with_theme(theme, &style);
        if run.bg.is_none() && !run.inverse {
            bg = theme.overlay_bg;
        }
        if run.fg.is_none() && !run.inverse {
            fg = theme.default_fg;
        }
        for ch in run.text.chars() {
            out.push((ch, fg, bg, underline_style(&style)));
        }
    }
    out
}

fn intersect_rect(
    (x, y, w, h): (usize, usize, usize, usize),
    (cx, cy, cw, ch): (usize, usize, usize, usize),
) -> Option<(usize, usize, usize, usize)> {
    let x0 = x.max(cx);
    let y0 = y.max(cy);
    let x1 = x.saturating_add(w).min(cx.saturating_add(cw));
    let y1 = y.saturating_add(h).min(cy.saturating_add(ch));
    if x0 < x1 && y0 < y1 {
        Some((x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

/// Width of each brand-spectrum swatch on the chord rail (pixels).
pub const FOCUS_SWATCH_PX: usize = 5;

/// Paint a temporary chord-help strip along the **bottom** of the window buffer.
///
/// - Bar background matches the current **focus border** brand color.
/// - Right edge: rainbow of all spectrum colors as [`FOCUS_SWATCH_PX`]-wide blocks.
/// - Does not reserve a terminal cell row (overlay while Ctrl+Shift is held).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_footer(
    font: &FontMetrics,
    text: &str,
    buffer: &mut [u32],
    stride_px: usize,
    buffer_height_px: usize,
    rows: usize,
    focus_rgb: [u8; 3],
    bg_alpha: u8,
) {
    if rows == 0 || stride_px == 0 || buffer_height_px == 0 {
        return;
    }
    let bar_h = font.cell_h.saturating_mul(rows).min(buffer_height_px);
    let y0 = buffer_height_px.saturating_sub(bar_h);
    fill_rect_argb(
        buffer, stride_px, 0, y0, stride_px, bar_h, focus_rgb, bg_alpha,
    );

    // Spectrum swatches on the right end of the rail.
    let swatch_n = FOCUS_BORDER_PALETTE.len();
    let rainbow_w = swatch_n.saturating_mul(FOCUS_SWATCH_PX);
    let rainbow_x0 = stride_px.saturating_sub(rainbow_w.saturating_add(2));
    for (i, (_name, rgb)) in FOCUS_BORDER_PALETTE.iter().enumerate() {
        let sx = rainbow_x0.saturating_add(i.saturating_mul(FOCUS_SWATCH_PX));
        if sx >= stride_px {
            break;
        }
        let w = FOCUS_SWATCH_PX.min(stride_px.saturating_sub(sx));
        fill_rect(buffer, stride_px, sx, y0, w, bar_h, *rgb);
    }
    // 1px separator between text and rainbow.
    if rainbow_x0 > 0 {
        fill_rect(
            buffer,
            stride_px,
            rainbow_x0.saturating_sub(1),
            y0,
            1,
            bar_h,
            contrast_ink(focus_rgb),
        );
    }

    let ink = contrast_ink(focus_rgb);
    let text_limit = rainbow_x0.saturating_sub(font.cell_w.max(4));
    let mut x = font.cell_w / 2;
    for ch in text.chars() {
        if x.saturating_add(font.cell_w) > text_limit {
            break;
        }
        blit_glyph(buffer, stride_px, font, ch, x, y0, ink);
        x = x.saturating_add(font.cell_w);
    }
}

/// Overlay width of the host scrollback scrollbar (PT-40).
pub const SCROLLBAR_WIDTH_PX: usize = 8;
/// Minimum thumb height so the grip stays hittable.
pub const SCROLLBAR_MIN_THUMB_PX: usize = 16;

/// Pixel layout of a pane scrollback scrollbar. `scroll == 0` is live tail
/// (thumb at the bottom). `scroll == max_scroll` is oldest history (thumb at
/// the top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarLayout {
    pub track_x: usize,
    pub track_y: usize,
    pub track_w: usize,
    pub track_h: usize,
    pub thumb_y: usize,
    pub thumb_h: usize,
}

impl ScrollbarLayout {
    pub fn contains(self, px: usize, py: usize) -> bool {
        px >= self.track_x
            && px < self.track_x.saturating_add(self.track_w)
            && py >= self.track_y
            && py < self.track_y.saturating_add(self.track_h)
    }

    pub fn thumb_contains(self, py: usize) -> bool {
        py >= self.thumb_y && py < self.thumb_y.saturating_add(self.thumb_h)
    }

    pub fn travel(self) -> usize {
        self.track_h.saturating_sub(self.thumb_h)
    }
}

/// Right-edge overlay bar. `None` when there is no history to scroll.
pub fn scrollbar_layout(
    content_x: usize,
    content_y: usize,
    content_w: usize,
    content_h: usize,
    scroll: usize,
    max_scroll: usize,
    view_rows: usize,
) -> Option<ScrollbarLayout> {
    if max_scroll == 0 || content_h < SCROLLBAR_MIN_THUMB_PX || content_w < SCROLLBAR_WIDTH_PX {
        return None;
    }
    let track_w = SCROLLBAR_WIDTH_PX.min(content_w);
    let track_x = content_x.saturating_add(content_w.saturating_sub(track_w));
    let track_y = content_y;
    let track_h = content_h;
    let view_rows = view_rows.max(1);
    let total = view_rows.saturating_add(max_scroll);
    let thumb_h = track_h
        .saturating_mul(view_rows)
        .checked_div(total)
        .unwrap_or(track_h)
        .max(SCROLLBAR_MIN_THUMB_PX)
        .min(track_h);
    let travel = track_h.saturating_sub(thumb_h);
    let scroll = scroll.min(max_scroll);
    let thumb_y = if travel == 0 {
        track_y
    } else {
        track_y + travel - travel.saturating_mul(scroll) / max_scroll
    };
    Some(ScrollbarLayout {
        track_x,
        track_y,
        track_w,
        track_h,
        thumb_y,
        thumb_h,
    })
}

/// Map a thumb top edge to a `view_scroll` value.
pub fn scrollbar_scroll_from_thumb_y(
    layout: ScrollbarLayout,
    thumb_y: usize,
    max_scroll: usize,
) -> usize {
    let travel = layout.travel();
    if max_scroll == 0 || travel == 0 {
        return 0;
    }
    let rel = thumb_y.saturating_sub(layout.track_y).min(travel);
    max_scroll.saturating_mul(travel.saturating_sub(rel)) / travel
}

/// Thumb top so `py` is the grab point (`grab_off` = pointer − thumb_y at press).
pub fn scrollbar_thumb_y_for_pointer(layout: ScrollbarLayout, py: usize, grab_off: i32) -> usize {
    let travel = layout.travel();
    let raw = py as i32 - grab_off;
    let min = layout.track_y as i32;
    let max = (layout.track_y + travel) as i32;
    raw.clamp(min, max) as usize
}

#[allow(clippy::too_many_arguments)]
pub fn rasterize_scrollbar(
    layout: ScrollbarLayout,
    buffer: &mut [u32],
    stride_px: usize,
    track: [u8; 3],
    thumb: [u8; 3],
) {
    fill_rect(
        buffer,
        stride_px,
        layout.track_x,
        layout.track_y,
        layout.track_w,
        layout.track_h,
        track,
    );
    fill_rect(
        buffer,
        stride_px,
        layout.track_x,
        layout.thumb_y,
        layout.track_w,
        layout.thumb_h,
        thumb,
    );
}

pub(crate) fn mix_rgb(bg: [u8; 3], fg: [u8; 3], fg_weight: u16) -> [u8; 3] {
    let bg_weight = 256u16.saturating_sub(fg_weight);
    [
        ((u16::from(bg[0]) * bg_weight + u16::from(fg[0]) * fg_weight) / 256) as u8,
        ((u16::from(bg[1]) * bg_weight + u16::from(fg[1]) * fg_weight) / 256) as u8,
        ((u16::from(bg[2]) * bg_weight + u16::from(fg[2]) * fg_weight) / 256) as u8,
    ]
}

/// Bottom-right inverse ` N/M ` chip while a pane is in history view (
/// nested parity). Host-only overlay; does not mutate the child grid.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_scroll_chip(
    font: &FontMetrics,
    label: &str,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    bg: [u8; 3],
    fg: [u8; 3],
) {
    if clip_w == 0 || clip_h == 0 || font.cell_w == 0 {
        return;
    }
    let cols = label.chars().count().max(1);
    let chip_w = cols.saturating_mul(font.cell_w).min(clip_w);
    let chip_h = font.cell_h.min(clip_h);
    let x0 = origin_x.saturating_add(clip_w.saturating_sub(chip_w));
    let y0 = origin_y.saturating_add(clip_h.saturating_sub(chip_h));
    fill_rect(buffer, stride_px, x0, y0, chip_w, chip_h, bg);
    let mut x = x0;
    for ch in label.chars() {
        if x.saturating_add(font.cell_w) > x0 + chip_w {
            break;
        }
        blit_glyph(buffer, stride_px, font, ch, x, y0, fg);
        x = x.saturating_add(font.cell_w);
    }
}

/// Top-right bell toast (PT-39), mirroring the pmux-attach viewport HUD:
/// one row, focus-border fill, contrast ink text.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_bell_toast(
    font: &FontMetrics,
    label: &str,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    bg: [u8; 3],
    fg: [u8; 3],
) {
    if clip_w == 0 || clip_h == 0 || font.cell_w == 0 {
        return;
    }
    let cols = label.chars().count().max(1);
    let chip_w = cols.saturating_mul(font.cell_w).min(clip_w);
    let chip_h = font.cell_h.min(clip_h);
    let x0 = origin_x.saturating_add(clip_w.saturating_sub(chip_w));
    fill_rect(buffer, stride_px, x0, origin_y, chip_w, chip_h, bg);
    let mut x = x0;
    for ch in label.chars() {
        if x.saturating_add(font.cell_w) > x0 + chip_w {
            break;
        }
        blit_glyph(buffer, stride_px, font, ch, x, origin_y, fg);
        x = x.saturating_add(font.cell_w);
    }
}

/// Bottom-left inverse find prompt. `reserve_right_px` leaves room for the
/// scroll chip so the two overlays do not overlap.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_find_prompt(
    font: &FontMetrics,
    label: &str,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
    reserve_right_px: usize,
    bg: [u8; 3],
    fg: [u8; 3],
) {
    if clip_w == 0 || clip_h == 0 || font.cell_w == 0 {
        return;
    }
    let field_w = clip_w.saturating_sub(reserve_right_px).max(1);
    let cols = (field_w / font.cell_w).max(1);
    let mut text: String = label.chars().take(cols).collect();
    while text.chars().count() < cols {
        text.push(' ');
    }
    let bar_w = cols.saturating_mul(font.cell_w).min(field_w);
    let bar_h = font.cell_h.min(clip_h);
    let y0 = origin_y.saturating_add(clip_h.saturating_sub(bar_h));
    fill_rect(buffer, stride_px, origin_x, y0, bar_w, bar_h, bg);
    let mut x = origin_x;
    for ch in text.chars() {
        if x.saturating_add(font.cell_w) > origin_x + bar_w {
            break;
        }
        blit_glyph(buffer, stride_px, font, ch, x, y0, fg);
        x = x.saturating_add(font.cell_w);
    }
}

/// Subtitle-style walkthrough caption (PT-193). Alpha-blended band; text
/// has a 1-px shadow. Does not change pane geometry.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_walkthrough_caption(
    font: &FontMetrics,
    view: &crate::walkthrough::CaptionView,
    band: crate::walkthrough::CaptionBand,
    buffer: &mut [u32],
    stride_px: usize,
    bg: [u8; 3],
    fg: [u8; 3],
) {
    if band.w == 0 || band.h == 0 || font.cell_w == 0 {
        return;
    }
    fill_rect_argb(
        buffer,
        stride_px,
        band.x,
        band.y,
        band.w,
        band.h,
        bg,
        crate::walkthrough::CAPTION_ALPHA,
    );
    let shadow = mix_rgb(bg, [0, 0, 0], 200);
    let pad_x = font.cell_w;
    let pad_y = font.cell_h / 4 + 1;
    let paint_line = |buffer: &mut [u32], text: &str, x0: usize, y0: usize| {
        let mut x = x0;
        for ch in text.chars() {
            if x.saturating_add(font.cell_w) > band.x + band.w {
                break;
            }
            blit_glyph(
                buffer,
                stride_px,
                font,
                ch,
                x.saturating_add(1),
                y0.saturating_add(1),
                shadow,
            );
            blit_glyph(buffer, stride_px, font, ch, x, y0, fg);
            x = x.saturating_add(font.cell_w);
        }
    };
    paint_line(buffer, &view.caption, band.x + pad_x, band.y + pad_y);
    paint_line(
        buffer,
        crate::walkthrough::DISMISS_LABEL,
        band.dismiss.x,
        band.dismiss.y,
    );
    let line2_right = band
        .show_me
        .or(band.skip)
        .map(|rect| rect.x)
        .unwrap_or(band.x.saturating_add(band.w));
    let mut x = band.x + pad_x;
    let line2_y = band.y + pad_y + font.cell_h;
    for ch in view.line2.chars() {
        if x.saturating_add(font.cell_w) > line2_right {
            break;
        }
        blit_glyph(
            buffer,
            stride_px,
            font,
            ch,
            x.saturating_add(1),
            line2_y.saturating_add(1),
            shadow,
        );
        blit_glyph(buffer, stride_px, font, ch, x, line2_y, fg);
        x = x.saturating_add(font.cell_w);
    }
    if let Some(rect) = band.show_me {
        paint_line(buffer, crate::walkthrough::SHOW_ME_LABEL, rect.x, rect.y);
    }
    if let Some(rect) = band.skip {
        paint_line(buffer, crate::walkthrough::SKIP_LABEL, rect.x, rect.y);
    }
}

/// Compact name column (80×24 and the old default).
pub const PALETTE_NAME_CELLS: usize = 19;
/// Name column at the full 120-cell panel.
pub const PALETTE_NAME_CELLS_FULL: usize = 30;
/// Compact panel cap (today's layout).
pub const PALETTE_COMPACT_MAX_CELLS: usize = 90;
/// Widest comfortable panel, in cells.
pub const PALETTE_MAX_CELLS: usize = 120;
/// Compact list-line cap.
pub const PALETTE_LIST_LINES: usize = 14;
/// Inner inset on each side when the panel is not compact (PT-201).
pub const PALETTE_INSET_CELLS: usize = 2;
/// Extra pixels between list rows when the panel is not compact.
pub const PALETTE_ROW_GAP_PX: usize = 6;
/// Extra pixels on the query row when the panel is not compact.
pub const PALETTE_QUERY_EXTRA_PX: usize = 8;

/// Palette panel geometry. No `HostState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteGeom {
    pub width_cells: usize,
    pub inset_cells: usize,
    /// Name column x, in cells from the inner text origin.
    pub name_x: usize,
    pub name_cells: usize,
    pub row_pitch_px: usize,
    pub query_h_px: usize,
    pub chord_gap_cells: usize,
    pub compact: bool,
}

/// Choose palette width, inset, name column, and row pitch.
///
/// A window of 80×24 or smaller keeps today's compact layout. A larger
/// window grows to `min(120 cells, 85 % of the window)` with a 2-cell
/// inset and a 30-cell name column at full width.
pub fn palette_geom(window_cols: usize, window_rows: usize, cell_h: usize) -> PaletteGeom {
    let compact = window_cols <= 80 || window_rows <= 24;
    if compact {
        return PaletteGeom {
            width_cells: window_cols
                .saturating_sub(4)
                .clamp(20, PALETTE_COMPACT_MAX_CELLS),
            inset_cells: 1,
            name_x: 2,
            name_cells: PALETTE_NAME_CELLS,
            row_pitch_px: cell_h.max(1),
            query_h_px: cell_h.max(1),
            chord_gap_cells: 1,
            compact: true,
        };
    }
    let pct = window_cols.saturating_mul(85) / 100;
    let compact_width = window_cols.saturating_sub(4).min(PALETTE_COMPACT_MAX_CELLS);
    let width_cells = pct
        .max(compact_width)
        .min(PALETTE_MAX_CELLS)
        .min(window_cols.saturating_sub(4));
    let span = PALETTE_MAX_CELLS.saturating_sub(24).max(1);
    let t = width_cells.saturating_sub(24);
    let name_cells = PALETTE_NAME_CELLS + (PALETTE_NAME_CELLS_FULL - PALETTE_NAME_CELLS) * t / span;
    PaletteGeom {
        width_cells,
        inset_cells: PALETTE_INSET_CELLS,
        name_x: 0,
        name_cells: name_cells.clamp(PALETTE_NAME_CELLS, PALETTE_NAME_CELLS_FULL),
        row_pitch_px: cell_h.saturating_add(PALETTE_ROW_GAP_PX).max(1),
        query_h_px: cell_h.saturating_add(PALETTE_QUERY_EXTRA_PX).max(1),
        chord_gap_cells: 2,
        compact: false,
    }
}

/// One list section of the palette panel (RECENT, MATCHES, a picker list).
pub struct PaletteSection<'a> {
    pub header: &'a str,
    /// Muted text after the header (the query under MATCHES).
    pub subtitle: &'a str,
    pub rows: &'a [crate::palette::PaletteRow],
}

/// Surface treatment shared by host-owned overlays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlaySurface {
    /// Opacity of the active pane surface used as the overlay tint weight.
    pub opacity: f32,
    /// In-frame backdrop blur radius. Zero keeps the existing pixels sharp.
    pub blur_radius: u32,
}

impl Default for OverlaySurface {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            blur_radius: 0,
        }
    }
}

impl OverlaySurface {
    /// Build the overlay treatment from the active-pane and window blur state.
    pub fn from_window(opacity: f32, background_blur_px: u32, window_blur_active: bool) -> Self {
        Self {
            opacity: opacity.clamp(0.0, 1.0),
            blur_radius: background_blur_px.max(if window_blur_active { 8 } else { 0 }),
        }
    }

    fn is_opaque(self) -> bool {
        self.opacity >= 1.0 && self.blur_radius == 0
    }
}

/// Everything `rasterize_palette` paints. Pickers that reuse the panel pass
/// no chips and no detail.
pub struct PaletteFrame<'a> {
    /// Optional query row. Context menus omit it so the section header is first.
    pub query: Option<&'a str>,
    /// Chip labels and the selected chip; `None` hides the chip row.
    pub chips: Option<(&'a [&'a str], usize)>,
    pub sections: &'a [PaletteSection<'a>],
    /// Selection index across every section's rows, in order.
    pub selected: usize,
    /// First visible list line (headers and rows). Independent of `selected`
    /// so hover does not jump the window.
    pub scroll: usize,
    pub detail: Option<&'a crate::palette::PaletteDetail>,
    pub footer: &'a str,
}

/// Return how many list lines fit once `fixed_px` (query, chips, detail,
/// footer, padding) are reserved. Compact windows keep the 14-line cap.
pub fn palette_list_lines(
    font: &FontMetrics,
    geom: PaletteGeom,
    fixed_px: usize,
    buffer_height_px: usize,
) -> usize {
    if font.cell_h == 0 || geom.row_pitch_px == 0 {
        return 0;
    }
    let margin_y = font.cell_h.max(4);
    let available = buffer_height_px
        .saturating_sub(margin_y.saturating_mul(2))
        .saturating_sub(fixed_px);
    let lines = available / geom.row_pitch_px;
    if geom.compact {
        lines.min(PALETTE_LIST_LINES)
    } else {
        lines
    }
}

/// Column x offsets (in cells from the panel's text origin) for one row:
/// `(name, describe, describe_max_cells, chord)`. The description never
/// reaches the chord column.
pub fn palette_row_columns(
    width_cells: usize,
    chord_cells: usize,
    name_cells: usize,
    name_x: usize,
    chord_gap: usize,
) -> (usize, usize, usize, usize) {
    let describe_x = name_x + name_cells + 1;
    let chord_x = width_cells.saturating_sub(chord_cells.saturating_add(chord_gap));
    let describe_max = chord_x.saturating_sub(describe_x + chord_gap);
    (name_x, describe_x, describe_max, chord_x)
}

/// One painted data row (not a section header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteLaidRow {
    pub global: usize,
    pub y: usize,
}

/// Panel rectangle and list-row positions. Paint and hit-test share this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteLayout {
    pub geom: PaletteGeom,
    pub panel_x: usize,
    pub panel_y: usize,
    pub panel_w: usize,
    pub panel_h: usize,
    pub start: usize,
    pub shown: usize,
    pub more_line: bool,
    pub rows: Vec<PaletteLaidRow>,
}

/// Keep `scroll` unless `selected_line` left the visible window.
pub fn palette_scroll_for_selection(
    scroll: usize,
    selected_line: usize,
    visible: usize,
    total: usize,
) -> usize {
    let visible = visible.max(1);
    let max_scroll = total.saturating_sub(visible);
    let mut scroll = scroll.min(max_scroll);
    if selected_line < scroll {
        scroll = selected_line;
    } else if selected_line >= scroll.saturating_add(visible) {
        scroll = selected_line.saturating_add(1).saturating_sub(visible);
    }
    scroll.min(max_scroll)
}

/// Map a pointer onto a painted list row. The hit band is the row pitch.
pub fn palette_hit(layout: &PaletteLayout, pointer_x: usize, pointer_y: usize) -> Option<usize> {
    if pointer_x < layout.panel_x
        || pointer_x >= layout.panel_x.saturating_add(layout.panel_w)
        || pointer_y < layout.panel_y
        || pointer_y >= layout.panel_y.saturating_add(layout.panel_h)
    {
        return None;
    }
    layout.rows.iter().find_map(|row| {
        (pointer_y >= row.y && pointer_y < row.y.saturating_add(layout.geom.row_pitch_px))
            .then_some(row.global)
    })
}

fn palette_chrome_px(
    geom: PaletteGeom,
    cell_h: usize,
    has_query: bool,
    has_chips: bool,
    has_detail: bool,
) -> usize {
    if geom.compact {
        let fixed_rows = usize::from(has_query)
            + usize::from(has_chips)
            + usize::from(has_detail)
            + if has_detail { 3 } else { 0 }
            + 1
            + 1;
        return cell_h.saturating_mul(fixed_rows);
    }
    let blank = cell_h;
    let mut px = if has_query {
        geom.query_h_px.saturating_add(blank)
    } else {
        0
    };
    if has_chips {
        px = px.saturating_add(cell_h).saturating_add(blank);
    }
    if has_detail {
        px = px
            .saturating_add(blank)
            .saturating_add(cell_h.saturating_mul(3));
    }
    px.saturating_add(blank)
        .saturating_add(cell_h)
        .saturating_add(cell_h)
}

fn palette_collect_lines(frame: &PaletteFrame<'_>) -> (Vec<PaletteLine>, usize) {
    let mut lines = Vec::new();
    let mut global = 0;
    let mut selected_line = 0;
    for (section_index, section) in frame.sections.iter().enumerate() {
        if section.rows.is_empty() {
            continue;
        }
        lines.push(PaletteLine::Header(section_index));
        for row_index in 0..section.rows.len() {
            if global == frame.selected {
                selected_line = lines.len();
            }
            lines.push(PaletteLine::Row(section_index, row_index));
            global += 1;
        }
    }
    (lines, selected_line)
}

/// Display width of `text` in cells.
fn text_cells(text: &str) -> usize {
    text.chars()
        .map(|ch| prismattyc_core::char_display_width(ch).max(1))
        .sum()
}

/// Cut `text` to `max_cells`, ending in `…` when anything was removed.
pub fn ellipsized(text: &str, max_cells: usize) -> String {
    if text_cells(text) <= max_cells {
        return text.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = prismattyc_core::char_display_width(ch).max(1);
        if used + cells > max_cells - 1 {
            break;
        }
        out.push(ch);
        used += cells;
    }
    out.push('…');
    out
}

/// Wrap `text` into at most `lines` lines of `max_cells`, breaking at spaces;
/// the last line is ellipsized when text remains.
fn wrap_lines(text: &str, max_cells: usize, lines: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() && out.len() < lines {
        if out.len() + 1 == lines || text_cells(rest) <= max_cells {
            out.push(ellipsized(rest, max_cells));
            break;
        }
        let mut cut = 0;
        let mut used = 0;
        for (index, ch) in rest.char_indices() {
            let cells = prismattyc_core::char_display_width(ch).max(1);
            if used + cells > max_cells {
                break;
            }
            used += cells;
            if ch == ' ' {
                cut = index;
            }
        }
        if cut == 0 {
            out.push(ellipsized(rest, max_cells));
            break;
        }
        out.push(rest[..cut].to_owned());
        rest = rest[cut..].trim_start();
    }
    out
}

enum PaletteLine {
    Header(usize),
    Row(usize, usize),
}

fn palette_window(
    font: &FontMetrics,
    stride_px: usize,
    buffer_height_px: usize,
) -> Option<(usize, usize)> {
    if font.cell_w == 0 || font.cell_h == 0 {
        return None;
    }
    Some((stride_px / font.cell_w, buffer_height_px / font.cell_h))
}

/// Compute palette panel geometry and visible list-row positions.
pub fn palette_layout(
    font: &FontMetrics,
    frame: &PaletteFrame<'_>,
    stride_px: usize,
    buffer_height_px: usize,
) -> Option<PaletteLayout> {
    if stride_px < font.cell_w.saturating_mul(24)
        || buffer_height_px < font.cell_h.saturating_mul(8)
    {
        return None;
    }
    let (window_cols, window_rows) = palette_window(font, stride_px, buffer_height_px)?;
    let geom = palette_geom(window_cols, window_rows, font.cell_h);
    if geom.width_cells < 20 {
        return None;
    }
    let has_query = frame.query.is_some();
    let has_chips = frame.chips.is_some();
    let has_detail = frame.detail.is_some();
    let chrome_px = palette_chrome_px(geom, font.cell_h, has_query, has_chips, has_detail);
    let budget = palette_list_lines(font, geom, chrome_px, buffer_height_px);
    let (lines, selected_line) = palette_collect_lines(frame);
    let total = lines.len();
    let more_line = total > budget;
    let shown = if total <= budget {
        total
    } else {
        budget.saturating_sub(1).max(1)
    };
    let start = if total <= budget {
        0
    } else {
        palette_scroll_for_selection(frame.scroll, selected_line, shown, total)
    };
    let list_rows = shown + usize::from(more_line);
    let header_gaps = if geom.compact {
        0
    } else {
        lines
            .iter()
            .skip(start)
            .take(shown)
            .enumerate()
            .filter(|(index, line)| *index > 0 && matches!(line, PaletteLine::Header(_)))
            .count()
    };
    let margin_y = font.cell_h.max(4);
    let width = font.cell_w.saturating_mul(geom.width_cells);
    let list_px = geom
        .row_pitch_px
        .saturating_mul(list_rows)
        .saturating_add(font.cell_h.saturating_mul(header_gaps));
    let height = chrome_px
        .saturating_add(list_px)
        .min(buffer_height_px.saturating_sub(margin_y.saturating_mul(2)));
    if width < font.cell_w.saturating_mul(12) || height < font.cell_h.saturating_mul(4) {
        return None;
    }
    let x = stride_px.saturating_sub(width) / 2;
    let y = buffer_height_px.saturating_sub(height) / 2;
    let blank = if geom.compact { 0 } else { font.cell_h };
    let mut row_y = y.saturating_add(font.cell_h / 2);
    if has_query {
        row_y = row_y.saturating_add(geom.query_h_px);
        row_y = row_y.saturating_add(blank);
    }
    if has_chips {
        row_y = row_y.saturating_add(font.cell_h).saturating_add(blank);
    }
    let mut rows = Vec::new();
    let mut global = 0;
    for line in lines.iter().take(start) {
        if let PaletteLine::Row(..) = line {
            global += 1;
        }
    }
    for (visible, line) in lines.iter().skip(start).take(shown).enumerate() {
        if !geom.compact && visible > 0 && matches!(line, PaletteLine::Header(_)) {
            row_y = row_y.saturating_add(font.cell_h);
        }
        if let PaletteLine::Row(..) = line {
            rows.push(PaletteLaidRow { global, y: row_y });
            global += 1;
        }
        row_y = row_y.saturating_add(geom.row_pitch_px);
    }
    Some(PaletteLayout {
        geom,
        panel_x: x,
        panel_y: y,
        panel_w: width,
        panel_h: height,
        start,
        shown,
        more_line,
        rows,
    })
}

/// Paint the centered command palette above guest content and below the splash.
///
/// Layout, top to bottom: query; chips; per section a muted header with a
/// rule, then its rows (name column, ellipsized description, right-aligned
/// chords); a `▾ N more` / `▴ N more` line when the list scrolls; a blank
/// row; the detail box (`overlay_bg`, 3 rows); the footer in the focus
/// colour. Roomy windows add pitch, inset, and blank lines (PT-201).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_palette(
    font: &FontMetrics,
    frame: &PaletteFrame<'_>,
    theme: &Theme,
    surface: OverlaySurface,
    buffer: &mut [u32],
    stride_px: usize,
    buffer_height_px: usize,
    focus_rgb: [u8; 3],
) -> Option<PaletteLayout> {
    let layout = palette_layout(font, frame, stride_px, buffer_height_px)?;
    let geom = layout.geom;
    let x = layout.panel_x;
    let y = layout.panel_y;
    let width = layout.panel_w;
    let height = layout.panel_h;
    let inner_cells = geom
        .width_cells
        .saturating_sub(geom.inset_cells.saturating_mul(2));
    let (lines, _) = palette_collect_lines(frame);
    let start = layout.start;
    let shown = layout.shown;
    let more_line = layout.more_line;
    let total = lines.len();
    let text_x = x.saturating_add(font.cell_w.saturating_mul(geom.inset_cells));
    let text_right = x
        .saturating_add(width)
        .saturating_sub(font.cell_w.saturating_mul(geom.inset_cells));
    let bottom = y.saturating_add(height);

    let muted = readable_ink(
        mix_rgb(theme.chrome_bg, theme.chrome_fg, 140),
        theme.chrome_bg,
    );
    let selected_bg = theme.selection_bg.unwrap_or(theme.default_fg);
    let selected_fg = readable_ink(theme.selection_fg.unwrap_or(theme.default_bg), selected_bg);

    paint_panel_shadow(buffer, stride_px, x, y, width, height, theme.default_bg);
    paint_panel_border(buffer, stride_px, x, y, width, height, focus_rgb);
    paint_overlay_surface(
        buffer,
        stride_px,
        x.saturating_add(1),
        y.saturating_add(1),
        width.saturating_sub(2),
        height.saturating_sub(2),
        theme.chrome_bg,
        surface,
    );

    let blank = if geom.compact { 0 } else { font.cell_h };
    let mut row_y = y.saturating_add(font.cell_h / 2);
    if let Some(query) = frame.query {
        let query_line = format!("> {}█", query);
        let query_text_y = row_y.saturating_add(geom.query_h_px.saturating_sub(font.cell_h) / 2);
        draw_theme_text(
            buffer,
            stride_px,
            font,
            &query_line,
            text_x,
            query_text_y,
            theme.chrome_fg,
            text_right,
        );
        row_y = row_y.saturating_add(geom.query_h_px).saturating_add(blank);
    }

    if let Some((labels, selected_chip)) = frame.chips {
        let hint = "C-←/→ filter";
        let hint_cells = text_cells(hint);
        let hint_x = text_right.saturating_sub(hint_cells.saturating_mul(font.cell_w));
        let mut chip_x = text_x.saturating_add(font.cell_w);
        for (index, label) in labels.iter().enumerate() {
            let label_cells = text_cells(label);
            let chip_w = label_cells.saturating_add(2).saturating_mul(font.cell_w);
            if chip_x.saturating_add(chip_w).saturating_add(font.cell_w) > hint_x {
                break;
            }
            let ink = if index == selected_chip {
                fill_rect(
                    buffer,
                    stride_px,
                    chip_x,
                    row_y,
                    chip_w,
                    font.cell_h,
                    theme.chrome_fg,
                );
                theme.chrome_bg
            } else {
                muted
            };
            draw_theme_text(
                buffer,
                stride_px,
                font,
                label,
                chip_x.saturating_add(font.cell_w),
                row_y,
                ink,
                hint_x,
            );
            chip_x = chip_x.saturating_add(chip_w).saturating_add(font.cell_w);
        }
        draw_theme_text(
            buffer, stride_px, font, hint, hint_x, row_y, muted, text_right,
        );
        row_y = row_y.saturating_add(font.cell_h).saturating_add(blank);
    }

    let mut global = 0;
    for line in lines.iter().take(start) {
        if let PaletteLine::Row(..) = line {
            global += 1;
        }
    }
    for (visible, line) in lines.iter().skip(start).take(shown).enumerate() {
        if !geom.compact && visible > 0 && matches!(line, PaletteLine::Header(_)) {
            row_y = row_y.saturating_add(font.cell_h);
        }
        if row_y.saturating_add(geom.row_pitch_px) > bottom.saturating_sub(font.cell_h) {
            break;
        }
        let text_y = row_y.saturating_add(geom.row_pitch_px.saturating_sub(font.cell_h) / 2);
        match *line {
            PaletteLine::Header(section_index) => {
                let section = &frame.sections[section_index];
                let header = if section.subtitle.is_empty() {
                    section.header.to_owned()
                } else {
                    format!("{}  {}", section.header, section.subtitle)
                };
                draw_theme_text(
                    buffer,
                    stride_px,
                    font,
                    &header,
                    text_x.saturating_add(font.cell_w.saturating_mul(2)),
                    text_y,
                    muted,
                    text_right,
                );
                let rule_x =
                    text_x.saturating_add(font.cell_w.saturating_mul(text_cells(&header) + 3));
                if rule_x.saturating_add(font.cell_w) < text_right {
                    fill_rect(
                        buffer,
                        stride_px,
                        rule_x,
                        text_y.saturating_add(font.cell_h / 2),
                        text_right.saturating_sub(rule_x),
                        1,
                        mix_rgb(theme.chrome_bg, theme.chrome_fg, 60),
                    );
                }
            }
            PaletteLine::Row(section_index, row_index) => {
                let row = &frame.sections[section_index].rows[row_index];
                let is_selected = global == frame.selected;
                global += 1;
                if is_selected {
                    fill_rect(
                        buffer,
                        stride_px,
                        x.saturating_add(1),
                        row_y,
                        width.saturating_sub(2),
                        geom.row_pitch_px,
                        selected_bg,
                    );
                }
                let ink = if is_selected {
                    selected_fg
                } else {
                    theme.chrome_fg
                };
                let chord_ink = if is_selected { selected_fg } else { muted };
                let chord_cells = text_cells(&row.chord_label);
                let (name_c, desc_c, desc_max, chord_c) = palette_row_columns(
                    inner_cells,
                    chord_cells,
                    geom.name_cells,
                    geom.name_x,
                    geom.chord_gap_cells,
                );
                let name = ellipsized(
                    &row.name,
                    if row.full_width {
                        inner_cells.saturating_sub(name_c)
                    } else {
                        geom.name_cells
                    },
                );
                draw_theme_text(
                    buffer,
                    stride_px,
                    font,
                    &name,
                    text_x.saturating_add(font.cell_w.saturating_mul(name_c)),
                    text_y,
                    ink,
                    text_right,
                );
                let describe = ellipsized(&row.describe, desc_max);
                draw_theme_text(
                    buffer,
                    stride_px,
                    font,
                    &describe,
                    text_x.saturating_add(font.cell_w.saturating_mul(desc_c)),
                    text_y,
                    ink,
                    text_right,
                );
                draw_theme_text(
                    buffer,
                    stride_px,
                    font,
                    &row.chord_label,
                    text_x.saturating_add(font.cell_w.saturating_mul(chord_c)),
                    text_y,
                    chord_ink,
                    text_right,
                );
            }
        }
        row_y = row_y.saturating_add(geom.row_pitch_px);
    }
    if more_line {
        let below = total.saturating_sub(start + shown);
        let text = if below > 0 {
            format!("▾ {below} more")
        } else {
            format!("▴ {start} more")
        };
        draw_theme_text(
            buffer,
            stride_px,
            font,
            &text,
            text_x.saturating_add(font.cell_w.saturating_mul(2)),
            row_y,
            muted,
            text_right,
        );
        row_y = row_y.saturating_add(geom.row_pitch_px);
    }

    if let Some(detail) = frame.detail {
        row_y = row_y.saturating_add(font.cell_h);
        let box_x = x.saturating_add(font.cell_w.saturating_mul(geom.inset_cells) / 2);
        let box_w = width.saturating_sub(font.cell_w.saturating_mul(geom.inset_cells));
        fill_rect(
            buffer,
            stride_px,
            box_x,
            row_y,
            box_w,
            font.cell_h.saturating_mul(3),
            theme.overlay_bg,
        );
        let inner = inner_cells.saturating_sub(2);
        let body = format!("{} — {}", detail.name, detail.text);
        let text_lines = wrap_lines(&body, inner, 2);
        let mut line_y = row_y;
        for (index, line) in text_lines.iter().enumerate() {
            let lx = text_x.saturating_add(font.cell_w);
            draw_theme_text(
                buffer,
                stride_px,
                font,
                line,
                lx,
                line_y,
                theme.chrome_fg,
                text_right,
            );
            if index == 0 {
                // Faux bold for the name: a second pass one pixel right.
                let name = ellipsized(&detail.name, inner);
                draw_theme_text(
                    buffer,
                    stride_px,
                    font,
                    &name,
                    lx.saturating_add(1),
                    line_y,
                    theme.chrome_fg,
                    text_right,
                );
            }
            line_y = line_y.saturating_add(font.cell_h);
        }
        if !detail.chords.is_empty() || !detail.config_key.is_empty() {
            let meta_y = row_y.saturating_add(font.cell_h.saturating_mul(2));
            let lx = text_x.saturating_add(font.cell_w);
            let lead = "chords: ";
            draw_theme_text(buffer, stride_px, font, lead, lx, meta_y, muted, text_right);
            let chords_x = lx.saturating_add(font.cell_w.saturating_mul(text_cells(lead)));
            draw_theme_text(
                buffer,
                stride_px,
                font,
                &detail.chords,
                chords_x,
                meta_y,
                focus_rgb,
                text_right,
            );
            let key_x =
                chords_x.saturating_add(font.cell_w.saturating_mul(text_cells(&detail.chords)));
            let key = format!(" · config key: {}", detail.config_key);
            draw_theme_text(
                buffer, stride_px, font, &key, key_x, meta_y, muted, text_right,
            );
        }
    }

    let hint_y = bottom.saturating_sub(font.cell_h.saturating_mul(3) / 2);
    draw_theme_text(
        buffer,
        stride_px,
        font,
        frame.footer,
        text_x,
        hint_y,
        focus_rgb,
        text_right,
    );
    Some(layout)
}

/// One row on the theme-settings list (family or leaf).
pub struct ThemePickerRow<'a> {
    pub label: &'a str,
    pub branch: bool,
}

/// Root picker hint. Must stay ≤ 50 display cells (`52` panel minus one cell
/// inset on each side).
pub const THEME_PICKER_HINT_ROOT: &str = "Up/Down | Right open | Enter apply | Esc cancel";
/// Family submenu hint. Same 50-cell budget as [`THEME_PICKER_HINT_ROOT`].
pub const THEME_PICKER_HINT_FAMILY: &str = "Up/Down | Left back | Enter apply | Esc cancel";

/// Return the number of theme rows that fit between the picker header and
/// ANSI preview. The picker uses this same calculation for rendering and
/// selection scrolling so the selected row stays visible after resizing.
pub fn theme_picker_visible_rows(
    font: &FontMetrics,
    theme_count: usize,
    stride_px: usize,
    buffer_height_px: usize,
) -> usize {
    if theme_count == 0 || font.cell_h == 0 || buffer_height_px < font.cell_h.saturating_mul(8) {
        return 0;
    }
    let window_cols = stride_px / font.cell_w.max(1);
    let window_rows = buffer_height_px / font.cell_h;
    let geom = palette_geom(window_cols.max(1), window_rows.max(1), font.cell_h);
    let pitch = geom.row_pitch_px;
    let margin_y = font.cell_h.max(4);
    let available_height = buffer_height_px.saturating_sub(margin_y.saturating_mul(2));
    let header_px = font
        .cell_h
        .saturating_mul(4)
        .saturating_add(if geom.compact { 0 } else { font.cell_h });
    let list_budget = available_height.saturating_sub(header_px);
    let fitted = theme_count
        .saturating_mul(pitch)
        .saturating_add(header_px)
        .min(available_height);
    fitted
        .saturating_sub(header_px)
        .min(list_budget)
        .checked_div(pitch.max(1))
        .unwrap_or(0)
        .max(1)
}

/// Centered host-owned theme settings. The selected theme paints its own
/// preview chrome and all 16 ANSI swatches; it never changes pane geometry.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_theme_picker(
    font: &FontMetrics,
    rows: &[ThemePickerRow<'_>],
    selected: Option<usize>,
    scroll: usize,
    preview: &Theme,
    hint: &str,
    surface: OverlaySurface,
    buffer: &mut [u32],
    stride_px: usize,
    buffer_height_px: usize,
    focus_rgb: [u8; 3],
) {
    if rows.is_empty()
        || stride_px < font.cell_w.saturating_mul(24)
        || buffer_height_px < font.cell_h.saturating_mul(8)
    {
        return;
    }
    let margin_x = font.cell_w.max(4);
    let margin_y = font.cell_h.max(4);
    let window_cols = stride_px / font.cell_w.max(1);
    let window_rows = buffer_height_px / font.cell_h.max(1);
    let geom = palette_geom(window_cols, window_rows, font.cell_h);
    let width = font
        .cell_w
        .saturating_mul(52)
        .min(stride_px.saturating_sub(margin_x.saturating_mul(2)));
    let height = font
        .cell_h
        .saturating_mul(4)
        .saturating_add(if geom.compact { 0 } else { font.cell_h })
        .saturating_add(geom.row_pitch_px.saturating_mul(rows.len()))
        .saturating_add(font.cell_h)
        .min(buffer_height_px.saturating_sub(margin_y.saturating_mul(2)));
    let x = (stride_px.saturating_sub(width)) / 2;
    let y = (buffer_height_px.saturating_sub(height)) / 2;
    if width < 4 || height < 4 {
        return;
    }

    paint_panel_shadow(buffer, stride_px, x, y, width, height, preview.default_bg);
    paint_panel_border(buffer, stride_px, x, y, width, height, focus_rgb);
    paint_overlay_surface(
        buffer,
        stride_px,
        x + 1,
        y + 1,
        width - 2,
        height - 2,
        preview.chrome_bg,
        surface,
    );

    let text_x = x.saturating_add(font.cell_w);
    let text_right = x.saturating_add(width).saturating_sub(font.cell_w);
    let mut row_y = y.saturating_add(font.cell_h / 2);
    let position = selected
        .map(|index| format!(" ({}/{})", index.saturating_add(1), rows.len()))
        .unwrap_or_default();
    draw_theme_text(
        buffer,
        stride_px,
        font,
        &format!("Theme settings - {}{position}", preview.name),
        text_x,
        row_y,
        preview.chrome_fg,
        text_right,
    );
    row_y = row_y.saturating_add(font.cell_h);
    draw_theme_text(
        buffer,
        stride_px,
        font,
        hint,
        text_x,
        row_y,
        preview.pane_border,
        text_right,
    );
    row_y = row_y.saturating_add(font.cell_h);
    if !geom.compact {
        row_y = row_y.saturating_add(font.cell_h);
    }

    let swatch_y = y
        .saturating_add(height)
        .saturating_sub(font.cell_h.saturating_add(font.cell_h / 2));
    let visible_rows = theme_picker_visible_rows(font, rows.len(), stride_px, buffer_height_px);
    let max_scroll = rows.len().saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);
    for (index, row) in rows.iter().enumerate().skip(scroll).take(visible_rows) {
        if row_y.saturating_add(geom.row_pitch_px) > swatch_y {
            break;
        }
        let is_selected = selected == Some(index);
        let text_y = row_y.saturating_add(geom.row_pitch_px.saturating_sub(font.cell_h) / 2);
        if is_selected {
            fill_rect(
                buffer,
                stride_px,
                x + 1,
                row_y,
                width - 2,
                geom.row_pitch_px,
                blend_rgb(focus_rgb, preview.chrome_bg, 0.28),
            );
            fill_rect(
                buffer,
                stride_px,
                x + 1,
                row_y,
                3,
                geom.row_pitch_px,
                focus_rgb,
            );
        }
        let marker = if is_selected { ">" } else { " " };
        let branch = if row.branch { " >" } else { "" };
        draw_theme_text(
            buffer,
            stride_px,
            font,
            &format!("{marker} {}{branch}", row.label),
            text_x,
            text_y,
            if is_selected {
                preview.chrome_fg
            } else {
                preview.pane_border
            },
            text_right,
        );
        row_y = row_y.saturating_add(geom.row_pitch_px);
    }

    let swatch_x = text_x.saturating_add(font.cell_w.saturating_mul(10));
    draw_theme_text(
        buffer,
        stride_px,
        font,
        "ANSI 0-15",
        text_x,
        swatch_y,
        preview.chrome_fg,
        swatch_x,
    );
    let available = text_right.saturating_sub(swatch_x);
    let swatch_w = (available / 16).max(1);
    let swatch_h = (font.cell_h / 2).max(3);
    let swatch_top = swatch_y.saturating_add((font.cell_h.saturating_sub(swatch_h)) / 2);
    for (index, rgb) in preview.ansi.iter().enumerate() {
        let sx = swatch_x.saturating_add(index.saturating_mul(swatch_w));
        if sx >= text_right {
            break;
        }
        fill_rect(
            buffer,
            stride_px,
            sx,
            swatch_top,
            swatch_w.min(text_right - sx),
            swatch_h,
            *rgb,
        );
    }
}

/// Flare ray rotation: one revolution per period, in ms.
const RAY_SPIN_MS: f32 = 6000.0;
/// Ray arm length in cell heights at zero and full flare intensity.
const RAY_LEN_MIN_CELLS: f32 = 1.1;
const RAY_LEN_MAX_CELLS: f32 = 3.2;
/// Skip the star glyph cell before the arm starts (~3 px at 15 px font).
const RAY_START_CELLS: f32 = 0.2;
/// Ray colour at rest and at full flash (matches the core's flare tints).
const RAY_TINT: [u8; 3] = [0xa8, 0xd0, 0xff];
const RAY_WHITE: [u8; 3] = [0xff, 0xff, 0xff];

/// Full-window launch splash. Fills the frame with the theme background
/// and draws the content lines as a centered, left-aligned column — the
/// same layout as the CLI splash, so both front-ends read identically.
/// Host-owned and topmost: drawn above panes, chrome, and other overlays.
///
/// `animation_ms` is the splash clock when the main page (with the word
/// art) is showing: the flare stars are nudged onto their letter corners
/// and their `╱` ray cells are replaced by pixel-drawn arms that rotate
/// around the star and lengthen with the flare's brightness. `None` (topic
/// pages) draws the lines as they are.
pub fn rasterize_splash(
    font: &FontMetrics,
    lines: &[Vec<(String, [u8; 3])>],
    buffer: &mut [u32],
    stride_px: usize,
    buffer_height_px: usize,
    bg: [u8; 3],
    animation_ms: Option<u64>,
) {
    if stride_px == 0 || buffer_height_px == 0 {
        return;
    }
    fill_rect(buffer, stride_px, 0, 0, stride_px, buffer_height_px, bg);
    let line_cols = |line: &[(String, [u8; 3])]| {
        line.iter()
            .map(|(text, _)| text.chars().count())
            .sum::<usize>()
    };
    let max_cols = lines.iter().map(|line| line_cols(line)).max().unwrap_or(0);
    if max_cols == 0 || lines.is_empty() {
        return;
    }
    let block_w = max_cols.saturating_mul(font.cell_w).min(stride_px);
    let block_h = lines
        .len()
        .saturating_mul(font.cell_h)
        .min(buffer_height_px);
    let col_x0 = (stride_px.saturating_sub(block_w)) / 2;
    let mut row_y = (buffer_height_px.saturating_sub(block_h)) / 2;
    // Flare glyphs (star, rays, streak) are painted last, nudged half a cell
    // sideways toward the letter and half a row up, so each star overlays a
    // block corner and its rays still emanate from it. The terminal
    // front-end cannot do this; the cell grid is the same, only the host's
    // pixels differ.
    let mut stars: Vec<(char, usize, usize, [u8; 3])> = Vec::new();
    let (nudge_x, nudge_y) = (font.cell_w / 2, font.cell_h / 2);
    // Art bounding box in pixels: rays never cross it.
    let art_top = row_y;
    let art_bottom = row_y + ART.len() * font.cell_h;
    let art_left = col_x0 + ART_MARGIN_LEFT * font.cell_w;
    let art_right = col_x0 + (art_frame_width() - ART_MARGIN) * font.cell_w;
    for line in lines {
        if row_y.saturating_add(font.cell_h) > buffer_height_px {
            break;
        }
        let mut x = col_x0;
        let mut col = 0usize;
        for (text, rgb) in line {
            for ch in text.chars() {
                if x.saturating_add(font.cell_w) > stride_px {
                    break;
                }
                if ch == RAY_GLYPH && animation_ms.is_some() {
                    // Replaced by pixel arms below.
                } else if matches!(
                    ch,
                    FLARE_GLYPH | FLARE_PEAK_GLYPH | RAY_GLYPH | STREAK_GLYPH | STREAK_CORE_GLYPH
                ) {
                    let top_left = col < ART_MARGIN_LEFT;
                    // Both groups rise half a row: the top-left star onto the
                    // P's top corner, the lower-right star onto the top-right
                    // corner of the C's bottom arm, so its rays leave the
                    // corner up-right and its streak runs right at that height.
                    // The lower-right group moves a whole extra cell: the C's
                    // right outline stroke (`╗`) sits between its block edge
                    // and the margin, and the star belongs on the block edge.
                    // The top-left group moves three-eighths of a cell right,
                    // so the star's centre lands a couple of pixels outside
                    // the P's corner rather than on top of the block edge.
                    let sy = row_y.saturating_sub(nudge_y);
                    let sx = if top_left {
                        x + nudge_x * 3 / 4
                    } else {
                        x.saturating_sub(nudge_x + font.cell_w)
                    };
                    stars.push((ch, sx, sy, *rgb));
                } else if ch != ' ' {
                    blit_glyph(buffer, stride_px, font, ch, x, row_y, *rgb);
                }
                x = x.saturating_add(font.cell_w);
                col += 1;
            }
        }
        row_y = row_y.saturating_add(font.cell_h);
    }
    let mut star_index = 0usize;
    let levels = animation_ms.map(flare_intensities);
    for (ch, sx, sy, rgb) in stars {
        if sx.saturating_add(font.cell_w) <= stride_px
            && sy.saturating_add(font.cell_h) <= buffer_height_px
        {
            blit_glyph(buffer, stride_px, font, ch, sx, sy, rgb);
        }
        let is_star = matches!(ch, FLARE_GLYPH | FLARE_PEAK_GLYPH);
        if let (true, Some(levels), Some(ms)) = (is_star, levels, animation_ms) {
            let level = levels.get(star_index).copied().unwrap_or(0.0);
            // Top-left spins clockwise, lower-right counter-clockwise.
            let dir = if star_index == 0 { 1.0 } else { -1.0 };
            let angle = dir * (ms as f32 / RAY_SPIN_MS) * std::f32::consts::TAU
                + std::f32::consts::FRAC_PI_4;
            let center = (
                sx as f32 + font.cell_w as f32 / 2.0,
                sy as f32 + font.cell_h as f32 / 2.0,
            );
            let len = font.cell_h as f32
                * (RAY_LEN_MIN_CELLS + (RAY_LEN_MAX_CELLS - RAY_LEN_MIN_CELLS) * level);
            let rgb = lerp_rgb(RAY_TINT, RAY_WHITE, level);
            let clip = RayClip {
                stride: stride_px,
                height: buffer_height_px,
                art: (art_left, art_top, art_right, art_bottom),
            };
            let start = RAY_START_CELLS * font.cell_h as f32;
            for arm in 0..4 {
                let theta = angle + arm as f32 * std::f32::consts::FRAC_PI_2;
                draw_ray(
                    buffer,
                    &clip,
                    center,
                    theta,
                    len,
                    start,
                    rgb,
                    0.25 + 0.7 * level,
                );
            }
            star_index += 1;
        }
    }
}

/// Pixel-space clip for flare rays: inside the buffer, outside the art box.
struct RayClip {
    stride: usize,
    height: usize,
    /// (left, top, right, bottom) in pixels; right/bottom exclusive.
    art: (usize, usize, usize, usize),
}

impl RayClip {
    fn allows(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x as usize >= self.stride || y as usize >= self.height {
            return false;
        }
        let (x, y) = (x as usize, y as usize);
        let (l, t, r, b) = self.art;
        !(x >= l && x < r && y >= t && y < b)
    }

    /// Farthest distance along `(dx, dy)` from `center`, between `start`
    /// and `max`, at which the clip still allows the pixel; 0 when it
    /// allows none. Blocked samples near the centre do not end the walk:
    /// the lower-right star's centre sits inside the art box, and its arms
    /// that leave the box keep their full length while `draw_ray`'s
    /// per-pixel clip hides the stretch inside it (PT-88). Arms that never
    /// leave the box get no room and draw nothing.
    fn room(&self, center: (f32, f32), dx: f32, dy: f32, start: f32, max: f32) -> f32 {
        let mut last = 0.0_f32;
        let mut d = start.max(0.0);
        loop {
            let here = d.min(max);
            let x = (center.0 + dx * here).round() as i32;
            let y = (center.1 + dy * here).round() as i32;
            if self.allows(x, y) {
                last = here;
            }
            if d >= max {
                break;
            }
            d += 0.5;
        }
        last
    }
}

fn lerp_rgb(a: [u8; 3], b: [u8; 3], k: f32) -> [u8; 3] {
    let k = k.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

/// Blend `rgb` over one pixel with `alpha` 0..1.
fn blend_px(buffer: &mut [u32], stride: usize, x: i32, y: i32, rgb: [u8; 3], alpha: f32) {
    if x < 0 || y < 0 || alpha <= 0.0 {
        return;
    }
    let idx = y as usize * stride + x as usize;
    if x as usize >= stride || idx >= buffer.len() {
        return;
    }
    let a = alpha.min(1.0);
    let dest = unpack_rgb(buffer[idx]);
    let mix = |f: u8, d: u8| (f as f32 * a + d as f32 * (1.0 - a)).round() as u8;
    let cover = (a * 255.0).round() as u8;
    buffer[idx] = pack_argb(
        raise_alpha(alpha_of(buffer[idx]), cover),
        [
            mix(rgb[0], dest[0]),
            mix(rgb[1], dest[1]),
            mix(rgb[2], dest[2]),
        ],
    );
}

/// One anti-aliased ray from `center` at angle `theta` (radians, screen
/// coordinates: y grows downward) of `len` pixels, fading with a quadratic
/// tail. Each sample splats bilinearly over the 2×2 pixels it straddles.
/// `len` is clamped to the clip room so the tail reaches the art box or
/// buffer edge instead of cutting at full brightness; an arm that starts
/// inside the art box and leaves it keeps its length past the edge.
#[allow(clippy::too_many_arguments)]
fn draw_ray(
    buffer: &mut [u32],
    clip: &RayClip,
    center: (f32, f32),
    theta: f32,
    len: f32,
    start: f32,
    rgb: [u8; 3],
    alpha: f32,
) {
    let (dx, dy) = (theta.cos(), theta.sin());
    let len = len.min(clip.room(center, dx, dy, start, len));
    if len <= start {
        return;
    }
    let span = len - start;
    let steps = (span * 2.0).ceil() as usize;
    for i in 0..steps {
        let d = start + i as f32 * 0.5;
        if d >= len {
            break;
        }
        let t = 1.0 - (d - start) / span;
        let fade = t * t;
        let a = alpha * fade;
        let (px, py) = (center.0 + dx * d, center.1 + dy * d);
        let (x0, y0) = (px.floor(), py.floor());
        let (fx, fy) = (px - x0, py - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        for (ox, oy, w) in [
            (0, 0, (1.0 - fx) * (1.0 - fy)),
            (1, 0, fx * (1.0 - fy)),
            (0, 1, (1.0 - fx) * fy),
            (1, 1, fx * fy),
        ] {
            let (x, y) = (x0 + ox, y0 + oy);
            if clip.allows(x, y) {
                blend_px(buffer, clip.stride, x, y, rgb, a * w);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_theme_text(
    buffer: &mut [u32],
    stride_px: usize,
    font: &FontMetrics,
    text: &str,
    mut x: usize,
    y: usize,
    rgb: [u8; 3],
    clip_right: usize,
) {
    for ch in text.chars() {
        let cells = prismattyc_core::char_display_width(ch).max(1);
        let advance = font.cell_w.saturating_mul(cells);
        if x.saturating_add(advance) > clip_right {
            break;
        }
        blit_glyph_in(buffer, stride_px, font, ch, x, y, rgb, x, clip_right, false);
        x = x.saturating_add(advance);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_render_timer(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    font: &FontMetrics,
    theme: &crate::theme::Theme,
    summary: crate::RenderWindowSummary,
) {
    let stride = width as usize;
    if stride == 0 || height == 0 || buffer.is_empty() {
        return;
    }
    let reason = summary
        .dominant_full_repaint_reason
        .map_or("-", crate::FullRepaintReason::as_str);
    let text = format!(
        " last/max raster={}/{}us frames={} cells_max={} blit_sum={} full={} ",
        summary.last_raster_us,
        summary.max_raster_us,
        summary.frame_count,
        summary.max_cells_painted,
        summary.blit_sum,
        reason,
    );
    let max_cells = stride.saturating_sub(16) / font.cell_w.max(1);
    let text: String = text.chars().take(max_cells.max(1)).collect();
    let panel_w = (text.chars().count() * font.cell_w + 8).min(stride.saturating_sub(8));
    let panel_h = font.cell_h.saturating_add(6).min(height as usize);
    let x = 4;
    let y = (height as usize).saturating_sub(panel_h + 4);
    fill_rect_argb(
        buffer,
        stride,
        x,
        y,
        panel_w,
        panel_h,
        theme.overlay_bg,
        230,
    );
    draw_theme_text(
        buffer,
        stride,
        font,
        &text,
        x + 4,
        y + 3,
        theme.chrome_fg,
        x.saturating_add(panel_w).saturating_sub(2),
    );
}

/// Top tab strip. Drawn only when the caller has more than one tab.
///
/// Active tab: 1px bottom marker (70% blend) plus title in full `focus_rgb`.
/// Inactive tabs stay `CHROME_BG` with neutral title ink. Badges are 6px squares
/// centered on the close-glyph midline. `window_pad` insets tab slots
/// (background still spans the full width). `pane_gap` is chrome between
/// slots and is not painted as a 1px divider.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn rasterize_tab_strip(
    font: &FontMetrics,
    tabs: &[crate::mux::TabInfo],
    buffer: &mut [u32],
    stride_px: usize,
    bar_h: usize,
    focus_rgb: [u8; 3],
    editing: Option<(usize, &str, bool)>,
    window_pad: usize,
    pane_gap: usize,
) {
    rasterize_tab_strip_with_theme(
        default_theme(),
        font,
        tabs,
        buffer,
        stride_px,
        bar_h,
        focus_rgb,
        editing,
        window_pad,
        pane_gap,
        0,
        false,
        None,
        crate::config::DEFAULT_HOVER_BLEND,
        OPAQUE_ALPHA,
        TitleRowStyle::default(),
        None,
    );
}

/// Themed live-host variant of [`rasterize_tab_strip`].
#[allow(clippy::too_many_arguments)]
pub fn rasterize_tab_strip_with_theme(
    theme: &Theme,
    font: &FontMetrics,
    tabs: &[crate::mux::TabInfo],
    buffer: &mut [u32],
    stride_px: usize,
    bar_h: usize,
    focus_rgb: [u8; 3],
    editing: Option<(usize, &str, bool)>,
    window_pad: usize,
    pane_gap: usize,
    content_inset: usize,
    reserve_end: bool,
    hover: Option<crate::mux::StripHit>,
    hover_blend: f32,
    bg_alpha: u8,
    title_row: TitleRowStyle<'_>,
    pulse: Option<f32>,
) {
    if tabs.is_empty() || stride_px == 0 || bar_h == 0 {
        return;
    }
    // PT-87: the bar itself carries `chrome_opacity`; pills, markers, text and
    // badges painted on top stay opaque. The bar ground is `pane_backdrop`,
    // matching the window frame, so the strip and the frame read as one band.
    fill_rect_argb(
        buffer,
        stride_px,
        0,
        0,
        stride_px,
        bar_h,
        theme.pane_backdrop,
        bg_alpha,
    );
    let n = tabs.len();
    let cell_w = font.cell_w.max(1);
    let inner_stride = if reserve_end {
        stride_px.saturating_sub(cell_w)
    } else {
        stride_px
    };
    if reserve_end && inner_stride < stride_px {
        let x0 = inner_stride;
        let end_fill = if hover == Some(crate::mux::StripHit::EmptyEnd) {
            crate::theme::hover_rgb(
                theme.variant,
                theme.pane_backdrop,
                theme.chrome_fg,
                hover_blend,
            )
        } else {
            theme.pane_backdrop
        };
        fill_rect_argb(
            buffer,
            stride_px,
            x0,
            0,
            cell_w.min(stride_px.saturating_sub(x0)),
            bar_h,
            end_fill,
            bg_alpha,
        );
        let tick = mix_rgb(theme.chrome_fg, theme.chrome_bg, 160);
        fill_rect(buffer, stride_px, x0, 1, 1, bar_h.saturating_sub(2), tick);
        fill_rect(
            buffer,
            stride_px,
            x0.saturating_add(cell_w.saturating_sub(2)),
            1,
            1,
            bar_h.saturating_sub(2),
            tick,
        );
    }
    for (i, tab) in tabs.iter().enumerate() {
        let Some((x0, width)) =
            crate::mux::tab_slot_bounds(i, n, inner_stride, window_pad, pane_gap)
        else {
            return;
        };
        let handle_w = crate::mux::pane_handle_w(cell_w);
        let title_h = font.cell_h.max(1);
        let handle_row = bar_h > title_h;
        let editing_here = editing.is_some_and(|(idx, _, _)| idx == i);
        let active_bg = active_chip_bg(theme, focus_rgb);
        if !editing_here {
            let base = if tab.selected {
                active_bg
            } else {
                theme.chrome_bg
            };
            let hovered = hover
                == Some(crate::mux::StripHit::Tab {
                    index: i,
                    close: false,
                });
            let hover_fill = |rgb: [u8; 3]| {
                if hovered {
                    crate::theme::hover_rgb(theme.variant, rgb, theme.chrome_fg, hover_blend)
                } else {
                    rgb
                }
            };
            if tab.selected {
                // The active tab runs top-to-bottom from the chip colour
                // lightened by ACTIVE_TAB_GRADIENT_DEPTH (less where the title
                // ink would lose AA) to the chip colour, so the selection
                // reads as a lit key.
                let depth = active_tab_gradient_depth(base);
                for y in 0..bar_h {
                    let row = hover_fill(active_tab_gradient_rgb(base, depth, y, bar_h));
                    fill_rect_argb(buffer, stride_px, x0, y, width, 1, row, bg_alpha);
                }
            } else {
                fill_rect_argb(
                    buffer,
                    stride_px,
                    x0,
                    0,
                    width,
                    bar_h,
                    hover_fill(base),
                    bg_alpha,
                );
            }
        }
        let (handle_y, handle_h) = if handle_row {
            (title_h.saturating_add(2), title_h.saturating_sub(4).max(1))
        } else {
            (2, bar_h.saturating_sub(4))
        };
        let handle_origin = x0.saturating_add(content_inset);
        for h in 0..tab.handles {
            let hx = handle_origin.saturating_add(h.saturating_mul(handle_w));
            if hx.saturating_add(handle_w) > x0.saturating_add(width) {
                break;
            }
            // Pane chips are white glass: a 1-px outline at half alpha and a
            // fill at 35 % (60 % for the focused pane) over the tab ground.
            let under = if tab.selected {
                active_bg
            } else {
                theme.chrome_bg
            };
            let base = pane_chip_fill_rgb(under, tab.focused_handle == Some(h));
            let active = tab.handle_active.get(h).copied().unwrap_or(false);
            let breathed = match (active, pulse) {
                (true, Some(phase)) => pulse_mix(base, theme.active_badge, phase),
                _ => base,
            };
            let noticed = title_row
                .notice
                .is_some_and(|(tab, handle, _)| tab == i && handle == h);
            let tinted = if noticed {
                mix_rgb(breathed, theme.attention_badge, 160)
            } else {
                breathed
            };
            let fill = if matches!(
                hover,
                Some(crate::mux::StripHit::Pane {
                    tab,
                    handle,
                    ..
                }) if tab == i && handle == h
            ) {
                crate::theme::hover_rgb(theme.variant, tinted, theme.chrome_fg, hover_blend)
            } else {
                tinted
            };
            let chip_w = handle_w.saturating_sub(1);
            fill_rect(
                buffer,
                stride_px,
                hx,
                handle_y,
                chip_w,
                handle_h,
                pane_chip_outline_rgb(under),
            );
            if chip_w > 2 && handle_h > 2 {
                fill_rect(
                    buffer,
                    stride_px,
                    hx + 1,
                    handle_y + 1,
                    chip_w - 2,
                    handle_h - 2,
                    fill,
                );
            }
        }
        if editing_here {
            fill_rect(buffer, stride_px, x0, 0, width, bar_h, focus_rgb);
        }
        if !editing_here
            && hover
                == Some(crate::mux::StripHit::Tab {
                    index: i,
                    close: true,
                })
        {
            if let Some(close_left) =
                crate::mux::tab_close_left_with_inset(x0, width, cell_w, content_inset)
            {
                let base = if tab.selected {
                    theme.tab_active_bg
                } else {
                    theme.chrome_bg
                };
                let fill =
                    crate::theme::hover_rgb(theme.variant, base, theme.chrome_fg, hover_blend);
                fill_rect(
                    buffer,
                    stride_px,
                    close_left,
                    0,
                    cell_w,
                    title_h.min(bar_h),
                    fill,
                );
            }
        }
        if tab.selected || editing_here {
            let marker_h = TAB_MARKER_H.min(bar_h);
            let under = if editing_here { focus_rgb } else { active_bg };
            fill_rect(
                buffer,
                stride_px,
                x0,
                bar_h.saturating_sub(marker_h),
                width,
                marker_h,
                active_tab_marker_rgb(under),
            );
        }
        let mut x = x0.saturating_add(TAB_LABEL_INSET.max(content_inset));
        let close_w = font.cell_w.max(1);
        let slot_end = x0.saturating_add(width);
        let close_left = crate::mux::tab_close_left_with_inset(x0, width, close_w, content_inset);
        let badge_count =
            usize::from(tab.attention) + usize::from(tab.unseen) + usize::from(tab.active);
        let badge_space = if badge_count == 0 {
            TAB_LABEL_INSET
        } else {
            badge_count.saturating_mul(crate::mux::TAB_BADGE_SIZE + 2) + 2
        };
        let show_badges = width >= badge_space;
        let text_limit = close_left
            .unwrap_or(slot_end)
            .saturating_sub(if show_badges {
                badge_space
            } else {
                TAB_LABEL_INSET
            });
        let zoomed_title;
        let hover_handle = match hover {
            Some(crate::mux::StripHit::Pane {
                tab: hit_tab,
                handle,
                ..
            }) if hit_tab == i => Some(handle),
            _ => None,
        };
        let notice = title_row
            .notice
            .filter(|(tab, _, _)| *tab == i)
            .map(|(_, handle, text)| (handle, text));
        let decided = title_row_decision(
            title_row.mode,
            tab.handles,
            tab.title.as_str(),
            tab.pane_title.as_deref(),
            &tab.handle_titles,
            hover_handle,
            notice,
        );
        let label = match editing.and_then(|(idx, text, _)| (idx == i).then_some(text)) {
            Some(text) => text,
            None => {
                if tab.zoomed {
                    zoomed_title = format!("{} {TAB_ZOOM_MARK}", decided.label);
                    zoomed_title.as_str()
                } else {
                    decided.label
                }
            }
        };
        let git_label = if !editing_here {
            tab.git_label.as_deref().map(|git| {
                crate::git_info::compact(
                    label,
                    git,
                    text_limit.saturating_sub(x) / font.cell_w.max(1),
                )
            })
        } else {
            None
        };
        let label = git_label.as_deref().unwrap_or(label);
        let selected_all = editing.is_some_and(|(idx, _, sel)| idx == i && sel);
        let ink = if editing_here {
            contrast_ink(focus_rgb)
        } else if tab.selected {
            contrast_ink(active_bg)
        } else {
            theme.chrome_fg
        };
        for ch in label.chars() {
            let cells = prismattyc_core::char_display_width(ch).max(1);
            let advance = font.cell_w.saturating_mul(cells);
            if x.saturating_add(advance) > text_limit {
                break;
            }
            if selected_all {
                let cell_h = font.cell_h.min(bar_h);
                fill_rect(
                    buffer,
                    stride_px,
                    x,
                    0,
                    advance,
                    cell_h,
                    contrast_ink(focus_rgb),
                );
                blit_glyph_in(
                    buffer, stride_px, font, ch, x, 0, focus_rgb, x0, slot_end, false,
                );
            } else {
                blit_glyph_in(buffer, stride_px, font, ch, x, 0, ink, x0, slot_end, false);
            }
            x = x.saturating_add(advance);
        }
        let mut badge_x = close_left.unwrap_or(slot_end).saturating_sub(2);
        let badge_y = crate::mux::tab_badge_top(title_h.min(bar_h));
        if show_badges {
            if tab.active {
                let badge = pulse
                    .map(|phase| pulse_color_with_theme(theme, phase))
                    .unwrap_or(theme.active_badge);
                fill_rect(
                    buffer,
                    stride_px,
                    badge_x.saturating_sub(crate::mux::TAB_BADGE_SIZE),
                    badge_y,
                    crate::mux::TAB_BADGE_SIZE,
                    crate::mux::TAB_BADGE_SIZE,
                    badge,
                );
                badge_x = badge_x.saturating_sub(crate::mux::TAB_BADGE_SIZE + 2);
            }
            if tab.unseen {
                fill_rect(
                    buffer,
                    stride_px,
                    badge_x.saturating_sub(crate::mux::TAB_BADGE_SIZE),
                    badge_y,
                    crate::mux::TAB_BADGE_SIZE,
                    crate::mux::TAB_BADGE_SIZE,
                    theme.unseen_badge,
                );
                badge_x = badge_x.saturating_sub(crate::mux::TAB_BADGE_SIZE + 2);
            }
            if tab.attention {
                fill_rect(
                    buffer,
                    stride_px,
                    badge_x.saturating_sub(crate::mux::TAB_BADGE_SIZE),
                    badge_y,
                    crate::mux::TAB_BADGE_SIZE,
                    crate::mux::TAB_BADGE_SIZE,
                    theme.attention_badge,
                );
            }
        }
        if let Some(close_left) = close_left {
            blit_glyph_in(
                buffer,
                stride_px,
                font,
                font.close_glyph,
                close_left,
                0,
                ink,
                x0,
                slot_end,
                false,
            );
        }
    }
}

/// Render the detail row at the native terminal font size.
/// Its scratch buffer and destination writes are bounded by the chip text box.
#[allow(clippy::too_many_arguments)]
fn rasterize_rail_names(
    font: &FontMetrics,
    text: &str,
    buffer: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    ink: [u8; 3],
    background: [u8; 3],
) {
    let visible_h = font.cell_h.min(height);
    if width == 0 || visible_h == 0 {
        return;
    }
    let source_w = width;
    let source_h = font.cell_h.max(1);
    let mut scratch = vec![pack_argb(OPAQUE_ALPHA, background); source_w * source_h];
    let text = ellipsized(text, source_w / font.cell_w.max(1));
    let mut dx = 0;
    for ch in text.chars() {
        blit_glyph_in(
            &mut scratch,
            source_w,
            font,
            ch,
            dx,
            0,
            ink,
            0,
            source_w,
            false,
        );
        dx += font.cell_w * prismattyc_core::char_display_width(ch).max(1);
    }
    for row in 0..visible_h {
        for col in 0..width {
            if x + col >= stride {
                break;
            }
            if let Some(pixel) = buffer.get_mut((y + row) * stride + x + col) {
                *pixel = scratch[row * source_w + col];
            }
        }
    }
}

/// Spaces rail (PT-91): label-sized chips on one window edge, the same
/// visual family as the tab strip. `views` is rail order with the `+` chip
/// last; `layout` says where every chip sits. Chips that do not fit are
/// not painted (the layout returns no box for them).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_space_rail(
    theme: &Theme,
    font: &FontMetrics,
    layout: &crate::space_rail::RailLayout,
    views: &[crate::space_rail::RailChipView],
    buffer: &mut [u32],
    stride_px: usize,
    focus_rgb: [u8; 3],
    danger_rgb: [u8; 3],
    hover: Option<crate::space_rail::RailHit>,
    hover_blend: f32,
    bg_alpha: u8,
) {
    if views.is_empty() || stride_px == 0 || layout.w == 0 || layout.h == 0 {
        return;
    }
    fill_rect_argb(
        buffer,
        stride_px,
        layout.x,
        layout.y,
        layout.w,
        layout.h,
        theme.pane_backdrop,
        bg_alpha,
    );
    let n = layout.chip_px.len();
    let mut displayed = views.to_vec();
    if layout.overflow {
        displayed.push(crate::space_rail::RailChipView {
            label: "…".into(),
            pane_names: String::new(),
            current: false,
            focused: false,
            editing: None,
            confirm: false,
            plus: true,
        });
    }
    let cell_w = font.cell_w.max(1);
    let cell_h = font.cell_h.max(1);
    for (index, view) in displayed.iter().enumerate() {
        let Some((x0, y0, width, height)) = layout.chip_bounds(index, n) else {
            continue;
        };
        let slot_end = x0.saturating_add(width);
        let editing = view.editing.is_some();
        // The current chip sits on `tab_active_bg`, the same subtle lift
        // the active tab gets (PT-95 / PT-123).
        let (mut fill, ink) = if editing {
            (Some(focus_rgb), contrast_ink(focus_rgb))
        } else if view.confirm {
            (Some(danger_rgb), contrast_ink(danger_rgb))
        } else if view.current {
            let bg = active_chip_bg(theme, focus_rgb);
            (Some(bg), contrast_ink(bg))
        } else {
            (None, theme.chrome_fg)
        };
        let hover_whole = (index == n + 1 && hover == Some(crate::space_rail::RailHit::Overflow))
            || hover == Some(crate::space_rail::RailHit::Plus) && view.plus
            || hover
                == Some(crate::space_rail::RailHit::Chip {
                    index,
                    close: false,
                });
        if hover_whole && !editing && !view.confirm {
            let base = fill.unwrap_or(theme.pane_backdrop);
            fill = Some(crate::theme::hover_rgb(
                theme.variant,
                base,
                theme.chrome_fg,
                hover_blend,
            ));
        }
        if let Some(fill) = fill {
            fill_rect(buffer, stride_px, x0, y0, width, height, fill);
        }
        if hover == Some(crate::space_rail::RailHit::Chip { index, close: true })
            && !editing
            && !view.confirm
        {
            if let Some(close_left) = layout.close_left(x0, width) {
                let base = fill.unwrap_or(theme.pane_backdrop);
                let close_fill =
                    crate::theme::hover_rgb(theme.variant, base, theme.chrome_fg, hover_blend);
                fill_rect(
                    buffer, stride_px, close_left, y0, cell_w, height, close_fill,
                );
            }
        }
        if view.current && !editing && !view.confirm {
            let marker_h = TAB_MARKER_H.min(height);
            fill_rect(
                buffer,
                stride_px,
                x0,
                y0.saturating_add(height).saturating_sub(marker_h),
                width,
                marker_h,
                tab_marker_rgb_with_theme(theme, focus_rgb),
            );
        }
        if view.focused && !editing && !view.confirm {
            // 1px keyboard-focus ring, a bounded blend of the focus colour.
            let ring = blend_rgb(focus_rgb, theme.pane_backdrop, 0.6);
            fill_rect(buffer, stride_px, x0, y0, width, 1, ring);
            fill_rect(
                buffer,
                stride_px,
                x0,
                y0.saturating_add(height).saturating_sub(1),
                width,
                1,
                ring,
            );
            fill_rect(buffer, stride_px, x0, y0, 1, height, ring);
            fill_rect(
                buffer,
                stride_px,
                slot_end.saturating_sub(1),
                y0,
                1,
                height,
                ring,
            );
        }
        // Label: left-aligned, ellipsized before the close cell (or the
        // chip end on the current / `+` chip).
        let close_left = if view.plus || view.current || editing || view.confirm {
            None
        } else {
            layout.close_left(x0, width)
        };
        let text_x = x0.saturating_add(crate::space_rail::RAIL_LABEL_INSET);
        let text_limit = close_left
            .unwrap_or(slot_end)
            .saturating_sub(crate::space_rail::RAIL_LABEL_INSET);
        let max_cells = text_limit.saturating_sub(text_x) / cell_w;
        let label = ellipsized(&view.label, max_cells);
        let selected_all = view.editing == Some(true);
        let mut x = text_x;
        for ch in label.chars() {
            let cells = prismattyc_core::char_display_width(ch).max(1);
            let advance = cell_w.saturating_mul(cells);
            if x.saturating_add(advance) > text_limit {
                break;
            }
            if selected_all {
                fill_rect(
                    buffer,
                    stride_px,
                    x,
                    y0,
                    advance,
                    cell_h.min(height),
                    contrast_ink(focus_rgb),
                );
                blit_glyph_in(
                    buffer, stride_px, font, ch, x, y0, focus_rgb, x0, slot_end, false,
                );
            } else {
                blit_glyph_in(buffer, stride_px, font, ch, x, y0, ink, x0, slot_end, false);
            }
            x = x.saturating_add(advance);
        }
        if !editing
            && !view.confirm
            && !view.pane_names.is_empty()
            && height >= cell_h.saturating_mul(2)
        {
            let names = view
                .pane_names
                .split(" · ")
                .map(str::to_string)
                .collect::<Vec<_>>();
            let detail = crate::space_rail::compact_names(&names, max_cells);
            rasterize_rail_names(
                font,
                &detail,
                buffer,
                stride_px,
                text_x,
                y0 + cell_h,
                text_limit.saturating_sub(text_x),
                height.saturating_sub(cell_h + TAB_MARKER_H),
                ink,
                fill.unwrap_or(theme.pane_backdrop),
            );
        }
        if let Some(close_left) = close_left {
            blit_glyph_in(
                buffer,
                stride_px,
                font,
                font.close_glyph,
                close_left,
                y0,
                ink,
                x0,
                slot_end,
                false,
            );
        } else if view.current && !editing && !view.confirm {
            // The current space cannot be deleted from the rail: a dot
            // where the × would be.
            if let Some(cell_left) = layout.close_left(x0, width) {
                let size = crate::mux::TAB_BADGE_SIZE.min(height);
                let dot_x = cell_left
                    .saturating_add(cell_w / 2)
                    .saturating_sub(size / 2);
                let dot_y = y0
                    .saturating_add(crate::mux::tab_chrome_center_y(height))
                    .saturating_sub(size / 2);
                fill_rect(buffer, stride_px, dot_x, dot_y, size, size, ink);
            }
        }
    }
}

/// The active tab's bottom border: a white line at half alpha over the
/// chip colour beneath it (owner order, PT-143). The rail's current chip
/// keeps the focus-colour marker.
pub(crate) fn active_tab_marker_rgb(under: [u8; 3]) -> [u8; 3] {
    mix_rgb(under, [0xff, 0xff, 0xff], ACTIVE_TAB_MARKER_ALPHA)
}

/// White weight out of 256 for [`active_tab_marker_rgb`]: 128 = 50 % alpha.
const ACTIVE_TAB_MARKER_ALPHA: u16 = 128;

/// Pane chip fill: white at 35 % alpha over the tab ground, 60 % for the
/// focused pane so it still reads as the one that has the keyboard.
pub(crate) fn pane_chip_fill_rgb(under: [u8; 3], focused: bool) -> [u8; 3] {
    let alpha = if focused {
        PANE_CHIP_FOCUSED_ALPHA
    } else {
        PANE_CHIP_ALPHA
    };
    mix_rgb(under, [0xff, 0xff, 0xff], alpha)
}

/// Pane chip 1-px outline: white at 50 % alpha over the tab ground.
pub(crate) fn pane_chip_outline_rgb(under: [u8; 3]) -> [u8; 3] {
    mix_rgb(under, [0xff, 0xff, 0xff], PANE_CHIP_OUTLINE_ALPHA)
}

const PANE_CHIP_ALPHA: u16 = 90;
const PANE_CHIP_FOCUSED_ALPHA: u16 = 154;
const PANE_CHIP_OUTLINE_ALPHA: u16 = 128;

#[cfg(test)]
pub(crate) fn tab_marker_rgb(focus_rgb: [u8; 3]) -> [u8; 3] {
    active_tab_marker_rgb(active_chip_bg(default_theme(), focus_rgb))
}

/// How much of the focus colour the active chip carries over `chrome_bg`
/// (PT-136): enough to read as "selected" at a glance, toned so the chip
/// stays chrome rather than a solid swatch.
/// Far end of the active-tab gradient: the chip colour mixed toward
/// [`ACTIVE_TAB_GRADIENT_TOWARD`] by this weight out of 256 on the top row.
/// 8 = three percent of the way to white: the owner's pick after darker and
/// stronger variants; the white bottom line carries the "lit" cue.
const ACTIVE_TAB_GRADIENT_DEPTH: u16 = 8;
const ACTIVE_TAB_GRADIENT_TOWARD: [u8; 3] = [0xff, 0xff, 0xff];

/// Gradient depth for a chip colour: the full [`ACTIVE_TAB_GRADIENT_DEPTH`]
/// unless the ink [`contrast_ink`] picks for `base` would drop under WCAG AA
/// (4.5:1) on the far row — light ink on a mid-tone chip — in which case
/// the largest depth that keeps AA on every row.
pub(crate) fn active_tab_gradient_depth(base: [u8; 3]) -> u16 {
    let ink = contrast_ink(base);
    (0..=ACTIVE_TAB_GRADIENT_DEPTH)
        .rev()
        .find(|&weight| {
            contrast_ratio(ink, mix_rgb(base, ACTIVE_TAB_GRADIENT_TOWARD, weight)) >= 4.5
        })
        .unwrap_or(0)
}

/// Row `y` of an `h`-row active tab: `base` mixed toward
/// [`ACTIVE_TAB_GRADIENT_TOWARD`] by `depth` (out of 256) on the first row,
/// easing linearly to `base` on the last row.
pub(crate) fn active_tab_gradient_rgb(base: [u8; 3], depth: u16, y: usize, h: usize) -> [u8; 3] {
    if h <= 1 {
        return base;
    }
    let span = h - 1;
    let weight = (usize::from(depth) * span.saturating_sub(y.min(span)) / span) as u16;
    mix_rgb(base, ACTIVE_TAB_GRADIENT_TOWARD, weight)
}

/// Background of the active tab chip and the current space chip:
/// `chrome_bg` lifted by [`ACTIVE_CHIP_LIFT`], unless the theme file pins
/// `tab_active_bg`. Ink is chosen for contrast — light on dark, dark on
/// light — by [`contrast_ink`]. (PT-136 tinted this toward the focus
/// colour; PT-143 made it neutral on the owner's order.)
pub fn active_chip_bg(theme: &Theme, _focus_rgb: [u8; 3]) -> [u8; 3] {
    if theme.tab_active_bg_explicit {
        return theme.tab_active_bg;
    }
    let toward = match theme.variant {
        crate::theme::ThemeVariant::Dark => [0xff, 0xff, 0xff],
        crate::theme::ThemeVariant::Light => [0, 0, 0],
    };
    mix_rgb(theme.chrome_bg, toward, ACTIVE_CHIP_LIFT)
}

/// How far the active chip lifts off `chrome_bg` (toward white on dark
/// themes, black on light), out of 256: 26 = 10 %. The owner dropped the
/// PT-136 focus-colour blend (55 %) as too dark and muddy; the white bottom
/// line and the lift carry the selection now.
const ACTIVE_CHIP_LIFT: u16 = 26;

/// WCAG relative luminance of an sRGB colour.
pub fn relative_luminance(rgb: [u8; 3]) -> f32 {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

/// WCAG contrast ratio between two colours (1.0..=21.0).
pub fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn tab_marker_rgb_with_theme(theme: &Theme, focus_rgb: [u8; 3]) -> [u8; 3] {
    blend_rgb(focus_rgb, theme.chrome_bg, TAB_MARKER_OPACITY)
}

pub(crate) fn blend_rgb(fg: [u8; 3], bg: [u8; 3], opacity: f32) -> [u8; 3] {
    let t = opacity.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (t * f32::from(a) + (1.0 - t) * f32::from(b)).round() as u8;
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
}

/// Light text on dark swatches, dark text on bright focus colors. Also the
/// bell toast ink (PT-39) — same rule the pmux-attach toast applies.
fn readable_ink(preferred: [u8; 3], background: [u8; 3]) -> [u8; 3] {
    if contrast_ratio(preferred, background) >= 4.5 {
        preferred
    } else if contrast_ratio([255; 3], background) >= contrast_ratio([0; 3], background) {
        [255; 3]
    } else {
        [0; 3]
    }
}

pub fn contrast_ink(bg: [u8; 3]) -> [u8; 3] {
    // Whichever of the two inks has the higher WCAG contrast against `bg`:
    // light text on a dark colour, dark text on a light one.
    let dark = [0x12, 0x12, 0x14]; // near DEFAULT_BG
    if contrast_ratio(CHROME_FG, bg) >= contrast_ratio(dark, bg) {
        CHROME_FG
    } else {
        dark
    }
}

/// 1px ring around a granted rich-focus region. Clipped to the
/// pane content origin; negative rows (scrolled off the top) are skipped.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_region_focus_ring(
    buffer: &mut [u32],
    stride_px: usize,
    content_x: usize,
    content_y: usize,
    cell_w: usize,
    cell_h: usize,
    row: i32,
    col: u16,
    rows: u16,
    cols: u16,
    rgb: [u8; 3],
) {
    if cell_w == 0 || cell_h == 0 || rows == 0 || cols == 0 {
        return;
    }
    if row < 0 {
        return;
    }
    let x = content_x.saturating_add(usize::from(col).saturating_mul(cell_w));
    let y = content_y.saturating_add((row as usize).saturating_mul(cell_h));
    let width = usize::from(cols).saturating_mul(cell_w).max(1);
    let height = usize::from(rows).saturating_mul(cell_h).max(1);
    let thickness = 1usize.min(width).min(height);
    fill_rect(buffer, stride_px, x, y, width, thickness, rgb);
    fill_rect(
        buffer,
        stride_px,
        x,
        y.saturating_add(height.saturating_sub(thickness)),
        width,
        thickness,
        rgb,
    );
    fill_rect(buffer, stride_px, x, y, thickness, height, rgb);
    fill_rect(
        buffer,
        stride_px,
        x.saturating_add(width.saturating_add(0).saturating_sub(thickness)),
        y,
        thickness,
        height,
        rgb,
    );
}

/// Paint structural pane chrome after terminal cells.
///
/// Focus uses a **thin** (1px) outline in the chosen brand spectrum color;
/// unfocused panes use neutral `PANE_BORDER`. Thickness is not used to encode
/// focus (user-request: thin borders).
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn rasterize_pane_chrome(
    buffer: &mut [u32],
    stride_px: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    focused: bool,
    unseen_output: bool,
    // active_pulse: Some(shared 0..1 pulse phase) while the pane is actively
    // producing output; all panes breathe in sync by design.
    active_pulse: Option<f32>,
    // light_cycle: Some(0..1 sweep progress) mid focus-change animation on the
    // focused pane; the border is traced clockwise from the top-left instead
    // of appearing at once. None = static border (the default and the >=1.0
    // terminal state). light_cycle_head draws the bright vehicle box at the
    // leading edge (user-toggled; trail-only when false).
    light_cycle: Option<f32>,
    light_cycle_head: bool,
    focus_rgb: [u8; 3],
) {
    rasterize_pane_chrome_with_theme(
        default_theme(),
        buffer,
        stride_px,
        x,
        y,
        width,
        height,
        focused,
        unseen_output,
        active_pulse,
        light_cycle,
        light_cycle_head,
        focus_rgb,
        OPAQUE_ALPHA,
    );
}

/// Themed live-host variant of [`rasterize_pane_chrome`].
#[allow(clippy::too_many_arguments)]
pub fn rasterize_pane_chrome_with_theme(
    theme: &Theme,
    buffer: &mut [u32],
    stride_px: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    focused: bool,
    unseen_output: bool,
    active_pulse: Option<f32>,
    light_cycle: Option<f32>,
    light_cycle_head: bool,
    focus_rgb: [u8; 3],
    border_alpha: u8,
) {
    if width == 0 || height == 0 {
        return;
    }
    let sweeping = focused && light_cycle.is_some_and(|progress| progress < 1.0);
    // Mid-sweep the not-yet-traced remainder stays neutral, as if unfocused.
    let color = if focused && !sweeping {
        focus_rgb
    } else {
        theme.pane_border
    };
    let thickness = 1usize.min(width).min(height);
    fill_rect_argb(
        buffer,
        stride_px,
        x,
        y,
        width,
        thickness,
        color,
        border_alpha,
    );
    fill_rect_argb(
        buffer,
        stride_px,
        x,
        y.saturating_add(height.saturating_sub(thickness)),
        width,
        thickness,
        color,
        border_alpha,
    );
    fill_rect_argb(
        buffer,
        stride_px,
        x,
        y,
        thickness,
        height,
        color,
        border_alpha,
    );
    fill_rect_argb(
        buffer,
        stride_px,
        x.saturating_add(width.saturating_sub(thickness)),
        y,
        thickness,
        height,
        color,
        border_alpha,
    );
    if sweeping {
        if let Some(progress) = light_cycle {
            trace_border_trail(
                buffer,
                stride_px,
                x,
                y,
                width,
                height,
                progress,
                light_cycle_head,
                focus_rgb,
            );
        }
    }

    // The latched "!" badge sits left of the corner slot, which belongs to
    // the live-activity dot.
    if unseen_output && width >= 24 && height >= 10 {
        let badge_size = 9;
        let badge_x = x.saturating_add(width.saturating_sub(badge_size + 11));
        let badge_y = y.saturating_add(3);
        fill_rect(
            buffer,
            stride_px,
            badge_x,
            badge_y,
            badge_size,
            badge_size,
            theme.unseen_badge,
        );
        // High-contrast exclamation mark makes the badge shape meaningful.
        fill_rect(
            buffer,
            stride_px,
            badge_x + 4,
            badge_y + 2,
            1,
            4,
            theme.chrome_bg,
        );
        fill_rect(
            buffer,
            stride_px,
            badge_x + 4,
            badge_y + 7,
            1,
            1,
            theme.chrome_bg,
        );
    }

    // Small solid dot in the corner slot: output is flowing right now.
    // Distinct shape and hue from the "!" badge (which is latched, not live).
    // It "breathes" with the shared pulse: full green at the peak, ~40%
    // toward the pane background at the trough — soft pulse, never a blink.
    if let Some(phase) = active_pulse {
        if width >= 10 && height >= 10 {
            let dot_size = 5;
            let dot_x = x.saturating_add(width.saturating_sub(dot_size + 3));
            let dot_y = y.saturating_add(5);
            fill_rect(
                buffer,
                stride_px,
                dot_x,
                dot_y,
                dot_size,
                dot_size,
                pulse_color_with_theme(theme, phase),
            );
        }
    }
}

/// Pixel overlay AFTER the guest raster (ADR-0010). Does not steal a cell.
/// Lights for MailAttention `depth > 0`, including the focused pane.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn rasterize_mail_letter(
    buffer: &mut [u32],
    stride_px: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    mail: bool,
    amber_focus: bool,
) {
    rasterize_mail_letter_with_theme(
        default_theme(),
        buffer,
        stride_px,
        x,
        y,
        width,
        height,
        mail,
        amber_focus,
    );
}

/// Themed live-host variant of [`rasterize_mail_letter`].
#[allow(clippy::too_many_arguments)]
pub fn rasterize_mail_letter_with_theme(
    theme: &Theme,
    buffer: &mut [u32],
    stride_px: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    mail: bool,
    amber_focus: bool,
) {
    if !mail
        || width < MAIL_RING_INSET + MAIL_GLYPH_W + 2
        || height < MAIL_RING_INSET + MAIL_GLYPH_H + 2
    {
        return;
    }
    let glyph_x = x.saturating_add(MAIL_RING_INSET);
    let glyph_y = y.saturating_add(MAIL_RING_INSET);
    let ink = if amber_focus {
        theme.chrome_fg
    } else {
        theme.mail_letter
    };
    if amber_focus {
        fill_rect(
            buffer,
            stride_px,
            glyph_x.saturating_sub(1),
            glyph_y.saturating_sub(1),
            MAIL_GLYPH_W + 2,
            MAIL_GLYPH_H + 2,
            theme.chrome_bg,
        );
    }
    for (row, bits) in MAIL_ENVELOPE.iter().enumerate() {
        for (col, pixel) in bits.iter().enumerate() {
            if *pixel == 0 {
                continue;
            }
            let px = glyph_x.saturating_add(col);
            let py = glyph_y.saturating_add(row);
            if px >= x.saturating_add(width) || py >= y.saturating_add(height) {
                continue;
            }
            fill_rect(buffer, stride_px, px, py, 1, 1, ink);
        }
    }
}

/// Trace the first `progress` (0..1) of the pane border clockwise from the
/// top-left corner in the focus color, with a brightened "light cycle" head
/// at the leading edge. The untraced remainder was already painted neutral.
#[allow(clippy::too_many_arguments)]
fn trace_border_trail(
    buffer: &mut [u32],
    stride_px: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    progress: f32,
    head: bool,
    focus_rgb: [u8; 3],
) {
    let perimeter = 2 * (width + height);
    let traced = (progress.clamp(0.0, 1.0) * perimeter as f32) as usize;
    if traced == 0 {
        return;
    }
    // Clockwise segments: top L->R, right T->B, bottom R->L, left B->T.
    // Corner pixels overlap between segments; visually irrelevant at 1px.
    let top = traced.min(width);
    fill_rect(buffer, stride_px, x, y, top, 1, focus_rgb);
    let right = traced.saturating_sub(width).min(height);
    fill_rect(
        buffer,
        stride_px,
        x.saturating_add(width.saturating_sub(1)),
        y,
        1,
        right,
        focus_rgb,
    );
    let bottom = traced.saturating_sub(width + height).min(width);
    fill_rect(
        buffer,
        stride_px,
        x.saturating_add(width.saturating_sub(bottom)),
        y.saturating_add(height.saturating_sub(1)),
        bottom,
        1,
        focus_rgb,
    );
    let left = traced.saturating_sub(2 * width + height).min(height);
    fill_rect(
        buffer,
        stride_px,
        x,
        y.saturating_add(height.saturating_sub(left)),
        1,
        left,
        focus_rgb,
    );

    if !head {
        return;
    }
    // Head: the "light cycle" itself — a visible 7x7 box at the leading edge:
    // focus-color shell with a near-white core so it reads as a vehicle
    // against both the 1px trail and the neutral remainder.
    let head_rgb = [
        lerp_channel(focus_rgb[0]),
        lerp_channel(focus_rgb[1]),
        lerp_channel(focus_rgb[2]),
    ];
    let (head_x, head_y) = if traced <= width {
        (x + traced.saturating_sub(1), y)
    } else if traced <= width + height {
        (
            x + width.saturating_sub(1),
            y + (traced - width).saturating_sub(1),
        )
    } else if traced <= 2 * width + height {
        (
            x + width.saturating_sub(traced - width - height),
            y + height.saturating_sub(1),
        )
    } else {
        (x, y + height.saturating_sub(traced - 2 * width - height))
    };
    let head_x = head_x.saturating_sub(3).max(x);
    let head_y = head_y.saturating_sub(3).max(y);
    let head_w = 7usize.min(x.saturating_add(width).saturating_sub(head_x));
    let head_h = 7usize.min(y.saturating_add(height).saturating_sub(head_y));
    fill_rect(buffer, stride_px, head_x, head_y, head_w, head_h, focus_rgb);
    if head_w > 2 && head_h > 2 {
        fill_rect(
            buffer,
            stride_px,
            head_x + 1,
            head_y + 1,
            head_w - 2,
            head_h - 2,
            head_rgb,
        );
    }
}

/// 65% of the way from the channel toward full white — the light-cycle head.
fn lerp_channel(channel: u8) -> u8 {
    (f32::from(channel) + (255.0 - f32::from(channel)) * 0.65).round() as u8
}

/// Dot color for pulse `phase` (0..1): sine brightness in 0.6..=1.0 between
/// the pane background and `ACTIVE_BADGE`. Phase 0.25 is the exact badge color.
#[cfg(test)]
fn pulse_color(phase: f32) -> [u8; 3] {
    pulse_color_with_theme(default_theme(), phase)
}

/// Mix `base` toward `toward` on the pulse sine (brightness 0.6..=1.0).
pub(crate) fn pulse_mix(base: [u8; 3], toward: [u8; 3], phase: f32) -> [u8; 3] {
    let wave = (phase.rem_euclid(1.0) * std::f32::consts::TAU).sin();
    let brightness = 0.6 + 0.4 * (0.5 * (1.0 + wave));
    let mix = |from: u8, to: u8| -> u8 {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * brightness).round() as u8
    };
    [
        mix(base[0], toward[0]),
        mix(base[1], toward[1]),
        mix(base[2], toward[2]),
    ]
}

fn pulse_color_with_theme(theme: &Theme, phase: f32) -> [u8; 3] {
    pulse_mix(theme.default_bg, theme.active_badge, phase)
}

/// Attach mail letter (`nf-fa-envelope`). Sits above the mono baseline and
/// clips at the pane origin unless we fit and pin it to the cell corner.
const MAIL_LETTER_GLYPH: char = '\u{F0E0}';

fn blit_glyph(
    buffer: &mut [u32],
    stride: usize,
    font: &FontMetrics,
    ch: char,
    cell_x: usize,
    cell_y: usize,
    fg: [u8; 3],
) {
    let mail = ch == MAIL_LETTER_GLYPH;
    blit_glyph_in(
        buffer,
        stride,
        font,
        ch,
        cell_x,
        cell_y,
        fg,
        0,
        usize::MAX,
        mail,
    );
}

#[allow(clippy::too_many_arguments)]
fn blit_cluster_in(
    buffer: &mut [u32],
    stride: usize,
    font: &FontMetrics,
    cluster: &str,
    cell_x: usize,
    cell_y: usize,
    fg: [u8; 3],
    clip_min_x: usize,
    clip_max_x: usize,
    pin_nw: bool,
) {
    if let Some(g) = font.paint_emoji_cluster(cluster) {
        // RI flag pairs are two width-1 scalars that occupy one width-2 cell.
        // Measuring only the first scalar clips the flag glyph in half.
        let wide = cluster_occupies_two_cells(cluster);
        let clip_min = i32::try_from(clip_min_x).unwrap_or(0);
        let clip_max = i32::try_from(clip_max_x).unwrap_or(i32::MAX);
        let clip_x0 = (cell_x as i32).max(clip_min);
        let clip_y0 = cell_y as i32;
        let cell_w = font.cell_w as i32 * if wide { 2 } else { 1 };
        let clip_x1 = (cell_x as i32 + cell_w).min(clip_max);
        let clip_y1 = clip_y0 + font.cell_h as i32 + 1;
        let ox = cell_x as i32 + (cell_w - g.width as i32) / 2;
        let oy = cell_y as i32 + g.oy;
        blit_rgba(
            buffer, stride, &g.rgba, g.width, g.height, ox, oy, clip_x0, clip_y0, clip_x1, clip_y1,
        );
        return;
    }
    let ch = cluster.chars().next().unwrap_or('\0');
    blit_glyph_in(
        buffer, stride, font, ch, cell_x, cell_y, fg, clip_min_x, clip_max_x, pin_nw,
    );
}

#[allow(clippy::too_many_arguments)]
fn blit_glyph_in(
    buffer: &mut [u32],
    stride: usize,
    font: &FontMetrics,
    ch: char,
    cell_x: usize,
    cell_y: usize,
    fg: [u8; 3],
    clip_min_x: usize,
    clip_max_x: usize,
    pin_nw: bool,
) {
    // Wide emoji may spill one cell to the right (continuation half is skipped by caller).
    let wide = prismattyc_core::char_display_width(ch) >= 2;
    let clip_min = i32::try_from(clip_min_x).unwrap_or(0);
    let clip_max = i32::try_from(clip_max_x).unwrap_or(i32::MAX);
    let clip_x0 = (cell_x as i32).max(clip_min);
    let clip_y0 = cell_y as i32;
    let cell_w = font.cell_w as i32 * if wide { 2 } else { 1 };
    let cell_end = cell_x as i32 + cell_w;
    let clip_x1 = cell_end.min(clip_max);
    let clip_y1 = clip_y0 + font.cell_h as i32 + 1;

    // Procedural sprites for box, block, braille, powerline triangles,
    // sextants, and corner triangles. Font outlines leave seams (Kiro
    // braille banner, Claude ─ composer, powerline prompts).
    if let Some(cov) = sprite_coverage(ch, font.cell_w, font.cell_h) {
        blit_coverage(
            buffer,
            stride,
            &cov,
            font.cell_w,
            font.cell_h,
            cell_x as i32,
            cell_y as i32,
            fg,
            clip_x0,
            clip_y0,
            clip_x1,
            clip_y1,
        );
        return;
    }

    // Scale + shift any outline that would clip the cell. Nerd title
    // spinners and Grok Working frames (⸬ / ⁙) otherwise lose ink.
    let paint = font.paint_char_fitting(ch, cell_w, font.cell_h as i32);
    match paint {
        GlyphPaint::Empty => {}
        GlyphPaint::Coverage(g) => {
            let mut ox = cell_x as i32 + g.xmin;
            let mut oy = cell_y as i32 + font.coverage_origin_y(&g);
            if pin_nw {
                // Ignore baseline / xmin so the letter sits in the cell corner.
                ox = clip_x0;
                oy = clip_y0;
            }
            (ox, oy) = shift_ink_into_clip(
                ox,
                oy,
                g.width as i32,
                g.height as i32,
                [clip_x0, clip_y0, clip_x1, clip_y1],
            );
            blit_coverage(
                buffer, stride, &g.bitmap, g.width, g.height, ox, oy, fg, clip_x0, clip_y0,
                clip_x1, clip_y1,
            );
        }
        GlyphPaint::Color(g) => {
            let ox = cell_x as i32 + g.ox;
            let oy = cell_y as i32 + g.oy;
            blit_rgba(
                buffer, stride, &g.rgba, g.width, g.height, ox, oy, clip_x0, clip_y0, clip_x1,
                clip_y1,
            );
        }
    }
}

/// Slide a bitmap so its ink stays inside the clip box. If it is taller
/// (or wider) than the box, pin to the min edge and let the far side clip.
fn shift_ink_into_clip(
    mut ox: i32,
    mut oy: i32,
    width: i32,
    height: i32,
    clip: [i32; 4],
) -> (i32, i32) {
    let [clip_x0, clip_y0, clip_x1, clip_y1] = clip;
    if oy < clip_y0 {
        oy = clip_y0;
    }
    if oy + height > clip_y1 {
        oy = clip_y1.saturating_sub(height).max(clip_y0);
    }
    if ox < clip_x0 {
        ox = clip_x0;
    }
    if ox + width > clip_x1 {
        ox = clip_x1.saturating_sub(width).max(clip_x0);
    }
    (ox, oy)
}

/// Quadrant fill mask `[top_left, top_right, bottom_left, bottom_right]` for
/// the block quadrant characters U+2596..=U+259F.
fn block_quadrant_mask(cp: u32) -> [bool; 4] {
    match cp {
        0x2596 => [false, false, true, false], // lower left
        0x2597 => [false, false, false, true], // lower right
        0x2598 => [true, false, false, false], // upper left
        0x2599 => [true, false, true, true],   // upper left + both lower
        0x259A => [true, false, false, true],  // upper left + lower right
        0x259B => [true, true, true, false],   // both upper + lower left
        0x259C => [true, true, false, true],   // both upper + lower right
        0x259D => [false, true, false, false], // upper right
        0x259E => [false, true, true, false],  // upper right + lower left
        0x259F => [false, true, true, true],   // upper right + both lower
        _ => [false; 4],
    }
}

/// Coverage bitmap (`cell_w * cell_h`, row-major alpha) for the block-element
/// glyphs U+2580..=U+259F, or `None` for any other character.
///
/// Fills are computed on the exact cell grid so neighboring cells tile without
/// seams. Shade characters use partial alpha; every other block is opaque.
fn block_element_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x2580..=0x259F).contains(&cp) || cell_w == 0 || cell_h == 0 {
        return None;
    }
    let mut cov = vec![0u8; cell_w * cell_h];
    let mut fill = |x0: usize, x1: usize, y0: usize, y1: usize, a: u8| {
        for y in y0..y1 {
            for x in x0..x1 {
                cov[y * cell_w + x] = a;
            }
        }
    };
    // Shared boundaries; rounding is consistent so adjacent cells align.
    let hw = ((cell_w as f32) / 2.0).round() as usize;
    let hh = ((cell_h as f32) / 2.0).round() as usize;
    let eighth_w = |n: usize| (((n as f32) / 8.0) * cell_w as f32).round() as usize;
    let eighth_h = |n: usize| (((n as f32) / 8.0) * cell_h as f32).round() as usize;

    match cp {
        0x2580 => fill(0, cell_w, 0, hh, 255),      // upper half
        0x2588 => fill(0, cell_w, 0, cell_h, 255),  // full block
        0x2590 => fill(hw, cell_w, 0, cell_h, 255), // right half
        0x2581..=0x2587 => {
            // Lower n/8 block (2581 = 1/8 ... 2587 = 7/8), filled from bottom.
            let y0 = cell_h - eighth_h((cp - 0x2580) as usize);
            fill(0, cell_w, y0, cell_h, 255);
        }
        0x2589..=0x258F => {
            // Left n/8 block (2589 = 7/8 ... 258F = 1/8), filled from left.
            let x1 = eighth_w((0x2590 - cp) as usize);
            fill(0, x1, 0, cell_h, 255);
        }
        0x2594 => fill(0, cell_w, 0, eighth_h(1), 255), // upper 1/8
        0x2595 => fill(cell_w - eighth_w(1), cell_w, 0, cell_h, 255), // right 1/8
        0x2591 => fill(0, cell_w, 0, cell_h, 64),       // light shade  (~25%)
        0x2592 => fill(0, cell_w, 0, cell_h, 128),      // medium shade (~50%)
        0x2593 => fill(0, cell_w, 0, cell_h, 192),      // dark shade   (~75%)
        0x2596..=0x259F => {
            let [tl, tr, bl, br] = block_quadrant_mask(cp);
            if tl {
                fill(0, hw, 0, hh, 255);
            }
            if tr {
                fill(hw, cell_w, 0, hh, 255);
            }
            if bl {
                fill(0, hw, hh, cell_h, 255);
            }
            if br {
                fill(hw, cell_w, hh, cell_h, 255);
            }
        }
        _ => return None,
    }
    Some(cov)
}

fn box_light_stroke(cell_w: usize, cell_h: usize) -> usize {
    (cell_h / 16).max(1).min(cell_w.max(1))
}

fn box_dash_ranges(length: usize, dash_count: usize) -> Vec<(usize, usize)> {
    let slots = dash_count.saturating_mul(2).saturating_sub(1).max(1);
    (0..dash_count)
        .filter_map(|dash| {
            let start = dash.saturating_mul(2) * length / slots;
            let end = ((dash.saturating_mul(2) + 1) * length).div_ceil(slots);
            (start < end).then_some((start, end.min(length)))
        })
        .collect()
}

/// Procedural dashed box lines. Keep both ends painted so adjacent cells do
/// not show a gap at narrow widths.
fn box_dashed_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    let (horizontal, heavy, dash_count) = match cp {
        0x2504 => (true, false, 3),  // ┄ light triple dash
        0x2505 => (true, true, 3),   // ┅ heavy triple dash
        0x2506 => (false, false, 3), // ┆ light triple dash
        0x2507 => (false, true, 3),  // ┇ heavy triple dash
        0x2508 => (true, false, 4),  // ┈ light quadruple dash
        0x2509 => (true, true, 4),   // ┉ heavy quadruple dash
        0x250A => (false, false, 4), // ┊ light quadruple dash
        0x250B => (false, true, 4),  // ┋ heavy quadruple dash
        0x254C => (true, false, 2),  // ╌ light double dash
        0x254D => (true, true, 2),   // ╍ heavy double dash
        0x254E => (false, false, 2), // ╎ light double dash
        0x254F => (false, true, 2),  // ╏ heavy double dash
        _ => return None,
    };
    let light = box_light_stroke(cell_w, cell_h);
    let thickness = if heavy {
        (light * 2).min(if horizontal { cell_h } else { cell_w })
    } else {
        light
    };
    let mut cov = vec![0u8; cell_w * cell_h];
    if horizontal {
        let y0 = (cell_h.saturating_sub(thickness)) / 2;
        let y1 = (y0 + thickness).min(cell_h);
        for (x0, x1) in box_dash_ranges(cell_w, dash_count) {
            for y in y0..y1 {
                for x in x0..x1 {
                    cov[y * cell_w + x] = 255;
                }
            }
        }
    } else {
        let x0 = (cell_w.saturating_sub(thickness)) / 2;
        let x1 = (x0 + thickness).min(cell_w);
        for (y0, y1) in box_dash_ranges(cell_h, dash_count) {
            for y in y0..y1 {
                for x in x0..x1 {
                    cov[y * cell_w + x] = 255;
                }
            }
        }
    }
    Some(cov)
}

/// Box-drawing U+2500..=U+257F drawn on the cell grid (Ghostty
/// `src/font/sprite/draw/box.zig`). Font glyphs of ─ do not span the full
/// cell, so a row of them looks dashed; procedural strokes tile solid.
fn box_drawing_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x2500..=0x257F).contains(&cp) || cell_w == 0 || cell_h == 0 {
        return None;
    }
    if let Some(cov) = box_arc_coverage(ch, cell_w, cell_h) {
        return Some(cov);
    }
    if let Some(cov) = box_dashed_coverage(ch, cell_w, cell_h) {
        return Some(cov);
    }
    let lines = box_drawing_lines(cp)?;
    let light = box_light_stroke(cell_w, cell_h);
    let heavy = (light * 2).min(cell_h.max(1));
    let mut cov = vec![0u8; cell_w * cell_h];
    let mut fill = |x0: usize, x1: usize, y0: usize, y1: usize| {
        let x1 = x1.min(cell_w);
        let y1 = y1.min(cell_h);
        for y in y0..y1 {
            for x in x0..x1 {
                cov[y * cell_w + x] = 255;
            }
        }
    };
    let h_light_top = (cell_h.saturating_sub(light)) / 2;
    let h_light_bot = (h_light_top + light).min(cell_h);
    let h_heavy_top = (cell_h.saturating_sub(heavy)) / 2;
    let h_heavy_bot = (h_heavy_top + heavy).min(cell_h);
    let h_dbl_top = h_light_top.saturating_sub(light);
    let h_dbl_bot = (h_light_bot + light).min(cell_h);
    let v_light_l = (cell_w.saturating_sub(light)) / 2;
    let v_light_r = (v_light_l + light).min(cell_w);
    let v_heavy_l = (cell_w.saturating_sub(heavy)) / 2;
    let v_heavy_r = (v_heavy_l + heavy).min(cell_w);
    let v_dbl_l = v_light_l.saturating_sub(light);
    let v_dbl_r = (v_light_r + light).min(cell_w);

    let any_h_heavy = matches!(lines.left, Stroke::Heavy) || matches!(lines.right, Stroke::Heavy);
    let any_v_heavy = matches!(lines.up, Stroke::Heavy) || matches!(lines.down, Stroke::Heavy);
    let up_bottom = if any_h_heavy {
        h_heavy_bot
    } else if matches!(lines.left, Stroke::Double) || matches!(lines.right, Stroke::Double) {
        h_dbl_bot
    } else {
        h_light_bot
    };
    let down_top = if any_h_heavy {
        h_heavy_top
    } else if matches!(lines.left, Stroke::Double) || matches!(lines.right, Stroke::Double) {
        h_dbl_top
    } else {
        h_light_top
    };
    let left_right = if any_v_heavy {
        v_heavy_r
    } else if matches!(lines.up, Stroke::Double) || matches!(lines.down, Stroke::Double) {
        v_dbl_r
    } else {
        v_light_r
    };
    let right_left = if any_v_heavy {
        v_heavy_l
    } else if matches!(lines.up, Stroke::Double) || matches!(lines.down, Stroke::Double) {
        v_dbl_l
    } else {
        v_light_l
    };

    match lines.up {
        Stroke::None => {}
        Stroke::Light => fill(v_light_l, v_light_r, 0, up_bottom),
        Stroke::Heavy => fill(v_heavy_l, v_heavy_r, 0, up_bottom),
        Stroke::Double => {
            fill(v_dbl_l, v_light_l, 0, up_bottom);
            fill(v_light_r, v_dbl_r, 0, up_bottom);
        }
    }
    match lines.down {
        Stroke::None => {}
        Stroke::Light => fill(v_light_l, v_light_r, down_top, cell_h),
        Stroke::Heavy => fill(v_heavy_l, v_heavy_r, down_top, cell_h),
        Stroke::Double => {
            fill(v_dbl_l, v_light_l, down_top, cell_h);
            fill(v_light_r, v_dbl_r, down_top, cell_h);
        }
    }
    match lines.left {
        Stroke::None => {}
        Stroke::Light => fill(0, left_right, h_light_top, h_light_bot),
        Stroke::Heavy => fill(0, left_right, h_heavy_top, h_heavy_bot),
        Stroke::Double => {
            fill(0, left_right, h_dbl_top, h_light_top);
            fill(0, left_right, h_light_bot, h_dbl_bot);
        }
    }
    match lines.right {
        Stroke::None => {}
        Stroke::Light => fill(right_left, cell_w, h_light_top, h_light_bot),
        Stroke::Heavy => fill(right_left, cell_w, h_heavy_top, h_heavy_bot),
        Stroke::Double => {
            fill(right_left, cell_w, h_dbl_top, h_light_top);
            fill(right_left, cell_w, h_light_bot, h_dbl_bot);
        }
    }
    Some(cov)
}

fn arc_span_start(coord: f64, limit: usize, thickness: usize) -> usize {
    if limit == 0 || thickness >= limit {
        return 0;
    }
    ((coord - thickness as f64 / 2.0).round() as isize).clamp(0, (limit - thickness) as isize)
        as usize
}

fn paint_arc_line(
    cov: &mut [u8],
    cell_w: usize,
    cell_h: usize,
    from: (usize, usize),
    to: (usize, usize),
    thickness: usize,
) {
    let (mut x, mut y) = from;
    loop {
        let x1 = x.saturating_add(thickness).min(cell_w);
        let y1 = y.saturating_add(thickness).min(cell_h);
        for py in y..y1 {
            for px in x..x1 {
                cov[py * cell_w + px] = 255;
            }
        }
        if (x, y) == to {
            break;
        }
        if x != to.0 {
            x = if x < to.0 { x + 1 } else { x - 1 };
        } else if y != to.1 {
            y = if y < to.1 { y + 1 } else { y - 1 };
        }
    }
}

/// Procedural light arcs. The arc joins the same horizontal and vertical
/// stroke centres as `─` and `│`, then continues to the cell edges.
fn box_arc_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let (corner_x, corner_y) = match ch {
        '╭' => (1.0, 1.0),
        '╮' => (-1.0, 1.0),
        '╯' => (-1.0, -1.0),
        '╰' => (1.0, -1.0),
        _ => return None,
    };
    let light = box_light_stroke(cell_w, cell_h);
    let mut cov = vec![0u8; cell_w * cell_h];
    let x0 = (cell_w.saturating_sub(light)) / 2;
    let y0 = (cell_h.saturating_sub(light)) / 2;
    let x1 = (x0 + light).min(cell_w);
    let y1 = (y0 + light).min(cell_h);
    let cx = (x0 + x1) as f64 / 2.0;
    let cy = (y0 + y1) as f64 / 2.0;
    let radius = cx.min(cell_w as f64 - cx).min(cy).min(cell_h as f64 - cy);
    if radius <= 0.0 {
        return Some(cov);
    }
    let arc_cx = cx + corner_x * radius;
    let arc_cy = cy + corner_y * radius;
    // Rasterize a quarter-circle as a deterministic 4-connected stair path.
    // The stroke remains binary, and every diagonal transition gets an
    // orthogonal bridge instead of relying on corner-touching pixels.
    let steps = ((radius * std::f64::consts::FRAC_PI_2).ceil() as usize * 4).max(1);
    let mut previous = None;
    for step in 0..=steps {
        let angle = std::f64::consts::FRAC_PI_2 * step as f64 / steps as f64;
        let px = arc_span_start(arc_cx - corner_x * radius * angle.cos(), cell_w, light);
        let py = arc_span_start(arc_cy - corner_y * radius * angle.sin(), cell_h, light);
        if let Some(last) = previous {
            paint_arc_line(&mut cov, cell_w, cell_h, last, (px, py), light);
        } else {
            paint_arc_line(&mut cov, cell_w, cell_h, (px, py), (px, py), light);
        }
        previous = Some((px, py));
    }
    let fill = |cov: &mut [u8], xa: usize, xb: usize, ya: usize, yb: usize| {
        for y in ya.min(cell_h)..yb.min(cell_h) {
            for x in xa.min(cell_w)..xb.min(cell_w) {
                cov[y * cell_w + x] = 255;
            }
        }
    };
    match ch {
        '╭' => {
            fill(
                &mut cov,
                (arc_cx.ceil() as usize).min(cell_w),
                cell_w,
                y0,
                y1,
            );
            fill(&mut cov, x0, x1, arc_cy.ceil() as usize, cell_h);
        }
        '╮' => {
            fill(&mut cov, 0, arc_cx.floor() as usize + 1, y0, y1);
            fill(&mut cov, x0, x1, arc_cy.ceil() as usize, cell_h);
        }
        '╯' => {
            fill(&mut cov, 0, arc_cx.floor() as usize + 1, y0, y1);
            fill(&mut cov, x0, x1, 0, arc_cy.floor() as usize + 1);
        }
        '╰' => {
            fill(
                &mut cov,
                (arc_cx.ceil() as usize).min(cell_w),
                cell_w,
                y0,
                y1,
            );
            fill(&mut cov, x0, x1, 0, arc_cy.floor() as usize + 1);
        }
        _ => unreachable!(),
    }
    Some(cov)
}

#[derive(Clone, Copy)]
enum Stroke {
    None,
    Light,
    Heavy,
    Double,
}

struct BoxLines {
    up: Stroke,
    right: Stroke,
    down: Stroke,
    left: Stroke,
}

fn box_drawing_lines(cp: u32) -> Option<BoxLines> {
    // Ghostty `draw2500_257F` / `linesChar`. Dashed lines are handled by
    // `box_dashed_coverage`; arcs use their own deterministic rasterizer.
    use Stroke::{Double, Heavy, Light, None as No};
    let (up, right, down, left) = match cp {
        0x2500 => (No, Light, No, Light),
        0x2501 => (No, Heavy, No, Heavy),
        0x2502 => (Light, No, Light, No),
        0x2503 => (Heavy, No, Heavy, No),
        0x250C => (No, Light, Light, No),
        0x250F => (No, Heavy, Heavy, No),
        0x2510 => (No, No, Light, Light),
        0x2513 => (No, No, Heavy, Heavy),
        0x2514 => (Light, Light, No, No),
        0x2517 => (Heavy, Heavy, No, No),
        0x2518 => (Light, No, No, Light),
        0x251B => (Heavy, No, No, Heavy),
        0x251C => (Light, Light, Light, No),
        0x2523 => (Heavy, Heavy, Heavy, No),
        0x2524 => (Light, No, Light, Light),
        0x252B => (Heavy, No, Heavy, Heavy),
        0x252C => (No, Light, Light, Light),
        0x2533 => (No, Heavy, Heavy, Heavy),
        0x2534 => (Light, Light, No, Light),
        0x253B => (Heavy, Heavy, No, Heavy),
        0x253C => (Light, Light, Light, Light),
        0x254B => (Heavy, Heavy, Heavy, Heavy),
        0x2550 => (No, Double, No, Double),
        0x2551 => (Double, No, Double, No),
        0x2554 => (No, Double, Double, No),
        0x2557 => (No, No, Double, Double),
        0x255A => (Double, Double, No, No),
        0x255D => (Double, No, No, Double),
        0x2560 => (Double, Double, Double, No),
        0x2563 => (Double, No, Double, Double),
        0x2566 => (No, Double, Double, Double),
        0x2569 => (Double, Double, No, Double),
        0x256C => (Double, Double, Double, Double),
        0x2574 => (No, No, No, Light),
        0x2575 => (Light, No, No, No),
        0x2576 => (No, Light, No, No),
        0x2577 => (No, No, Light, No),
        0x2578 => (No, No, No, Heavy),
        0x2579 => (Heavy, No, No, No),
        0x257A => (No, Heavy, No, No),
        0x257B => (No, No, Heavy, No),
        _ => return None,
    };
    Some(BoxLines {
        up,
        right,
        down,
        left,
    })
}

fn is_box_dashed(cp: u32) -> bool {
    matches!(
        cp,
        0x2504..=0x250B | 0x254C..=0x254F
    )
}

/// Return whether `ch` is rendered by a procedural cell sprite.
///
/// Keep this predicate dimension-free so ligature metadata can classify a
/// cell without allocating a coverage bitmap. It must stay in sync with the
/// dispatch order in `sprite_coverage`.
fn is_sprite(ch: char) -> bool {
    let cp = ch as u32;
    (0x2580..=0x259F).contains(&cp)
        || (0x2500..=0x257F).contains(&cp)
            && (matches!(ch, '╭' | '╮' | '╯' | '╰')
                || box_drawing_lines(cp).is_some()
                || is_box_dashed(cp))
        || (0x2800..=0x28FF).contains(&cp)
        || matches!(cp, 0xE0B0 | 0xE0B2 | 0xE0B8 | 0xE0BA | 0xE0BC | 0xE0BE)
        || (0x1FB00..=0x1FB3B).contains(&cp)
        || (0x25E2..=0x25E5).contains(&cp)
}

/// Procedural cell sprites for sets that look broken as font outlines.
///
/// We cover: box (U+2500), light arcs (U+256D..=U+2570), block (U+2580),
/// braille (U+2800), powerline triangles (U+E0B0), sextants (U+1FB00),
/// corner triangles (U+25E2), and dashed box lines. Still font-drawn:
/// powerline arcs, git-branch nerd icons, octants, smooth mosaics.
fn sprite_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    block_element_coverage(ch, cell_w, cell_h)
        .or_else(|| box_drawing_coverage(ch, cell_w, cell_h))
        .or_else(|| braille_coverage(ch, cell_w, cell_h))
        .or_else(|| powerline_triangle_coverage(ch, cell_w, cell_h))
        .or_else(|| sextant_coverage(ch, cell_w, cell_h))
        .or_else(|| corner_triangle_coverage(ch, cell_w, cell_h))
}

/// Braille Patterns U+2800..=U+28FF as eight dots on a 2×4 cell partition.
///
/// Each on-bit paints a square centered in its tile, inset 1px when the
/// tile is large enough. Adjacent cells then share the same gutter as
/// dots inside a cell (the lattice on Kiro banners was extra cell-edge
/// margin). Filling the whole tile hides the dots; leftover-first
/// spacing made the gutter grow with cell size.
fn braille_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x2800..=0x28FF).contains(&cp) || cell_w == 0 || cell_h == 0 {
        return None;
    }
    let x = [0, cell_w / 2, cell_w];
    let y = [0, cell_h / 4, cell_h / 2, (cell_h * 3) / 4, cell_h];
    let bits = (cp - 0x2800) as u8;
    // Bit order is the Unicode braille layout: left column 1,2,3,7 then
    // right column 4,5,6,8.
    let tiles = [
        (0, 0, bits & 0x01 != 0),
        (0, 1, bits & 0x02 != 0),
        (0, 2, bits & 0x04 != 0),
        (1, 0, bits & 0x08 != 0),
        (1, 1, bits & 0x10 != 0),
        (1, 2, bits & 0x20 != 0),
        (0, 3, bits & 0x40 != 0),
        (1, 3, bits & 0x80 != 0),
    ];
    let mut cov = vec![0u8; cell_w * cell_h];
    for (col, row, on) in tiles {
        if !on {
            continue;
        }
        fill_braille_dot(&mut cov, cell_w, x[col], x[col + 1], y[row], y[row + 1]);
    }
    Some(cov)
}

fn fill_braille_dot(cov: &mut [u8], cell_w: usize, x0: usize, x1: usize, y0: usize, y1: usize) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let tw = x1 - x0;
    let th = y1 - y0;
    let inset = usize::from(tw >= 3 && th >= 3);
    let dw = tw.saturating_sub(inset * 2).max(1);
    let dh = th.saturating_sub(inset * 2).max(1);
    let x_start = x0 + (tw.saturating_sub(dw)) / 2;
    let y_start = y0 + (th.saturating_sub(dh)) / 2;
    for py in y_start..(y_start + dh).min(y1) {
        for px in x_start..(x_start + dw).min(x1) {
            cov[py * cell_w + px] = 255;
        }
    }
}

/// Powerline filled triangles that must tile (U+E0B0/E0B2/E0B8/E0BA/E0BC/E0BE).
/// Outlines and arcs stay font-drawn.
fn powerline_triangle_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    let w = cell_w as f32;
    let h = cell_h as f32;
    let verts = match cp {
        0xE0B0 => [(0.0, 0.0), (w, h / 2.0), (0.0, h)],
        0xE0B2 => [(w, 0.0), (0.0, h / 2.0), (w, h)],
        0xE0B8 => [(0.0, 0.0), (w, h), (0.0, h)],
        0xE0BA => [(w, 0.0), (w, h), (0.0, h)],
        0xE0BC => [(0.0, 0.0), (w, 0.0), (0.0, h)],
        0xE0BE => [(0.0, 0.0), (w, 0.0), (w, h)],
        _ => return None,
    };
    Some(fill_triangle(cell_w, cell_h, verts))
}

fn fill_triangle(cell_w: usize, cell_h: usize, verts: [(f32, f32); 3]) -> Vec<u8> {
    let [(ax, ay), (bx, by), (cx, cy)] = verts;
    let mut cov = vec![0u8; cell_w * cell_h];
    let edge = |x0: f32, y0: f32, x1: f32, y1: f32, px: f32, py: f32| {
        (px - x0) * (y1 - y0) - (py - y0) * (x1 - x0)
    };
    for y in 0..cell_h {
        for x in 0..cell_w {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let e0 = edge(ax, ay, bx, by, px, py);
            let e1 = edge(bx, by, cx, cy, px, py);
            let e2 = edge(cx, cy, ax, ay, px, py);
            if (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) || (e0 <= 0.0 && e1 <= 0.0 && e2 <= 0.0) {
                cov[y * cell_w + x] = 255;
            }
        }
    }
    cov
}

/// Symbols for Legacy Computing sextants U+1FB00..=U+1FB3B (Ghostty
/// `draw1FB00_1FB3B`). Chafa and image-in-terminal fallbacks use these.
fn sextant_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x1FB00..=0x1FB3B).contains(&cp) || cell_w == 0 || cell_h == 0 {
        return None;
    }
    let idx = cp - 0x1FB00;
    let bits = idx + idx / 0x14 + 1;
    let hw = ((cell_w as f32) / 2.0).round() as usize;
    let h1 = ((cell_h as f32) / 3.0).round() as usize;
    let h2 = ((cell_h as f32) * 2.0 / 3.0).round() as usize;
    let mut cov = vec![0u8; cell_w * cell_h];
    let mut fill = |x0: usize, x1: usize, y0: usize, y1: usize| {
        for y in y0..y1.min(cell_h) {
            for x in x0..x1.min(cell_w) {
                cov[y * cell_w + x] = 255;
            }
        }
    };
    if bits & 1 != 0 {
        fill(0, hw, 0, h1);
    }
    if bits & 2 != 0 {
        fill(hw, cell_w, 0, h1);
    }
    if bits & 4 != 0 {
        fill(0, hw, h1, h2);
    }
    if bits & 8 != 0 {
        fill(hw, cell_w, h1, h2);
    }
    if bits & 16 != 0 {
        fill(0, hw, h2, cell_h);
    }
    if bits & 32 != 0 {
        fill(hw, cell_w, h2, cell_h);
    }
    Some(cov)
}

/// Geometric Shapes filled corner triangles U+25E2..=U+25E5.
fn corner_triangle_coverage(ch: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    let w = cell_w as f32;
    let h = cell_h as f32;
    let verts = match cp {
        0x25E2 => [(0.0, h), (w, h), (w, 0.0)],
        0x25E3 => [(0.0, 0.0), (0.0, h), (w, h)],
        0x25E4 => [(0.0, 0.0), (0.0, h), (w, 0.0)],
        0x25E5 => [(0.0, 0.0), (w, 0.0), (w, h)],
        _ => return None,
    };
    Some(fill_triangle(cell_w, cell_h, verts))
}

#[allow(clippy::too_many_arguments)]
fn blit_coverage(
    buffer: &mut [u32],
    stride: usize,
    bitmap: &[u8],
    w: usize,
    h: usize,
    ox: i32,
    oy: i32,
    fg: [u8; 3],
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    for gy in 0..h {
        for gx in 0..w {
            let cover = bitmap[gy * w + gx];
            if cover == 0 {
                continue;
            }
            let px = ox + gx as i32;
            let py = oy + gy as i32;
            if px < clip_x0 || py < clip_y0 || px >= clip_x1 || py >= clip_y1 {
                continue;
            }
            if px < 0 || py < 0 {
                continue;
            }
            let (px, py) = (px as usize, py as usize);
            if px >= stride || py * stride + px >= buffer.len() {
                continue;
            }
            let idx = py * stride + px;
            let dest = unpack_rgb(buffer[idx]);
            let a = cover as f32 / 255.0;
            let blended = [
                (fg[0] as f32 * a + dest[0] as f32 * (1.0 - a)) as u8,
                (fg[1] as f32 * a + dest[1] as f32 * (1.0 - a)) as u8,
                (fg[2] as f32 * a + dest[2] as f32 * (1.0 - a)) as u8,
            ];
            // Ink is opaque: raise the pixel's alpha by the same coverage so
            // glyphs stay readable over a translucent ground (PT-87).
            buffer[idx] = pack_argb(raise_alpha(alpha_of(buffer[idx]), cover), blended);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_rgba(
    buffer: &mut [u32],
    stride: usize,
    rgba: &[u8],
    w: usize,
    h: usize,
    ox: i32,
    oy: i32,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    for gy in 0..h {
        for gx in 0..w {
            let si = (gy * w + gx) * 4;
            let a = rgba[si + 3];
            if a == 0 {
                continue;
            }
            let px = ox + gx as i32;
            let py = oy + gy as i32;
            if px < clip_x0 || py < clip_y0 || px >= clip_x1 || py >= clip_y1 {
                continue;
            }
            if px < 0 || py < 0 {
                continue;
            }
            let (px, py) = (px as usize, py as usize);
            if px >= stride || py * stride + px >= buffer.len() {
                continue;
            }
            let idx = py * stride + px;
            let dest = unpack_rgb(buffer[idx]);
            let af = a as f32 / 255.0;
            let blended = [
                (rgba[si] as f32 * af + dest[0] as f32 * (1.0 - af)) as u8,
                (rgba[si + 1] as f32 * af + dest[1] as f32 * (1.0 - af)) as u8,
                (rgba[si + 2] as f32 * af + dest[2] as f32 * (1.0 - af)) as u8,
            ];
            buffer[idx] = pack_argb(raise_alpha(alpha_of(buffer[idx]), a), blended);
        }
    }
}

/// Alpha-blit `rgba` (src_w×src_h) scaled to a dst_w×dst_h rect at (dst_x,dst_y),
/// nearest-neighbor, clipped to [clip_x0,clip_x1)×[clip_y0,clip_y1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_rgba_scaled(
    buffer: &mut [u32],
    stride: usize,
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_x: i32,
    dst_y: i32,
    dst_w: usize,
    dst_h: usize,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    blit_rgba_scaled_window(
        buffer, stride, rgba, src_w, 0, 0, src_w, src_h, dst_x, dst_y, dst_w, dst_h, clip_x0,
        clip_y0, clip_x1, clip_y1,
    );
}

/// Like [`blit_rgba_scaled`] but samples a window (`src_x`,`src_y`,`src_w`,`src_h`)
/// inside a packed buffer whose row stride is `src_stride` pixels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_rgba_scaled_window(
    buffer: &mut [u32],
    stride: usize,
    rgba: &[u8],
    src_stride: usize,
    src_x: usize,
    src_y: usize,
    src_w: usize,
    src_h: usize,
    dst_x: i32,
    dst_y: i32,
    dst_w: usize,
    dst_h: usize,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 || src_stride == 0 {
        return;
    }
    for dy in 0..dst_h {
        let sy = dy * src_h / dst_h;
        let py = dst_y + dy as i32;
        if py < clip_y0 || py >= clip_y1 || py < 0 {
            continue;
        }
        for dx in 0..dst_w {
            let sx = dx * src_w / dst_w;
            let px = dst_x + dx as i32;
            if px < clip_x0 || px >= clip_x1 || px < 0 {
                continue;
            }
            let si = ((src_y + sy) * src_stride + (src_x + sx)) * 4;
            if si + 3 >= rgba.len() {
                continue;
            }
            let a = rgba[si + 3];
            if a == 0 {
                continue;
            }
            let (px, py) = (px as usize, py as usize);
            if px >= stride {
                continue;
            }
            let idx = py * stride + px;
            if idx >= buffer.len() {
                continue;
            }
            let dest = unpack_rgb(buffer[idx]);
            let af = a as f32 / 255.0;
            buffer[idx] = pack_argb(
                raise_alpha(alpha_of(buffer[idx]), a),
                [
                    (rgba[si] as f32 * af + dest[0] as f32 * (1.0 - af)) as u8,
                    (rgba[si + 1] as f32 * af + dest[1] as f32 * (1.0 - af)) as u8,
                    (rgba[si + 2] as f32 * af + dest[2] as f32 * (1.0 - af)) as u8,
                ],
            );
        }
    }
}

/// Blit Kitty unicode-placeholder tiles over cells that already skipped the
/// `U+10EEEE` glyph. Walks visible cells only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_kitty_placeholders(
    emulator: &Emulator,
    font: &FontMetrics,
    scroll_offset: usize,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
) {
    use prismattyc_emulator::kitty_placeholder::{
        color_to_id, decode_placeholder_pos, image_id, placeholder_tile, PlaceholderPos,
    };

    let screen = emulator.screen();
    let cols = screen.columns();
    let rows = screen.rows();
    let cw = font.cell_w;
    let ch = font.cell_h;
    let scroll = scroll_offset.min(screen.max_view_scroll());
    let clip_x0 = origin_x as i32;
    let clip_y0 = origin_y as i32;
    let clip_x1 = (origin_x + clip_w) as i32;
    let clip_y1 = (origin_y + clip_h) as i32;

    struct Cont {
        id_low: u32,
        placement_id: u32,
        pos: PlaceholderPos,
    }

    for row in 0..rows {
        let mut prev: Option<Cont> = None;
        for col in 0..cols {
            let cell = screen.view_cell(scroll, row, col);
            if cell.character != KITTY_PLACEHOLDER {
                prev = None;
                continue;
            }
            let Some(id_low) = color_to_id(cell.style.foreground) else {
                prev = None;
                continue;
            };
            let placement_id = color_to_id(cell.style.underline_color).unwrap_or(0);
            let pos = decode_placeholder_pos(
                cell.combining_marks(),
                prev.filter(|p| p.id_low == id_low && p.placement_id == placement_id)
                    .map(|p| p.pos),
            );
            prev = Some(Cont {
                id_low,
                placement_id,
                pos,
            });
            let id = image_id(id_low, Some(u16::from(pos.msb)));
            let Some(img) = emulator.image_by_id(id) else {
                continue;
            };
            let Some(v) = emulator.virtual_placement(id, placement_id) else {
                continue;
            };
            let place_cols = if v.cols == 0 {
                img.width.div_ceil(cw as u32).max(1) as u16
            } else {
                v.cols
            };
            let place_rows = if v.rows == 0 {
                img.height.div_ceil(ch as u32).max(1) as u16
            } else {
                v.rows
            };
            let Some(tile) = placeholder_tile(
                img.width,
                img.height,
                u32::from(place_cols),
                u32::from(place_rows),
                cw as u32,
                ch as u32,
                pos.row,
                pos.col,
            ) else {
                continue;
            };
            let x0 = origin_x + col * cw;
            let y0 = origin_y + row * ch;
            blit_rgba_scaled_window(
                buffer,
                stride_px,
                &img.rgba,
                img.width as usize,
                tile.src_x as usize,
                tile.src_y as usize,
                tile.src_w as usize,
                tile.src_h as usize,
                (x0 as i32) + tile.dst_ox as i32,
                (y0 as i32) + tile.dst_oy as i32,
                tile.dst_w as usize,
                tile.dst_h as usize,
                clip_x0,
                clip_y0,
                clip_x1,
                clip_y1,
            );
        }
    }
}

/// True when any direct Kitty placement covers this view cell.
///
/// Same geometry as [`blit_direct_kitty_images`]: the placement's top view
/// row is `anchor_abs_line - scrolled_lines + scroll_offset`.
fn direct_image_covers_view_cell(
    images: &[PlacedImage],
    scrolled_lines: u64,
    scroll_offset: usize,
    row: usize,
    col: usize,
) -> bool {
    images.iter().any(|img| {
        let rel_line = img.anchor_abs_line as i64 - scrolled_lines as i64 + scroll_offset as i64;
        if rel_line < 0 {
            return false;
        }
        let top = rel_line as usize;
        let rows = img.rows.max(1) as usize;
        let cols = img.cols.max(1) as usize;
        let left = img.anchor_col as usize;
        row >= top
            && row < top.saturating_add(rows)
            && col >= left
            && col < left.saturating_add(cols)
    })
}

/// Paint the cell background under a direct image. Skip inverse, selection,
/// glyph ink, and the caret so leftover cell paint cannot show through
/// transparent source pixels. Keep default_bg (at surface alpha) or the
/// explicit ANSI background so pane_backdrop does not leak through.
#[allow(clippy::too_many_arguments)]
fn paint_image_covered_cell_background(
    theme: &Theme,
    style: Style,
    ch_draw: char,
    buffer: &mut [u32],
    stride_px: usize,
    x0: usize,
    y0: usize,
    cell_w: usize,
    cell_h: usize,
    paint: ScreenPaint,
) {
    let mut plain = style;
    plain.inverse = false;
    let (_, bg) = resolve_style_colors_with_theme(theme, &plain);
    let cell_bg = if ch_draw == MAIL_LETTER_GLYPH {
        theme.default_bg
    } else {
        bg
    };
    let default_bg_cell = style.background == Color::Default;
    let skip_bg = paint.skip_default_bg && default_bg_cell && ch_draw != MAIL_LETTER_GLYPH;
    paint_cell_background(
        buffer,
        stride_px,
        x0,
        y0,
        cell_w,
        cell_h,
        cell_bg,
        if default_bg_cell {
            paint.default_bg_alpha
        } else {
            OPAQUE_ALPHA
        },
        skip_bg,
        false,
        paint.bg_weight,
    );
}

/// Blit cursor-placed Kitty images (`a=T`) into the pane. Same clip box as
/// the cell grid. Insertion order is the paint order. Do not fill the
/// destination rect: the buffer already holds the pane backdrop, and a later
/// transparent placement must not erase an earlier overlapping image.
#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_direct_kitty_images(
    images: &[PlacedImage],
    scrolled_lines: u64,
    scroll_offset: usize,
    font: &FontMetrics,
    buffer: &mut [u32],
    stride_px: usize,
    origin_x: usize,
    origin_y: usize,
    clip_w: usize,
    clip_h: usize,
) {
    let cw = font.cell_w;
    let ch = font.cell_h;
    let clip_x0 = origin_x as i32;
    let clip_y0 = origin_y as i32;
    let clip_x1 = (origin_x + clip_w) as i32;
    let clip_y1 = (origin_y + clip_h) as i32;
    for img in images {
        let rel_line = img.anchor_abs_line as i64 - scrolled_lines as i64 + scroll_offset as i64;
        if rel_line < 0 {
            continue;
        }
        let dst_x = origin_x as i32 + img.anchor_col as i32 * cw as i32;
        let dst_y = origin_y as i32 + rel_line as i32 * ch as i32;
        let dst_w = img.cols.max(1) as usize * cw;
        let dst_h = img.rows.max(1) as usize * ch;
        blit_rgba_scaled(
            buffer,
            stride_px,
            &img.rgba,
            img.width as usize,
            img.height as usize,
            dst_x,
            dst_y,
            dst_w,
            dst_h,
            clip_x0,
            clip_y0,
            clip_x1,
            clip_y1,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cell_background(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    alpha: u8,
    skip_default: bool,
    selected: bool,
    bg_weight: u16,
) {
    if selected || bg_weight >= 256 {
        if skip_default && !selected {
            return;
        }
        fill_rect_argb(buffer, stride, x0, y0, w, h, rgb, alpha);
        return;
    }
    if bg_weight == 0 {
        return;
    }
    fill_rect_blend(buffer, stride, x0, y0, w, h, rgb, alpha, bg_weight);
}

fn paint_panel_shadow(
    buffer: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: [u8; 3],
) {
    const SHADOW_PX: usize = 3;
    fill_rect(
        buffer,
        stride,
        x.saturating_add(width),
        y.saturating_add(SHADOW_PX),
        SHADOW_PX,
        height,
        color,
    );
    fill_rect(
        buffer,
        stride,
        x.saturating_add(SHADOW_PX),
        y.saturating_add(height),
        width,
        SHADOW_PX,
        color,
    );
}

fn paint_panel_border(
    buffer: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: [u8; 3],
) {
    if width == 0 || height == 0 {
        return;
    }
    fill_rect(buffer, stride, x, y, width, 1, color);
    fill_rect(
        buffer,
        stride,
        x,
        y.saturating_add(height).saturating_sub(1),
        width,
        1,
        color,
    );
    let side_height = height.saturating_sub(2);
    fill_rect(
        buffer,
        stride,
        x,
        y.saturating_add(1),
        1,
        side_height,
        color,
    );
    fill_rect(
        buffer,
        stride,
        x.saturating_add(width).saturating_sub(1),
        y.saturating_add(1),
        1,
        side_height,
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_overlay_surface(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    surface: OverlaySurface,
) {
    if w == 0 || h == 0 {
        return;
    }
    if surface.is_opaque() {
        fill_rect(buffer, stride, x0, y0, w, h, rgb);
        return;
    }
    let fg_weight = opacity_to_weight(surface.opacity);
    let backdrop = if surface.blur_radius == 0 {
        None
    } else {
        let mut rgba = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
        for y in 0..h {
            for x in 0..w {
                let src = (y0.saturating_add(y))
                    .saturating_mul(stride)
                    .saturating_add(x0.saturating_add(x));
                let dst = (y * w + x) * 4;
                if src < buffer.len() {
                    let px = buffer[src];
                    let rgb = unpack_rgb(px);
                    rgba[dst..dst + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], alpha_of(px)]);
                }
            }
        }
        box_blur_rgba(&mut rgba, w, h, surface.blur_radius);
        Some(rgba)
    };
    for y in 0..h {
        for x in 0..w {
            let idx = (y0.saturating_add(y))
                .saturating_mul(stride)
                .saturating_add(x0.saturating_add(x));
            if idx >= buffer.len() {
                continue;
            }
            let dest = buffer[idx];
            let dest_rgb = unpack_rgb(dest);
            let backdrop_rgb = backdrop
                .as_ref()
                .map(|rgba| {
                    let offset = (y * w + x) * 4;
                    [rgba[offset], rgba[offset + 1], rgba[offset + 2]]
                })
                .unwrap_or(dest_rgb);
            let backdrop_px = pack_argb(alpha_of(dest), backdrop_rgb);
            buffer[idx] = blend_pixel(backdrop_px, rgb, OPAQUE_ALPHA, fg_weight);
        }
    }
}

fn blend_pixel(dest: u32, rgb: [u8; 3], alpha: u8, fg_weight: u16) -> u32 {
    let bg_weight = 256u16.saturating_sub(fg_weight);
    let blended_alpha =
        ((u16::from(alpha_of(dest)) * bg_weight + u16::from(alpha) * fg_weight) / 256) as u8;
    pack_argb(blended_alpha, mix_rgb(unpack_rgb(dest), rgb, fg_weight))
}

#[allow(clippy::too_many_arguments)]
fn fill_rect_blend(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    alpha: u8,
    fg_weight: u16,
) {
    for y in y0..y0.saturating_add(h) {
        if y.checked_mul(stride).is_none() {
            break;
        }
        for x in x0..x0.saturating_add(w) {
            let idx = y * stride + x;
            if idx < buffer.len() {
                buffer[idx] = blend_pixel(buffer[idx], rgb, alpha, fg_weight);
            }
        }
    }
}

fn fill_rect(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgb: [u8; 3],
) {
    fill_rect_argb(buffer, stride, x0, y0, w, h, rgb, OPAQUE_ALPHA);
}

/// [`fill_rect`] with an explicit straight alpha.
#[allow(clippy::too_many_arguments)]
pub fn fill_rect_argb(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    alpha: u8,
) {
    let color = pack_argb(alpha, rgb);
    for y in y0..y0.saturating_add(h) {
        if y.checked_mul(stride).is_none() {
            break;
        }
        for x in x0..x0.saturating_add(w) {
            let idx = y * stride + x;
            if idx < buffer.len() {
                buffer[idx] = color;
            }
        }
    }
}

/// Replace alpha inside a rectangle without changing its straight RGB.
///
/// Used to apply pane-local opacity to an already-rasterized background image.
#[allow(clippy::too_many_arguments)]
pub fn set_rect_alpha(
    buffer: &mut [u32],
    stride: usize,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    alpha: u8,
) {
    let alpha_bits = u32::from(alpha) << 24;
    for y in y0..y0.saturating_add(h) {
        if y.checked_mul(stride).is_none() {
            break;
        }
        for x in x0..x0.saturating_add(w) {
            let idx = y * stride + x;
            if idx < buffer.len() {
                buffer[idx] = (buffer[idx] & 0x00ff_ffff) | alpha_bits;
            }
        }
    }
}

/// Every frame pixel is `0xAARRGGBB` with straight (not premultiplied) RGB.
/// Alpha is `0xff` unless a translucent ground, chrome bar, or default-bg cell
/// wrote a lower value (PT-87). [`premultiply_in_place`] converts the finished
/// frame for a present path that carries alpha.
pub const OPAQUE_ALPHA: u8 = 0xff;

fn pack_rgb(rgb: [u8; 3]) -> u32 {
    pack_argb(OPAQUE_ALPHA, rgb)
}

pub fn pack_argb(alpha: u8, rgb: [u8; 3]) -> u32 {
    (u32::from(alpha) << 24)
        | (u32::from(rgb[0]) << 16)
        | (u32::from(rgb[1]) << 8)
        | u32::from(rgb[2])
}

fn unpack_rgb(px: u32) -> [u8; 3] {
    [
        ((px >> 16) & 0xff) as u8,
        ((px >> 8) & 0xff) as u8,
        (px & 0xff) as u8,
    ]
}

pub fn alpha_of(px: u32) -> u8 {
    ((px >> 24) & 0xff) as u8
}

/// Raise `dest` alpha toward opaque by glyph/image coverage `cover`.
fn raise_alpha(dest: u8, cover: u8) -> u8 {
    let dest = u32::from(dest);
    (dest + (255 - dest) * u32::from(cover) / 255) as u8
}

/// Map 0.0-1.0 opacity onto a straight alpha byte.
pub fn opacity_to_alpha(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Premultiply every pixel's RGB by its alpha, in place.
///
/// X11 (and Wayland where a backend carries alpha) expects premultiplied
/// ARGB. Skip the pass entirely when the window is opaque.
pub fn premultiply_in_place(buffer: &mut [u32]) {
    for px in buffer.iter_mut() {
        let alpha = u32::from(alpha_of(*px));
        if alpha == 255 {
            continue;
        }
        let r = ((*px >> 16) & 0xff) * alpha / 255;
        let g = ((*px >> 8) & 0xff) * alpha / 255;
        let b = (*px & 0xff) * alpha / 255;
        *px = (alpha << 24) | (r << 16) | (g << 8) | b;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::Screen;
    use std::sync::Arc;

    fn test_run_cell(style: Style) -> RunCell {
        RunCell {
            style,
            selected: false,
            cursor: false,
            wide_cont: false,
            special: false,
            multi_scalar: false,
            sprite: false,
            primary_covered: true,
        }
    }

    fn lit_component_count(cov: &[u8], w: usize, h: usize) -> usize {
        let mut seen = vec![false; cov.len()];
        let mut components = 0;
        for y in 0..h {
            for x in 0..w {
                let start = y * w + x;
                if cov[start] == 0 || seen[start] {
                    continue;
                }
                components += 1;
                let mut stack = vec![(x, y)];
                seen[start] = true;
                while let Some((x, y)) = stack.pop() {
                    for (nx, ny) in [
                        (x.wrapping_sub(1), y),
                        (x + 1, y),
                        (x, y.wrapping_sub(1)),
                        (x, y + 1),
                    ] {
                        if nx >= w || ny >= h {
                            continue;
                        }
                        let index = ny * w + nx;
                        if cov[index] != 0 && !seen[index] {
                            seen[index] = true;
                            stack.push((nx, ny));
                        }
                    }
                }
            }
        }
        components
    }

    fn assert_binary_and_connected(cov: &[u8], w: usize, h: usize, ch: char) {
        assert!(
            cov.iter().all(|&alpha| alpha == 0 || alpha == 255),
            "{ch} coverage must be binary"
        );
        assert_eq!(
            lit_component_count(cov, w, h),
            1,
            "{ch} coverage must be one 4-neighbor component"
        );
    }

    #[test]
    fn filtered_raster_paints_only_selected_rows() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut screen = Screen::new(4, 2, 0);
        screen.put_char('A');
        screen.line_feed();
        screen.carriage_return();
        screen.put_char('B');
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let mut full = vec![0u32; width * height];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut full,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
        );
        let sentinel = 0x1234_5678;
        let mut filtered = vec![sentinel; width * height];
        let rows = [1];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut filtered,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
            Some(&rows),
        );
        assert!(filtered[..font.cell_h * width]
            .iter()
            .all(|pixel| *pixel == sentinel));
        assert_eq!(
            &filtered[font.cell_h * width..],
            &full[font.cell_h * width..]
        );
    }

    #[test]
    fn filtered_raster_empty_rows_preserve_the_frame_and_cursor() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let screen = Screen::new(4, 2, 0);
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let sentinel = 0x7654_3210;
        let mut buffer = vec![sentinel; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
            Some(&[]),
        );
        assert!(buffer.iter().all(|pixel| *pixel == sentinel));
    }

    #[test]
    fn filtered_ligature_raster_paints_only_selected_rows() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut screen = Screen::new(4, 2, 0);
        screen.put_char('=');
        screen.put_char('>');
        screen.line_feed();
        screen.carriage_return();
        screen.put_char('B');
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let sentinel = 0x2468_ace0;
        let mut buffer = vec![sentinel; width * height];
        let rows = [1];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            true,
            &[],
            ScreenPaint::default(),
            &[],
            0,
            Some(&rows),
        );
        assert!(buffer[..font.cell_h * width]
            .iter()
            .all(|pixel| *pixel == sentinel));
        assert!(buffer[font.cell_h * width..]
            .iter()
            .any(|pixel| *pixel != sentinel));
    }

    #[test]
    fn filtered_raster_paints_selection_only_in_the_selected_cell() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let theme = default_theme();
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char(' ');
        screen.put_char(' ');
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let mut buffer = vec![0x1234_5678; width * height];
        let selection = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 0,
        };
        rasterize_screen_at_with_theme_options_filtered(
            theme,
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            Some(selection),
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
            Some(&[0]),
        );
        assert_ne!(buffer[0], buffer[font.cell_w]);
    }

    #[test]
    fn filtered_raster_paints_cursor_when_its_row_is_selected() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let screen = {
            let mut screen = Screen::new(2, 2, 0);
            screen.cursor_down(1);
            screen
        };
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let paint = ScreenPaint {
            skip_default_bg: true,
            ..ScreenPaint::default()
        };
        let sentinel = 0x2468_ace0;
        let mut outside = vec![sentinel; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut outside,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            paint,
            &[],
            0,
            Some(&[0]),
        );
        assert!(outside.iter().all(|pixel| *pixel == sentinel));

        let mut inside = vec![sentinel; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut inside,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            paint,
            &[],
            0,
            Some(&[1]),
        );
        assert!(inside[font.cell_h * width..]
            .iter()
            .any(|pixel| *pixel != sentinel));
    }

    #[test]
    fn filtered_raster_matches_full_raster_for_wide_continuation_cells() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut screen = Screen::new(4, 1, 0);
        screen.put_char('界');
        assert!(screen.view_cell(0, 0, 1).wide_cont);
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let mut full = vec![0u32; width * height];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut full,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
        );
        let mut filtered = vec![0u32; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut filtered,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            ScreenPaint::default(),
            &[],
            0,
            Some(&[0]),
        );
        assert_eq!(filtered, full);
    }

    #[test]
    fn filtered_raster_suppresses_glyphs_under_direct_images() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char('A');
        let width = screen.columns() * font.cell_w;
        let height = screen.rows() * font.cell_h;
        let image = PlacedImage {
            id: 1,
            image_number: None,
            placement_id: 1,
            z: 0,
            rgba: Arc::<[u8]>::from(vec![255, 0, 0, 255]),
            width: 1,
            height: 1,
            cols: 1,
            rows: 1,
            anchor_abs_line: 0,
            anchor_col: 0,
        };
        let paint = ScreenPaint {
            skip_default_bg: true,
            ..ScreenPaint::default()
        };
        let sentinel = 0x7654_3210;
        let mut covered = vec![sentinel; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut covered,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            paint,
            &[image],
            0,
            Some(&[0]),
        );
        assert!(covered.iter().all(|pixel| *pixel == sentinel));

        let mut uncovered = vec![sentinel; width * height];
        rasterize_screen_at_with_theme_options_filtered(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut uncovered,
            width,
            0,
            0,
            width,
            height,
            false,
            &[],
            paint,
            &[],
            0,
            Some(&[0]),
        );
        assert!(uncovered.iter().any(|pixel| *pixel != sentinel));
    }

    #[test]
    fn split_runs_respects_terminal_boundaries() {
        let style = Style::default();
        let mut plain = vec![test_run_cell(style); 6]; // "a => b"
        assert_eq!(split_runs(&plain), vec![Run { start: 0, len: 6 }]);

        plain[2].style.bold = true;
        assert_eq!(
            split_runs(&plain),
            vec![
                Run { start: 0, len: 2 },
                Run { start: 2, len: 1 },
                Run { start: 3, len: 3 },
            ]
        );
        for boundary in 0..7 {
            let mut cells = vec![test_run_cell(style); 3];
            match boundary {
                0 => cells[1].selected = true,
                1 => cells[1].cursor = true,
                2 => cells[1].wide_cont = true,
                3 => cells[1].special = true,
                4 => cells[1].multi_scalar = true,
                5 => cells[1].sprite = true,
                _ => cells[1].primary_covered = false,
            }
            assert_eq!(
                split_runs(&cells),
                vec![Run { start: 0, len: 1 }, Run { start: 2, len: 1 }]
            );
        }
    }

    #[test]
    fn rustybuzz_ligature_features_shape_expected_glyph_counts() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let Some(face) = font.primary_face() else {
            return;
        };
        let shape_count = |tags: &[&str]| {
            let features: Vec<rustybuzz::Feature> = tags
                .iter()
                .map(|tag| tag.parse().expect("valid test feature"))
                .collect();
            let mut buffer = rustybuzz::UnicodeBuffer::new();
            buffer.push_str("=>");
            buffer.guess_segment_properties();
            rustybuzz::shape(&face, &features, buffer)
                .glyph_infos()
                .len()
        };
        let liga_count = shape_count(&["liga"]);
        let disabled_count = shape_count(&["-liga", "-calt"]);
        let calt_count = shape_count(&["calt"]);
        eprintln!(
            "test shaping counts: liga={liga_count}, calt={calt_count}, disabled={disabled_count}"
        );
        assert!(
            liga_count < 2 || liga_count == disabled_count,
            "liga must not increase glyph count, got {liga_count}"
        );
        assert_eq!(disabled_count, 2);
        if liga_count == disabled_count {
            eprintln!("bundled BAKED_MONO has no => liga lookup; feature pass remains hermetic");
        }
    }

    #[test]
    fn ligature_raster_preserves_background_and_default_off_pixels() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut screen = Screen::new(4, 1, 0);
        screen.set_style(Style {
            foreground: Color::Rgb {
                r: 250,
                g: 250,
                b: 250,
            },
            background: Color::Rgb { r: 3, g: 4, b: 5 },
            ..Style::default()
        });
        for ch in " => ".chars() {
            screen.put_char(ch);
        }
        let width = 4 * font.cell_w;
        let height = font.cell_h;
        let mut legacy = vec![0u32; width * height];
        let mut disabled = vec![0u32; width * height];
        let mut enabled = vec![0u32; width * height];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut legacy,
            width,
            0,
            0,
            width,
            height,
            ScreenPaint::default(),
        );
        rasterize_screen_at_with_theme_options(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut disabled,
            width,
            0,
            0,
            width,
            height,
            false,
            &["calt".to_string(), "liga".to_string()],
            ScreenPaint::default(),
            &[],
            0,
        );
        rasterize_screen_at_with_theme_options(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut enabled,
            width,
            0,
            0,
            width,
            height,
            true,
            &["calt".to_string(), "liga".to_string()],
            ScreenPaint::default(),
            &[],
            0,
        );
        assert_eq!(legacy, disabled, "disabled shaping must keep legacy pixels");
        assert!(
            enabled.iter().zip(disabled.iter()).any(|(a, b)| a != b),
            "enabled ligature shaping must change the => raster"
        );
        let bg = pack_rgb([3, 4, 5]);
        for cell in 0..4 {
            let x = cell * font.cell_w + font.cell_w.saturating_sub(1);
            assert_eq!(enabled[(height - 1) * width + x], bg);
        }
    }

    #[test]
    fn ligature_run_breaks_at_cursor_and_paints_caret() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let mut emulator = Emulator::new(4, 1, 0);
        let _ = emulator.feed(b" => ");
        let _ = emulator.feed(b"\x1b[1;3H");
        let screen = emulator.screen();
        let width = 4 * font.cell_w;
        let height = font.cell_h;
        let mut buffer = vec![0u32; width * height];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            screen,
            &font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            true,
            &["calt".to_string(), "liga".to_string()],
            ScreenPaint::default(),
            &[],
            0,
        );
        let caret = pack_rgb(
            default_theme()
                .cursor_bg
                .unwrap_or(default_theme().default_fg),
        );
        let cursor_ink = (0..height)
            .flat_map(|row| {
                buffer[row * width + font.cell_w * 2..row * width + font.cell_w * 3].iter()
            })
            .filter(|&&pixel| pixel == caret)
            .count();
        assert_eq!(screen.cursor().column, 2, "CSI should place cursor on >");
        assert!(
            cursor_ink > font.cell_w * height / 3,
            "cursor block must paint over > (ink pixels={cursor_ink}, caret={caret:#x})"
        );
    }

    #[test]
    fn scaled_blit_upscales_and_clips() {
        // 1x1 opaque red, scaled into a 2x2 dst on a 4x4 white buffer, clipped to x<3.
        let stride = 4usize;
        let mut buf = vec![pack_rgb([255, 255, 255]); 16];
        let rgba = [255u8, 0, 0, 255];
        blit_rgba_scaled(&mut buf, stride, &rgba, 1, 1, 1, 1, 2, 2, 0, 0, 3, 4);
        // (1,1),(2,1),(1,2),(2,2) become red; (3,*) clipped out stays white.
        assert_eq!(unpack_rgb(buf[stride + 1]), [255, 0, 0]); // row 1, col 1
        assert_eq!(unpack_rgb(buf[2 * stride + 2]), [255, 0, 0]); // row 2, col 2
        assert_eq!(unpack_rgb(buf[stride + 3]), [255, 255, 255]); // row 1, col 3 (clipped)
    }

    fn rgba_px(r: u8, g: u8, b: u8) -> [u8; 4] {
        [r, g, b, 255]
    }

    #[test]
    fn cover_scale_rgba_crops_center_of_wide_source() {
        // 4×2 unique pixels → 2×2 takes the center two columns.
        let mut src = Vec::new();
        for px in [
            rgba_px(255, 0, 0),
            rgba_px(0, 255, 0),
            rgba_px(0, 0, 255),
            rgba_px(255, 255, 255),
            rgba_px(0, 255, 255),
            rgba_px(255, 0, 255),
            rgba_px(255, 255, 0),
            rgba_px(0, 0, 0),
        ] {
            src.extend_from_slice(&px);
        }
        let out = cover_scale_rgba(4, 2, &src, 2, 2);
        assert_eq!(&out[0..4], &rgba_px(0, 255, 0));
        assert_eq!(&out[4..8], &rgba_px(0, 0, 255));
        assert_eq!(&out[8..12], &rgba_px(255, 0, 255));
        assert_eq!(&out[12..16], &rgba_px(255, 255, 0));
    }

    #[test]
    fn cover_scale_rgba_replicates_one_pixel() {
        let src = rgba_px(10, 20, 30);
        let out = cover_scale_rgba(1, 1, &src, 3, 3);
        assert_eq!(out.len(), 3 * 3 * 4);
        for px in out.chunks_exact(4) {
            assert_eq!(px, &src);
        }
    }

    #[test]
    fn box_blur_rgba_radius_zero_is_identity() {
        let mut rgba = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let before = rgba.clone();
        box_blur_rgba(&mut rgba, 2, 1, 0);
        assert_eq!(rgba, before);
    }

    #[test]
    fn box_blur_rgba_spreads_white_pixel() {
        let width = 3;
        let mut rgba = vec![0u8; width * width * 4];
        let mid = (width + 1) * 4; // row 1, col 1
        rgba[mid..mid + 3].copy_from_slice(&[255, 255, 255]);
        rgba[mid + 3] = 255;
        box_blur_rgba(&mut rgba, width, width, 1);
        let neighbor = (width + 2) * 4; // row 1, col 2
        assert!(
            rgba[neighbor] > 0,
            "radius 1 must spread into a neighbor: {:?}",
            &rgba[neighbor..neighbor + 4]
        );
    }

    fn naive_box_blur_rgba(rgba: &mut [u8], w: usize, h: usize, radius: u32) {
        if radius == 0 || w == 0 || h == 0 {
            return;
        }
        let radius = radius as usize;
        let kernel = radius * 2 + 1;
        let mut tmp = vec![0u8; w * h * 4];
        let pass = |src: &[u8], dst: &mut [u8], horizontal: bool| {
            for y in 0..h {
                for x in 0..w {
                    let mut acc = [0u32; 4];
                    for t in 0..kernel {
                        let (sx, sy) = if horizontal {
                            (x.saturating_add(t).saturating_sub(radius).min(w - 1), y)
                        } else {
                            (x, y.saturating_add(t).saturating_sub(radius).min(h - 1))
                        };
                        let si = (sy * w + sx) * 4;
                        for c in 0..4 {
                            acc[c] += u32::from(src[si + c]);
                        }
                    }
                    let di = (y * w + x) * 4;
                    for c in 0..4 {
                        dst[di + c] = (acc[c] / kernel as u32) as u8;
                    }
                }
            }
        };
        for _ in 0..3 {
            pass(rgba, &mut tmp, true);
            pass(&tmp, rgba, false);
        }
    }

    #[test]
    fn box_blur_rgba_matches_naive_on_small_grids() {
        let cases: &[(usize, usize, u32)] = &[(3, 3, 1), (5, 4, 2), (2, 8, 7), (8, 1, 3)];
        for &(w, h, radius) in cases {
            let mut src = vec![0u8; w * h * 4];
            for (i, px) in src.chunks_exact_mut(4).enumerate() {
                px[0] = (i * 17 % 256) as u8;
                px[1] = (i * 41 % 256) as u8;
                px[2] = (i * 73 % 256) as u8;
                px[3] = 255;
            }
            let mut sliding = src.clone();
            let mut naive = src;
            box_blur_rgba(&mut sliding, w, h, radius);
            naive_box_blur_rgba(&mut naive, w, h, radius);
            assert_eq!(
                sliding, naive,
                "sliding window must match clamp-to-edge naive {w}x{h} r={radius}"
            );
        }
    }

    #[test]
    fn background_tint_opacity_endpoints_and_midpoint() {
        let pixel = [200u8, 0, 0];
        let theme = [0u8, 0, 200];
        assert_eq!(background_tint(pixel, theme, 0.0), pack_rgb(theme));
        assert_eq!(background_tint(pixel, theme, 1.0), pack_rgb(pixel));
        let mid = unpack_rgb(background_tint(pixel, theme, 0.5));
        assert_eq!(mid[0], 100);
        assert_eq!(mid[2], 100);
    }

    #[test]
    fn build_background_layer_tints_inline_png() {
        const RED_1X1_PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xF7, 0x03, 0x41,
            0x43, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let theme_bg = [0u8, 0, 0];
        let layer = build_background_layer(RED_1X1_PNG, 2, 2, theme_bg, 1.0, 0).unwrap();
        assert_eq!(layer.len(), 4);
        for px in layer {
            assert_eq!(unpack_rgb(px), [255, 0, 0]);
        }
    }

    #[test]
    fn skip_default_bg_leaves_sentinel_and_paints_sgr() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char(' ');
        screen.set_style(Style {
            background: Color::Rgb { r: 0, g: 255, b: 0 },
            ..Style::default()
        });
        screen.put_char(' ');
        let w = 2 * font.cell_w;
        let h = font.cell_h;
        let sentinel = 0x00AA_BBCC;
        let mut buf = vec![sentinel; w * h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            ScreenPaint {
                skip_default_bg: true,
                ..ScreenPaint::default()
            },
        );
        assert_eq!(
            buf[0], sentinel,
            "default-bg cell must keep the frame pixel"
        );
        let sgr = pack_rgb([0, 255, 0]);
        assert_eq!(
            buf[font.cell_w], sgr,
            "SGR-bg cell must overwrite the sentinel"
        );

        let mut inverse = Screen::new(1, 1, 0);
        inverse.set_style(Style {
            inverse: true,
            ..Style::default()
        });
        inverse.put_char(' ');
        let mut buf2 = vec![sentinel; font.cell_w * font.cell_h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &inverse,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf2,
            font.cell_w,
            0,
            0,
            font.cell_w,
            font.cell_h,
            ScreenPaint {
                skip_default_bg: true,
                ..ScreenPaint::default()
            },
        );
        assert_ne!(
            buf2[0], sentinel,
            "inverse cell must overwrite the sentinel"
        );
    }

    #[test]
    fn cell_bg_at_half_opacity_blends_over_backdrop() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(1, 1, 0);
        screen.set_style(Style {
            background: Color::Rgb { r: 255, g: 0, b: 0 },
            ..Style::default()
        });
        screen.put_char(' ');
        let w = font.cell_w;
        let h = font.cell_h;
        let backdrop = pack_rgb([0, 0, 255]);
        let mut buf = vec![backdrop; w * h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            ScreenPaint {
                bg_weight: opacity_to_weight(0.5),
                ..ScreenPaint::default()
            },
        );
        let mixed = pack_rgb(mix_rgb([0, 0, 255], [255, 0, 0], 128));
        assert_eq!(buf[0], mixed, "SGR bg at 0.5 must mix with the backdrop");
        assert_eq!(opacity_to_weight(1.0), 256);
        assert_eq!(opacity_to_weight(0.0), 0);
    }

    /// PT-98: with no background image the ground behind a pane is
    /// `pane_backdrop`, so a dimmed pane visibly recedes even on cells that
    /// carry no SGR background.
    #[test]
    fn inactive_pane_default_bg_recedes_toward_pane_backdrop() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let ghost = crate::theme::load(Some("ghost"), Path::new("/tmp/prism/config.toml")).unwrap();
        let mut screen = Screen::new(1, 1, 0);
        screen.put_char(' ');
        let (w, h) = (font.cell_w, font.cell_h);
        let paint_at = |opacity: f32| {
            let mut buf = vec![pack_rgb(ghost.pane_backdrop); w * h];
            rasterize_screen_at_with_theme(
                &ghost,
                &screen,
                &font,
                false,
                CursorShape::Block,
                0,
                None,
                &mut buf,
                w,
                0,
                0,
                w,
                h,
                ScreenPaint {
                    bg_weight: opacity_to_weight(opacity),
                    ..ScreenPaint::default()
                },
            );
            unpack_rgb(buf[0])
        };
        let active = paint_at(1.0);
        let inactive = paint_at(0.5);
        assert_eq!(active, ghost.default_bg, "an active pane paints its own bg");
        for channel in 0..3 {
            assert!(
                active[channel].abs_diff(inactive[channel]) >= 4,
                "channel {channel}: active {active:?} vs inactive {inactive:?} \
                 must differ by at least 4"
            );
        }
    }

    /// PT-87: a default-background cell carries the window alpha; a cell with
    /// an explicit SGR background stays opaque, as Ghostty does.
    #[test]
    fn window_alpha_reaches_default_bg_cells_only() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char(' ');
        screen.set_style(Style {
            background: Color::Rgb { r: 255, g: 0, b: 0 },
            ..Style::default()
        });
        screen.put_char(' ');
        let stride = font.cell_w * 2;
        let mut buf = vec![pack_argb(128, [0, 0, 255]); stride * font.cell_h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            stride,
            0,
            0,
            stride,
            font.cell_h,
            ScreenPaint {
                default_bg_alpha: 204,
                ..ScreenPaint::default()
            },
        );
        assert_eq!(
            alpha_of(buf[0]),
            204,
            "default-bg cell carries window alpha"
        );
        assert_eq!(
            alpha_of(buf[font.cell_w]),
            OPAQUE_ALPHA,
            "an SGR background stays opaque"
        );
    }

    /// PT-206: the mail envelope cell is a default-bg cell too. Under
    /// `window_opacity < 1` it carries the window alpha instead of printing
    /// an opaque theme-bg square; over a background image it still paints
    /// its pad instead of being skipped.
    #[test]
    fn mail_letter_cell_carries_window_alpha_and_keeps_image_pad() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char(MAIL_LETTER_GLYPH);
        screen.put_char(' ');
        let stride = font.cell_w * 2;
        let last = font.cell_h.saturating_sub(1);
        let mut buf = vec![pack_argb(128, [0, 0, 255]); stride * font.cell_h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            stride,
            0,
            0,
            stride,
            font.cell_h,
            ScreenPaint {
                default_bg_alpha: 204,
                ..ScreenPaint::default()
            },
        );
        // Bottom-left pixel of the mail cell: the envelope pins to the top-left
        // corner, so this pixel is ground, not ink.
        assert_eq!(
            alpha_of(buf[last * stride]),
            alpha_of(buf[last * stride + font.cell_w]),
            "mail cell ground alpha must match its default-bg neighbour"
        );
        assert_eq!(alpha_of(buf[last * stride]), 204);

        let sentinel = 0x00AA_BBCC;
        let mut buf = vec![sentinel; stride * font.cell_h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            stride,
            0,
            0,
            stride,
            font.cell_h,
            ScreenPaint {
                skip_default_bg: true,
                ..ScreenPaint::default()
            },
        );
        assert_ne!(
            buf[last * stride],
            sentinel,
            "mail cell keeps its theme pad over a background image"
        );
        assert_eq!(
            buf[last * stride + font.cell_w],
            sentinel,
            "the plain default-bg neighbour is still skipped"
        );
    }

    /// PT-87: ink is opaque, so glyphs stay readable over a translucent ground.
    #[test]
    fn glyph_coverage_raises_alpha_to_opaque() {
        let (w, h) = (4usize, 1usize);
        let mut buf = vec![pack_argb(64, [0, 0, 0]); w * h];
        blit_coverage(
            &mut buf,
            w,
            &[255, 128, 0, 0],
            w,
            h,
            0,
            0,
            [255, 255, 255],
            0,
            0,
            4,
            1,
        );
        assert_eq!(alpha_of(buf[0]), OPAQUE_ALPHA, "full coverage is opaque");
        assert!(
            alpha_of(buf[1]) > 64 && alpha_of(buf[1]) < OPAQUE_ALPHA,
            "partial coverage lands between ground and opaque: {}",
            alpha_of(buf[1])
        );
        assert_eq!(
            alpha_of(buf[2]),
            64,
            "untouched pixels keep the ground alpha"
        );
    }

    #[test]
    fn premultiply_scales_rgb_by_alpha_and_skips_opaque_pixels() {
        let mut buf = vec![
            pack_argb(255, [200, 100, 50]),
            pack_argb(128, [200, 100, 50]),
            pack_argb(0, [200, 100, 50]),
        ];
        premultiply_in_place(&mut buf);
        assert_eq!(buf[0], pack_argb(255, [200, 100, 50]));
        assert_eq!(buf[1], pack_argb(128, [100, 50, 25]));
        assert_eq!(buf[2], pack_argb(0, [0, 0, 0]));
        assert_eq!(opacity_to_alpha(1.0), 255);
        assert_eq!(opacity_to_alpha(0.0), 0);
        assert_eq!(opacity_to_alpha(0.8), 204);
    }

    #[test]
    fn set_rect_alpha_preserves_background_image_rgb_and_clips() {
        let mut buffer = vec![pack_argb(255, [10, 20, 30]); 8];
        set_rect_alpha(&mut buffer, 4, 1, 0, 2, 3, 153);
        assert_eq!(buffer[0], pack_argb(255, [10, 20, 30]));
        assert_eq!(buffer[1], pack_argb(153, [10, 20, 30]));
        assert_eq!(buffer[2], pack_argb(153, [10, 20, 30]));
        assert_eq!(buffer[5], pack_argb(153, [10, 20, 30]));
        assert_eq!(buffer[6], pack_argb(153, [10, 20, 30]));
        assert_eq!(buffer[7], pack_argb(255, [10, 20, 30]));
    }

    /// PT-87: the chrome bar carries `chrome_opacity`; its text does not.
    #[test]
    fn tab_strip_bar_carries_chrome_alpha_and_text_stays_opaque() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let tabs = [crate::mux::TabInfo {
            title: "one".into(),
            selected: true,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 0,
            focused_handle: None,
            handle_titles: Vec::new(),
            handle_active: Vec::new(),
            git_label: None,
            pane_title: None,
        }];
        let width = font.cell_w * 40;
        let bar_h = font.cell_h;
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip_with_theme(
            default_theme(),
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0, 0, 255],
            None,
            0,
            0,
            0,
            false,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            204,
            TitleRowStyle::default(),
            None,
        );
        let (x0, chip_w) = crate::mux::tab_slot_bounds(0, 1, width, 0, 0).unwrap();
        let chip_px = buffer[x0 + chip_w / 2];
        assert_eq!(
            unpack_rgb(chip_px),
            active_tab_row_rgb(active_chip_bg(default_theme(), [0, 0, 255]), 0, bar_h)
        );
        assert_eq!(alpha_of(chip_px), 204, "the chip carries chrome_opacity");
        assert!(
            buffer.iter().any(|&px| alpha_of(px) == OPAQUE_ALPHA),
            "strip ink stays opaque"
        );
    }

    #[test]
    fn pane_handle_chips_follow_the_tab_content_inset() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let tabs = [crate::mux::TabInfo {
            title: "one".into(),
            selected: false,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 2,
            focused_handle: Some(0),
            handle_titles: vec!["one".into(), "two".into()],
            handle_active: vec![false, false],
            git_label: None,
            pane_title: None,
        }];
        let inset = 16;
        let width = font.cell_w * 40;
        let bar_h = font.cell_h * 2;
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip_with_theme(
            default_theme(),
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0, 0, 255],
            None,
            0,
            0,
            inset,
            false,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );

        let handle_y = font.cell_h + 2;
        let tab_ground = pack_argb(OPAQUE_ALPHA, default_theme().chrome_bg);
        assert_eq!(
            buffer[handle_y * width + inset - 1],
            tab_ground,
            "the pane chip must not paint inside the left content inset"
        );
        assert_ne!(
            buffer[handle_y * width + inset],
            tab_ground,
            "the first pane chip begins at the pane content inset"
        );
    }

    #[test]
    fn pane_outline_carries_the_pane_surface_alpha() {
        let width = 12;
        let height = 8;
        let ground_alpha = 64;
        let border_alpha = 153;
        let mut buffer = vec![pack_argb(ground_alpha, [0, 0, 0]); width * height];
        rasterize_pane_chrome_with_theme(
            default_theme(),
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            true,
            false,
            None,
            None,
            false,
            [0, 0, 255],
            border_alpha,
        );
        assert_eq!(alpha_of(buffer[0]), border_alpha);
        assert_eq!(alpha_of(buffer[width - 1]), border_alpha);
        assert_eq!(alpha_of(buffer[(height - 1) * width]), border_alpha);
        assert_eq!(alpha_of(buffer[width + 1]), ground_alpha);
    }

    #[test]
    fn focus_change_repaints_active_and_inactive_alphas() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(1, 1, 0);
        screen.set_style(Style {
            background: Color::Rgb { r: 255, g: 0, b: 0 },
            ..Style::default()
        });
        screen.put_char(' ');
        let cw = font.cell_w;
        let ch = font.cell_h;
        let stride = cw * 2;
        let backdrop = pack_rgb([0, 0, 255]);
        let opaque = pack_rgb([255, 0, 0]);
        let mixed = pack_rgb(mix_rgb([0, 0, 255], [255, 0, 0], 128));
        let paint = |focus_left: bool| {
            let mut buf = vec![backdrop; stride * ch];
            let left = ScreenPaint {
                bg_weight: opacity_to_weight(if focus_left { 1.0 } else { 0.5 }),
                ..ScreenPaint::default()
            };
            let right = ScreenPaint {
                bg_weight: opacity_to_weight(if focus_left { 0.5 } else { 1.0 }),
                ..ScreenPaint::default()
            };
            rasterize_screen_at_with_theme(
                default_theme(),
                &screen,
                &font,
                false,
                CursorShape::Block,
                0,
                None,
                &mut buf,
                stride,
                0,
                0,
                cw,
                ch,
                left,
            );
            rasterize_screen_at_with_theme(
                default_theme(),
                &screen,
                &font,
                false,
                CursorShape::Block,
                0,
                None,
                &mut buf,
                stride,
                cw,
                0,
                cw,
                ch,
                right,
            );
            (buf[0], buf[cw])
        };
        assert_eq!(paint(true), (opaque, mixed));
        assert_eq!(paint(false), (mixed, opaque));
    }

    #[test]
    fn scaled_window_samples_source_column() {
        // 2×1 red|green; blit the right pixel into a 2×2 dest.
        let stride = 4usize;
        let mut buf = vec![pack_rgb([255, 255, 255]); 16];
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        blit_rgba_scaled_window(
            &mut buf, stride, &rgba, 2, 1, 0, 1, 1, 1, 1, 2, 2, 0, 0, 4, 4,
        );
        assert_eq!(unpack_rgb(buf[stride + 1]), [0, 255, 0]);
        assert_eq!(unpack_rgb(buf[2 * stride + 2]), [0, 255, 0]);
        assert_eq!(unpack_rgb(buf[stride + 3]), [255, 255, 255]);
    }

    #[test]
    fn placeholder_cell_skips_glyph_ink() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.set_style(Style {
            foreground: Color::Rgb { r: 255, g: 0, b: 0 },
            ..Style::default()
        });
        screen.put_char(KITTY_PLACEHOLDER);
        screen.put_char('A');
        let w = 2 * font.cell_w;
        let h = font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen_at_with_theme(
            default_theme(),
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            ScreenPaint::default(),
        );
        let bg = pack_rgb(default_theme().default_bg);
        let mut placeholder_ink = 0usize;
        let mut letter_ink = 0usize;
        for y in 0..h {
            for x in 0..font.cell_w {
                if buf[y * w + x] != bg {
                    placeholder_ink += 1;
                }
                if buf[y * w + font.cell_w + x] != bg {
                    letter_ink += 1;
                }
            }
        }
        assert_eq!(
            placeholder_ink, 0,
            "U+10EEEE must not paint Last Resort tofu"
        );
        assert!(letter_ink > 0, "control letter 'A' must still paint");
    }

    #[test]
    fn kitty_placeholder_tiles_sample_png_columns() {
        const PNG_2X1_RG_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8DwHwQBEPgD/U6VwW8AAAAASUVORK5CYII=";
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let mut emulator = Emulator::new(2, 1, 0);
        emulator.set_cell_pixels(font.cell_w as u32, font.cell_h as u32);
        let transmit = format!("\x1b_Ga=t,t=d,f=100,i=1;{PNG_2X1_RG_B64}\x1b\\");
        let _ = emulator.feed(transmit.as_bytes());
        let _ = emulator.feed(b"\x1b_Ga=p,U=1,i=1,c=2,r=1\x1b\\");
        let cells =
            format!("\x1b[38:2:0:0:1m{KITTY_PLACEHOLDER}\u{0305}\u{0305}{KITTY_PLACEHOLDER}");
        let _ = emulator.feed(cells.as_bytes());
        let w = 2 * font.cell_w;
        let h = font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen_at_with_theme(
            default_theme(),
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            ScreenPaint::default(),
        );
        blit_kitty_placeholders(&emulator, &font, 0, &mut buf, w, 0, 0, w, h);
        let tile_l = prismattyc_emulator::kitty_placeholder::placeholder_tile(
            2,
            1,
            2,
            1,
            font.cell_w as u32,
            font.cell_h as u32,
            0,
            0,
        )
        .expect("left tile");
        let tile_r = prismattyc_emulator::kitty_placeholder::placeholder_tile(
            2,
            1,
            2,
            1,
            font.cell_w as u32,
            font.cell_h as u32,
            0,
            1,
        )
        .expect("right tile");
        let lx = tile_l.dst_ox as usize + (tile_l.dst_w as usize / 2);
        let ly = tile_l.dst_oy as usize + (tile_l.dst_h as usize / 2);
        let rx = font.cell_w + tile_r.dst_ox as usize + (tile_r.dst_w as usize / 2);
        let ry = tile_r.dst_oy as usize + (tile_r.dst_h as usize / 2);
        assert_eq!(
            unpack_rgb(buf[ly * w + lx]),
            [255, 0, 0],
            "left tile is red"
        );
        assert_eq!(
            unpack_rgb(buf[ry * w + rx]),
            [0, 255, 0],
            "right tile is green"
        );
    }

    const RED_1X1_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";

    fn rasterize_direct_kitty(emulator: &Emulator, font: &FontMetrics) -> (Vec<u32>, usize, usize) {
        let w = emulator.screen().columns() * font.cell_w;
        let h = emulator.screen().rows() * font.cell_h;
        let mut buf = vec![pack_rgb(DEFAULT_BG); w * h];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            emulator.screen(),
            font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint::default(),
            emulator.images(),
            emulator.screen().scrolled_lines(),
        );
        blit_direct_kitty_images(
            emulator.images(),
            emulator.screen().scrolled_lines(),
            0,
            font,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
        );
        (buf, w, h)
    }

    #[test]
    fn direct_kitty_opaque_image_covers_every_cell_in_its_grid() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 24usize;
        let rows = 12usize;
        let mut emulator = Emulator::new(cols, rows + 4, 0);
        emulator.set_cell_pixels(font.cell_w as u32, font.cell_h as u32);
        let seq = format!("\x1b_Ga=T,t=d,f=100,i=7,c={cols},r={rows},q=2;{RED_1X1_PNG_B64}\x1b\\");
        let _ = emulator.feed(seq.as_bytes());
        let _ = emulator.feed("\n".repeat(rows).as_bytes());
        let img = emulator.images().first().expect("placed");
        assert_eq!((img.cols as usize, img.rows as usize), (cols, rows));
        let (buf, w, _) = rasterize_direct_kitty(&emulator, &font);
        let red = pack_rgb([255, 0, 0]);
        let bg = pack_rgb(DEFAULT_BG);
        let cw = font.cell_w;
        let ch = font.cell_h;
        for row in 0..rows {
            let mut bg_pixels = 0usize;
            let mut red_pixels = 0usize;
            for y in row * ch..(row + 1) * ch {
                for x in 0..cols * cw {
                    let px = buf[y * w + x];
                    if px == bg {
                        bg_pixels += 1;
                    }
                    if px == red {
                        red_pixels += 1;
                    }
                }
            }
            assert_eq!(
                bg_pixels, 0,
                "row {row} must not show a default-bg band over the image (bg={bg_pixels}, red={red_pixels})"
            );
            assert!(
                red_pixels > cols * cw * ch / 2,
                "row {row} must stay image pixels (red={red_pixels})"
            );
        }
    }

    #[test]
    fn direct_kitty_demo_png_has_no_full_width_grey_cell_row() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let png = include_bytes!("../../../demo/parts/prismattyc-256.png");
        let b64 = b64_std(png);
        let cols = 24usize;
        let rows = 12usize;
        let mut emulator = Emulator::new(cols, rows + 4, 0);
        emulator.set_cell_pixels(font.cell_w as u32, font.cell_h as u32);
        let seq = format!("\x1b_Ga=T,t=d,f=100,i=7,c={cols},r={rows},q=2;{b64}\x1b\\");
        let _ = emulator.feed(seq.as_bytes());
        let _ = emulator.feed("\n".repeat(rows).as_bytes());
        let img = emulator.images().first().expect("placed demo png");
        assert_eq!(img.width, 256);
        assert_eq!(img.height, 256);
        let (buf, w, _) = rasterize_direct_kitty(&emulator, &font);
        let bg = pack_rgb(DEFAULT_BG);
        let grey = pack_rgb([208, 208, 208]);
        let cw = font.cell_w;
        let ch = font.cell_h;
        for row in 5..=6 {
            let mut colourful_centre = 0usize;
            for col in 8..16 {
                let mut other = 0usize;
                for y in row * ch..(row + 1) * ch {
                    for x in col * cw..(col + 1) * cw {
                        let px = buf[y * w + x];
                        if px != bg && px != grey {
                            other += 1;
                        }
                    }
                }
                if other * 2 > cw * ch {
                    colourful_centre += 1;
                }
            }
            assert!(
                colourful_centre >= 6,
                "row {row} hides the image under a band (colourful centre cells={colourful_centre})"
            );
        }
    }

    fn transparent_placement(id: u32, cols: u16, rows: u16, line: u64, col: u16) -> PlacedImage {
        PlacedImage {
            id,
            image_number: None,
            placement_id: 0,
            z: 0,
            rgba: std::sync::Arc::from([0u8, 0, 0, 0]),
            width: 1,
            height: 1,
            cols,
            rows,
            anchor_abs_line: line,
            anchor_col: col,
        }
    }

    fn opaque_rgb_placement(
        id: u32,
        rgb: [u8; 3],
        cols: u16,
        rows: u16,
        line: u64,
        col: u16,
    ) -> PlacedImage {
        PlacedImage {
            id,
            image_number: None,
            placement_id: 0,
            z: 0,
            rgba: std::sync::Arc::from([rgb[0], rgb[1], rgb[2], 255]),
            width: 1,
            height: 1,
            cols,
            rows,
            anchor_abs_line: line,
            anchor_col: col,
        }
    }

    #[test]
    fn direct_kitty_skips_inverse_and_glyph_under_image() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 24usize;
        let mut emulator = Emulator::new(cols, 3, 0);
        emulator.set_cell_pixels(font.cell_w as u32, font.cell_h as u32);
        let _ = emulator.feed(b"\x1b[2;1H\x1b[7m");
        let _ = emulator.feed(&vec![b'X'; cols]);
        let _ = emulator.feed(b"\x1b[0m");
        let w = cols * font.cell_w;
        let h = 3 * font.cell_h;
        let inverse_bg = pack_rgb(DEFAULT_FG);
        let ch = font.cell_h;
        let mut leaked = vec![pack_rgb(DEFAULT_BG); w * h];
        rasterize_screen_at_with_theme(
            default_theme(),
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut leaked,
            w,
            0,
            0,
            w,
            h,
            ScreenPaint::default(),
        );
        assert!(
            leaked[ch * w..(2 * ch) * w].contains(&inverse_bg),
            "precondition: inverse cell background on the image row without skip"
        );
        let img = transparent_placement(1, cols as u16, 1, 1, 0);
        let mut buf = vec![pack_rgb(DEFAULT_BG); w * h];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint::default(),
            std::slice::from_ref(&img),
            0,
        );
        blit_direct_kitty_images(&[img], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let bg = pack_rgb(DEFAULT_BG);
        for y in ch..2 * ch {
            for x in 0..w {
                assert_eq!(
                    buf[y * w + x],
                    bg,
                    "covered cells must skip inverse and glyph paint"
                );
            }
        }
    }

    #[test]
    fn direct_kitty_transparent_pixels_match_default_bg_not_pane_backdrop() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 8usize;
        let rows = 3usize;
        let emulator = Emulator::new(cols, rows, 0);
        let w = cols * font.cell_w;
        let h = rows * font.cell_h;
        let theme = default_theme();
        let pane_backdrop = pack_rgb(theme.pane_backdrop);
        let default_bg = pack_rgb(theme.default_bg);
        assert_ne!(
            pane_backdrop, default_bg,
            "precondition: pane_backdrop differs from default_bg"
        );
        let mut buf = vec![pane_backdrop; w * h];
        let img = transparent_placement(1, cols as u16, 1, 1, 0);
        rasterize_screen_at_with_theme_options(
            theme,
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint::default(),
            std::slice::from_ref(&img),
            0,
        );
        blit_direct_kitty_images(&[img], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let ch = font.cell_h;
        for y in ch..2 * ch {
            for x in 0..w {
                assert_ne!(
                    buf[y * w + x],
                    pane_backdrop,
                    "transparent PNG must not show pane_backdrop"
                );
                assert_eq!(
                    buf[y * w + x],
                    default_bg,
                    "transparent PNG pixels over plain cells equal default_bg"
                );
            }
        }
    }

    #[test]
    fn direct_kitty_keeps_explicit_ansi_bg_under_transparent_image() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 8usize;
        let mut emulator = Emulator::new(cols, 3, 0);
        emulator.set_cell_pixels(font.cell_w as u32, font.cell_h as u32);
        let _ = emulator.feed(b"\x1b[2;1H\x1b[48;2;208;208;208m");
        let _ = emulator.feed(&vec![b' '; cols]);
        let _ = emulator.feed(b"\x1b[0m");
        let w = cols * font.cell_w;
        let h = 3 * font.cell_h;
        let grey = pack_rgb([208, 208, 208]);
        let img = transparent_placement(1, cols as u16, 1, 1, 0);
        let mut buf = vec![pack_rgb(DEFAULT_BG); w * h];
        rasterize_screen_at_with_theme_options(
            default_theme(),
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint::default(),
            std::slice::from_ref(&img),
            0,
        );
        blit_direct_kitty_images(&[img], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let ch = font.cell_h;
        for y in ch..2 * ch {
            for x in 0..w {
                assert_eq!(
                    buf[y * w + x],
                    grey,
                    "explicit ANSI background stays under transparent pixels"
                );
            }
        }
    }

    #[test]
    fn direct_kitty_transparent_pixels_keep_background_image() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 8usize;
        let rows = 3usize;
        let emulator = Emulator::new(cols, rows, 0);
        let w = cols * font.cell_w;
        let h = rows * font.cell_h;
        let backdrop = pack_argb(OPAQUE_ALPHA, [10, 80, 160]);
        let mut buf = vec![backdrop; w * h];
        let img = transparent_placement(1, cols as u16, 1, 1, 0);
        rasterize_screen_at_with_theme_options(
            default_theme(),
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint {
                skip_default_bg: true,
                ..ScreenPaint::default()
            },
            std::slice::from_ref(&img),
            0,
        );
        blit_direct_kitty_images(&[img], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let ch = font.cell_h;
        let default_bg = pack_rgb(DEFAULT_BG);
        for y in ch..2 * ch {
            for x in 0..w {
                assert_ne!(
                    buf[y * w + x],
                    default_bg,
                    "transparent image must not flatten the backdrop to default_bg"
                );
                assert_eq!(
                    buf[y * w + x],
                    backdrop,
                    "transparent PNG pixels must equal the background-image layer"
                );
            }
        }
    }

    #[test]
    fn direct_kitty_transparent_pixels_keep_surface_alpha() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 8usize;
        let rows = 3usize;
        let emulator = Emulator::new(cols, rows, 0);
        let w = cols * font.cell_w;
        let h = rows * font.cell_h;
        let theme = default_theme();
        let surface = pack_argb(128, theme.pane_backdrop);
        let mut buf = vec![surface; w * h];
        let img = transparent_placement(1, cols as u16, 1, 1, 0);
        rasterize_screen_at_with_theme_options(
            theme,
            emulator.screen(),
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buf,
            w,
            0,
            0,
            w,
            h,
            false,
            &[],
            ScreenPaint {
                default_bg_alpha: 128,
                ..ScreenPaint::default()
            },
            std::slice::from_ref(&img),
            0,
        );
        blit_direct_kitty_images(&[img], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let ch = font.cell_h;
        for y in ch..2 * ch {
            for x in 0..w {
                let px = buf[y * w + x];
                assert_eq!(alpha_of(px), 128, "image rect must keep surface alpha 128");
                assert_eq!(
                    unpack_rgb(px),
                    theme.default_bg,
                    "covered default-bg cells paint default_bg at surface alpha"
                );
            }
        }
    }

    #[test]
    fn direct_kitty_overlap_keeps_lower_image_under_transparent_top() {
        let Ok(font) = FontMetrics::load_baked(16.0) else {
            return;
        };
        let cols = 6usize;
        let rows = 3usize;
        let w = cols * font.cell_w;
        let h = rows * font.cell_h;
        let mut buf = vec![pack_rgb(DEFAULT_BG); w * h];
        let lower = opaque_rgb_placement(1, [255, 0, 0], 4, 2, 0, 0);
        let upper = transparent_placement(2, 2, 2, 0, 1);
        blit_direct_kitty_images(&[lower, upper], 0, 0, &font, &mut buf, w, 0, 0, w, h);
        let red = pack_rgb([255, 0, 0]);
        let cw = font.cell_w;
        let ch = font.cell_h;
        for y in 0..2 * ch {
            for x in cw..3 * cw {
                assert_eq!(
                    buf[y * w + x],
                    red,
                    "transparent overlap must leave the earlier image visible"
                );
            }
        }
    }

    fn b64_std(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let n = (u32::from(bytes[i]) << 16)
                | (u32::from(bytes[i + 1]) << 8)
                | u32::from(bytes[i + 2]);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push(T[(n & 63) as usize] as char);
            i += 3;
        }
        match bytes.len() - i {
            1 => {
                let n = u32::from(bytes[i]) << 16;
                out.push(T[((n >> 18) & 63) as usize] as char);
                out.push(T[((n >> 12) & 63) as usize] as char);
                out.push('=');
                out.push('=');
            }
            2 => {
                let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
                out.push(T[((n >> 18) & 63) as usize] as char);
                out.push(T[((n >> 12) & 63) as usize] as char);
                out.push(T[((n >> 6) & 63) as usize] as char);
                out.push('=');
            }
            _ => {}
        }
        out
    }

    #[test]
    fn block_full_and_shades_cover_whole_cell() {
        let full = block_element_coverage('\u{2588}', 6, 12).expect("full block");
        assert!(
            full.iter().all(|&a| a == 255),
            "full block must fill the cell"
        );
        let light = block_element_coverage('\u{2591}', 6, 12).expect("light shade");
        assert!(
            light.iter().all(|&a| a == 64),
            "shade is partial but whole-cell"
        );
    }

    #[test]
    fn block_halves_split_the_cell() {
        let (w, h) = (6usize, 12usize);
        let left = block_element_coverage('\u{258C}', w, h).expect("left half");
        for y in 0..h {
            for x in 0..w {
                let on = left[y * w + x] == 255;
                assert_eq!(on, x < w / 2, "left half fills only the left columns");
            }
        }
        let upper = block_element_coverage('\u{2580}', w, h).expect("upper half");
        for y in 0..h {
            for x in 0..w {
                let on = upper[y * w + x] == 255;
                assert_eq!(on, y < h / 2, "upper half fills only the top rows");
            }
        }
    }

    #[test]
    fn block_quadrant_fills_one_corner() {
        let (w, h) = (6usize, 12usize);
        let tl = block_element_coverage('\u{2598}', w, h).expect("upper-left quadrant");
        for y in 0..h {
            for x in 0..w {
                let on = tl[y * w + x] == 255;
                assert_eq!(on, x < w / 2 && y < h / 2, "only the top-left quadrant");
            }
        }
    }

    #[test]
    fn block_neighbors_tile_without_gaps() {
        // Two full blocks in adjacent columns must leave no unpainted column
        // between them: each fills its whole width, so the boundary is seamless.
        let (w, h) = (7usize, 5usize); // odd width exercises rounding
        let cov = block_element_coverage('\u{2588}', w, h).expect("full block");
        for (x, &alpha) in cov.iter().take(w).enumerate() {
            assert!(alpha == 255, "column {x} of a full block must be painted");
        }
    }

    #[test]
    fn non_block_char_is_not_procedural() {
        assert!(block_element_coverage('A', 6, 12).is_none());
        assert!(block_element_coverage('\u{2500}', 6, 12).is_none());
        assert!(box_drawing_coverage('A', 6, 12).is_none());
        assert!(braille_coverage('A', 6, 12).is_none());
        assert!(sprite_coverage('A', 6, 12).is_none());
    }

    #[test]
    fn braille_blank_is_empty_and_full_is_eight_dots() {
        let (w, h) = (10usize, 20usize);
        let blank = braille_coverage('\u{2800}', w, h).expect("blank");
        assert!(blank.iter().all(|&a| a == 0), "U+2800 must paint nothing");
        let full = braille_coverage('\u{28FF}', w, h).expect("full");
        let ink = full.iter().filter(|&&a| a == 255).count();
        let empty = full.iter().filter(|&&a| a == 0).count();
        assert!(ink >= 8, "eight dots need pixels: {ink}");
        assert!(
            empty > 0,
            "gaps between dots must remain so they read as dots"
        );
        assert!(ink < w * h, "U+28FF must not fill the cell solid");
        assert_eq!(
            sprite_coverage('\u{28FF}', w, h).as_deref(),
            Some(full.as_slice())
        );
        for (col, row) in [
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 0),
            (1, 1),
            (1, 2),
            (1, 3),
        ] {
            let x0 = if col == 0 { 0 } else { w / 2 };
            let x1 = if col == 0 { w / 2 } else { w };
            let y0 = row * h / 4;
            let y1 = if row == 3 { h } else { (row + 1) * h / 4 };
            let mut tile_ink = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    if full[y * w + x] == 255 {
                        tile_ink += 1;
                    }
                }
            }
            assert!(tile_ink > 0, "dot ({col},{row}) must paint");
        }
    }

    #[test]
    fn braille_left_column_stays_in_the_left_half() {
        let (w, h) = (7usize, 13usize);
        let left = braille_coverage('\u{2847}', w, h).expect("left column bits 1,2,3,7");
        assert_eq!(left.len(), w * h);
        for y in 0..h {
            for x in w / 2..w {
                assert_eq!(left[y * w + x], 0, "right half must stay empty at {x},{y}");
            }
        }
        assert!(left.contains(&255), "left column must paint");
    }

    #[test]
    fn powerline_right_triangle_fills_left_edge() {
        let (w, h) = (8usize, 16usize);
        let cov = powerline_triangle_coverage('\u{E0B0}', w, h).expect("");
        for y in 0..h {
            assert_eq!(cov[y * w], 255, "left edge of  must be filled at row {y}");
        }
        assert_eq!(cov[w / 2 - 1], 0, "top-right of  stays empty");
    }

    #[test]
    fn sextant_fullish_paints_and_emptyish_is_sparse() {
        let (w, h) = (6usize, 12usize);
        let first = sextant_coverage('\u{1FB00}', w, h).expect("sextant-1");
        assert!(first.contains(&255));
        assert!(first.contains(&0));
    }

    #[test]
    fn corner_triangle_lower_right_includes_bottom_right() {
        let (w, h) = (8usize, 8usize);
        let cov = corner_triangle_coverage('\u{25E2}', w, h).expect("◢");
        assert_eq!(cov[(h - 1) * w + (w - 1)], 255);
        assert_eq!(cov[0], 0);
    }

    #[test]
    fn box_drawing_light_strokes_span_edges_with_exact_connected_counts() {
        // Font ─ leaves gaps at cell edges (Claude composer looks dashed).
        // Ghostty draws a centered stroke 0..cell_w.
        for (w, h) in [(8usize, 16usize), (36, 72), (7, 13)] {
            let light = box_light_stroke(w, h);
            let cov = box_drawing_coverage('─', w, h).expect("─");
            assert_eq!(
                cov.iter().filter(|&&alpha| alpha == 255).count(),
                w * light,
                "─ must contain exactly one light stroke at {w}x{h}"
            );
            assert_binary_and_connected(&cov, w, h, '─');
            let y = (h - light) / 2;
            for x in 0..w {
                assert_eq!(cov[y * w + x], 255, "column {x} of ─ must be painted");
                assert_eq!(
                    cov.iter()
                        .skip(x)
                        .step_by(w)
                        .filter(|&&alpha| alpha == 255)
                        .count(),
                    light,
                    "column {x} of ─ must have exactly {light} light pixels"
                );
            }
            assert_eq!(cov[0], 0, "top row stays empty at {w}x{h}");

            let cov_v = box_drawing_coverage('│', w, h).expect("│");
            assert_eq!(
                cov_v.iter().filter(|&&alpha| alpha == 255).count(),
                h * light,
                "│ must contain exactly one light stroke at {w}x{h}"
            );
            assert_binary_and_connected(&cov_v, w, h, '│');
            let x = (w - light) / 2;
            for row in 0..h {
                assert_eq!(cov_v[row * w + x], 255, "row {row} of │ must be painted");
                assert_eq!(
                    cov_v[row * w..(row + 1) * w]
                        .iter()
                        .filter(|&&alpha| alpha == 255)
                        .count(),
                    light,
                    "row {row} of │ must have exactly {light} light pixels"
                );
            }
        }
    }

    #[test]
    fn dashed_box_lines_reach_the_right_edge_at_narrow_widths() {
        for ch in ['┄', '┅', '┈', '┉', '╌', '╍'] {
            for width in [13usize, 9] {
                let height = 16;
                let cov = box_drawing_coverage(ch, width, height).expect("dashed box line");
                let light = box_light_stroke(width, height);
                let y = (height - light) / 2;
                assert_eq!(cov[y * width + width - 1], 255, "{ch} misses right edge");
                assert!(
                    cov[y * width..(y + light) * width]
                        .iter()
                        .filter(|&&alpha| alpha == 0)
                        .count()
                        > 0,
                    "{ch} must keep gaps between dashes"
                );
            }
        }
        for ch in ['┆', '┇', '┊', '┋', '╎', '╏'] {
            let width = 13;
            let height = 9;
            let cov = box_drawing_coverage(ch, width, height).expect("dashed box line");
            let light = box_light_stroke(width, height);
            let x = (width - light) / 2;
            assert_eq!(
                cov[(height - 1) * width + x],
                255,
                "{ch} misses bottom edge"
            );
        }
    }

    #[test]
    fn sprite_predicate_matches_procedural_character_sets() {
        for ch in ['─', '╭', '┄', '█', '⠿', '', '\u{1FB00}', '◢'] {
            assert!(is_sprite(ch), "{ch} must be classified as a sprite");
            assert!(
                sprite_coverage(ch, 13, 16).is_some(),
                "{ch} must have coverage"
            );
        }
        for ch in ['A', '╳'] {
            assert!(!is_sprite(ch), "{ch} must stay font-drawn");
            assert!(sprite_coverage(ch, 13, 16).is_none());
        }
    }

    #[test]
    fn box_drawing_arcs_are_binary_and_four_connected_at_square_and_tall_sizes() {
        for (w, h) in [(16usize, 16usize), (36, 72), (7, 13)] {
            for ch in ['╭', '╮', '╯', '╰'] {
                let cov = box_drawing_coverage(ch, w, h).expect("arc");
                assert_binary_and_connected(&cov, w, h, ch);
            }
        }
    }

    #[test]
    fn box_drawing_arcs_meet_straight_strokes_at_even_stroke_widths() {
        for (w, h) in [(16usize, 32usize), (36, 72)] {
            let light = box_light_stroke(w, h);
            assert_eq!(light % 2, 0, "regression sizes must use even strokes");
            let horizontal = box_drawing_coverage('─', w, h).expect("─");
            let vertical = box_drawing_coverage('│', w, h).expect("│");
            let x0 = (w - light) / 2;
            let y0 = (h - light) / 2;
            let x1 = x0 + light;
            let y1 = y0 + light;
            let cx = (x0 + x1) as f64 / 2.0;
            let cy = (y0 + y1) as f64 / 2.0;
            let radius = cx.min(w as f64 - cx).min(cy).min(h as f64 - cy);

            for ch in ['╭', '╮', '╯', '╰'] {
                let cov = box_drawing_coverage(ch, w, h).expect("arc");
                let corner_x = if matches!(ch, '╭' | '╰') {
                    1.0
                } else {
                    -1.0
                };
                let corner_y = if matches!(ch, '╭' | '╮') {
                    1.0
                } else {
                    -1.0
                };
                let arc_cy = cy + corner_y * radius;
                let horizontal_edge = if corner_x > 0.0 { w - 1 } else { 0 };
                let vertical_edge = if corner_y > 0.0 { h - 1 } else { 0 };

                for row in 0..h {
                    assert_eq!(
                        cov[row * w + horizontal_edge],
                        horizontal[row * w + horizontal_edge],
                        "{ch} horizontal seam row {row} at {w}x{h}"
                    );
                }
                for col in 0..w {
                    assert_eq!(
                        cov[vertical_edge * w + col],
                        vertical[vertical_edge * w + col],
                        "{ch} vertical seam column {col} at {w}x{h}"
                    );
                }

                let (run_y0, run_y1) = if corner_y > 0.0 {
                    ((arc_cy.ceil() as usize).min(h), h)
                } else {
                    (0, (arc_cy.floor() as usize + 1).min(h))
                };
                for row in run_y0..run_y1 {
                    assert_eq!(
                        &cov[row * w..(row + 1) * w],
                        &vertical[row * w..(row + 1) * w],
                        "{ch} vertical run must stay aligned at row {row} of {w}x{h}"
                    );
                }
            }
        }
    }

    #[test]
    fn box_drawing_arcs_share_stroke_centres_and_tile_edges() {
        let (w, h) = (16usize, 16usize);
        let light = box_light_stroke(w, h);
        let horizontal = box_drawing_coverage('─', w, h).expect("─");
        let vertical = box_drawing_coverage('│', w, h).expect("│");
        let y = (h - light) / 2;
        let x = (w - light) / 2;
        for ch in ['╭', '╮', '╯', '╰'] {
            let cov = box_drawing_coverage(ch, w, h).expect("arc");
            assert!(cov.contains(&255), "{ch} must paint");
            assert!(
                cov.iter().filter(|&&a| a == 255).count() > w / 2,
                "{ch} must include its curved segment and arms"
            );
            let curve_pixels = (0..h)
                .flat_map(|row| (0..w).map(move |col| (col, row)))
                .filter(|&(col, row)| col < x || col >= x + light || row < y || row >= y + light)
                .filter(|&(col, row)| cov[row * w + col] == 255)
                .count();
            assert!(
                curve_pixels > 0,
                "{ch} must paint inside its quarter-circle"
            );
            match ch {
                '╭' => {
                    assert_eq!(cov[y * w + (w - 1)], horizontal[y * w + (w - 1)]);
                    assert_eq!(cov[(h - 1) * w + x], vertical[(h - 1) * w + x]);
                }
                '╮' => {
                    assert_eq!(cov[y * w], horizontal[y * w]);
                    assert_eq!(cov[(h - 1) * w + x], vertical[(h - 1) * w + x]);
                }
                '╯' => {
                    assert_eq!(cov[y * w], horizontal[y * w]);
                    assert_eq!(cov[x], vertical[x]);
                }
                '╰' => {
                    assert_eq!(cov[y * w + (w - 1)], horizontal[y * w + (w - 1)]);
                    assert_eq!(cov[x], vertical[x]);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn themes_only_remap_defaults_and_ansi_zero_through_fifteen() {
        let dracula = crate::theme::load(
            Some("dracula"),
            std::path::Path::new("/tmp/prism/config.toml"),
        )
        .unwrap();
        assert_eq!(
            palette_rgb_with_theme(&dracula, 1),
            dracula.ansi[1],
            "ANSI red comes from the selected theme"
        );
        assert_ne!(palette_rgb_with_theme(&dracula, 1), palette_rgb(1));
        assert_eq!(
            palette_rgb_with_theme(&dracula, 16),
            palette_rgb(16),
            "xterm cube entry 16 remains stock"
        );
        assert_eq!(
            palette_rgb_with_theme(&dracula, 244),
            palette_rgb(244),
            "xterm grayscale remains stock"
        );
        let rgb = Color::Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        assert_eq!(
            color_rgb_with_theme(&dracula, rgb, true),
            [0x12, 0x34, 0x56],
            "guest truecolor passes through unchanged"
        );
    }

    #[test]
    fn theme_picker_hints_fit_fifty_cell_clip() {
        // Panel is 52 cells; text_x/text_right inset one cell, so the clip is 50.
        for hint in [THEME_PICKER_HINT_ROOT, THEME_PICKER_HINT_FAMILY] {
            let cells: usize = hint
                .chars()
                .map(|ch| prismattyc_core::char_display_width(ch).max(1))
                .sum();
            assert!(cells <= 50, "{hint:?} is {cells} display cells; clip is 50");
        }
    }

    #[test]
    fn theme_picker_renders_every_preview_ansi_swatch() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let preview = crate::theme::load(
            Some("catppuccin-mocha"),
            std::path::Path::new("/tmp/prism/config.toml"),
        )
        .unwrap();
        let (width, height) = (640, 420);
        let mut buffer = vec![pack_rgb(preview.default_bg); width * height];
        let names: Vec<String> = crate::theme::builtins()
            .iter()
            .map(|theme| theme.name.clone())
            .collect();
        let rows: Vec<ThemePickerRow<'_>> = names
            .iter()
            .map(|label| ThemePickerRow {
                label,
                branch: false,
            })
            .collect();
        rasterize_theme_picker(
            &font,
            &rows,
            Some(1),
            0,
            &preview,
            "Up/Down | Enter apply | Esc cancel",
            OverlaySurface::default(),
            &mut buffer,
            width,
            height,
            focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
        );
        for rgb in preview.ansi {
            assert!(
                buffer.contains(&pack_rgb(rgb)),
                "missing ANSI swatch #{:02x}{:02x}{:02x}",
                rgb[0],
                rgb[1],
                rgb[2]
            );
        }
    }

    #[test]
    fn rasterize_fills_buffer_without_panic() {
        let Ok(font) = FontMetrics::load(14.0) else {
            // CI images without fonts still compile; skip runtime.
            return;
        };
        let mut screen = Screen::new(10, 5, 100);
        for c in "hi".chars() {
            screen.put_char(c);
        }
        let w = 10 * font.cell_w;
        let h = 5 * font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen(&screen, &font, true, 0, None, &mut buf, w);
        // Background should not be all zero after paint (default bg is dark grey-ish).
        assert!(buf.iter().any(|&p| p != 0));
    }

    #[test]
    fn caret_block_fills_cell_underline_and_bar_are_strips() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let screen = Screen::new(2, 1, 0);
        let w = 2 * font.cell_w;
        let h = font.cell_h;
        let bg = pack_rgb(DEFAULT_BG);
        let caret = pack_rgb(DEFAULT_FG);

        let mut block = vec![0u32; w * h];
        rasterize_screen_at(
            &screen,
            &font,
            true,
            CursorShape::Block,
            0,
            None,
            &mut block,
            w,
            0,
            0,
            usize::MAX,
            usize::MAX,
        );
        assert_eq!(block[0], caret, "block caret fills the cell");
        assert_eq!(
            block[(font.cell_h / 2) * w],
            caret,
            "block caret fills mid-cell"
        );
        assert_eq!(block[font.cell_w], bg, "neighbor cell stays default bg");

        let mut underline = vec![0u32; w * h];
        rasterize_screen_at(
            &screen,
            &font,
            true,
            CursorShape::Underline,
            0,
            None,
            &mut underline,
            w,
            0,
            0,
            usize::MAX,
            usize::MAX,
        );
        let bar_h = (font.cell_h / 8).max(2);
        assert_eq!(underline[0], bg, "underline does not fill the cell");
        assert_eq!(
            underline[(font.cell_h - 1) * w],
            caret,
            "underline sits on the baseline"
        );
        assert_eq!(
            underline[(font.cell_h - bar_h) * w],
            caret,
            "underline height matches the bar strip"
        );

        let mut bar = vec![0u32; w * h];
        rasterize_screen_at(
            &screen,
            &font,
            true,
            CursorShape::Bar,
            0,
            None,
            &mut bar,
            w,
            0,
            0,
            usize::MAX,
            usize::MAX,
        );
        let bar_w = (font.cell_w / 8).max(2).min(font.cell_w);
        assert_eq!(bar[0], caret, "bar occupies the left strip");
        assert_eq!(bar[bar_w], bg, "bar does not fill the cell");
        assert_eq!(
            bar[(font.cell_h / 2) * w],
            caret,
            "bar runs the full cell height"
        );
    }

    #[test]
    fn capital_h_ink_sits_above_baseline_band() {
        let Ok(font) = FontMetrics::load(20.0) else {
            return;
        };
        let mut screen = Screen::new(4, 2, 10);
        screen.put_char('H');
        let w = 4 * font.cell_w;
        let h = 2 * font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen(&screen, &font, false, 0, None, &mut buf, w);
        let bg = pack_rgb(DEFAULT_BG);
        // Collect rows in the first cell that have non-background ink.
        let mut ink_rows = Vec::new();
        for y in 0..font.cell_h {
            for x in 0..font.cell_w {
                if buf[y * w + x] != bg {
                    ink_rows.push(y);
                    break;
                }
            }
        }
        assert!(!ink_rows.is_empty(), "expected ink for 'H' in first cell");
        let first = *ink_rows.first().unwrap();
        let last = *ink_rows.last().unwrap();
        // Capitals must occupy the upper portion of the cell, not only below baseline.
        let baseline_y = font.baseline.round() as usize;
        assert!(
            first < baseline_y,
            "H ink starts at row {first}, baseline band ~{baseline_y} — glyph Y sign wrong?"
        );
        assert!(
            last <= baseline_y + 2,
            "H ink ends at row {last}, far below baseline {baseline_y}"
        );
    }

    fn baked_source(name: &'static str, bytes: &'static [u8]) -> FontSource {
        FontSource::Baked { name, bytes }
    }

    #[test]
    fn lazy_chain_keeps_unused_outlines_unparsed() {
        let font = FontMetrics::load_sources(
            16.0,
            vec![
                baked_source(BAKED_MONO_NAME, BAKED_MONO),
                baked_source(BAKED_MONO_NAME, BAKED_MONO),
                baked_source(BAKED_SYMBOLS_NAME, BAKED_SYMBOLS),
            ],
        )
        .expect("bundled chain");
        assert_eq!(font.fonts.len(), 1);
        assert!(font.primary_covers('H'));
        assert!(matches!(font.paint_char('H'), GlyphPaint::Coverage(_)));
        for ch in [' ', '\u{2800}', '\u{feff}'] {
            assert!(matches!(font.paint_char(ch), GlyphPaint::Empty));
        }
        for candidate in &font.lazy_fonts {
            assert!(candidate.font.get().is_none());
            assert!(candidate.bytes.get().is_none());
        }
    }

    #[test]
    fn lazy_chain_loads_only_covering_face_and_reuses_exact_raster() {
        let mut font = FontMetrics::load_baked(16.0).expect("bundled font");
        font.lazy_fonts = vec![
            LazyFont::new(baked_source(BAKED_FALLBACK_NAME, BAKED_FALLBACK)),
            LazyFont::new(baked_source(BAKED_SYMBOLS_NAME, BAKED_SYMBOLS)),
            LazyFont::new(baked_source(BAKED_SYMBOLS_NAME, BAKED_SYMBOLS)),
        ];
        let ch = '\u{23f8}';
        assert!(!font.primary_covers(ch));
        let reference = Font::from_bytes(BAKED_SYMBOLS, FontSettings::default()).unwrap();
        let (expected, bitmap) = reference.rasterize(ch, font.px);
        for _ in 0..2 {
            let GlyphPaint::Coverage(glyph) = font.paint_char(ch) else {
                panic!("the lazy symbol face must paint the pause glyph");
            };
            assert_eq!(
                (glyph.width, glyph.height),
                (expected.width, expected.height)
            );
            assert_eq!((glyph.xmin, glyph.ymin), (expected.xmin, expected.ymin));
            assert_eq!(glyph.bitmap, bitmap);
        }
        assert!(
            font.lazy_fonts[0].font.get().is_none(),
            "cmap miss must stay unparsed"
        );
        assert!(font.lazy_fonts[1].font.get().unwrap().is_some());
        assert!(
            font.lazy_fonts[2].bytes.get().is_none(),
            "later covering face must stay unused"
        );
        assert!(font.fallback_fonts.borrow().is_empty());
    }

    #[test]
    fn lazy_chain_blank_and_absent_glyphs_do_not_load_later_faces() {
        let mut font = FontMetrics::load_baked(16.0).expect("bundled font");
        font.fonts.clear();
        font.lazy_fonts = vec![
            LazyFont::new(baked_source(BAKED_MONO_NAME, BAKED_MONO)),
            LazyFont::new(baked_source(BAKED_SYMBOLS_NAME, BAKED_SYMBOLS)),
        ];
        font.cover_cache.borrow_mut().insert('\u{10ffff}', None);
        assert!(matches!(font.paint_char('\u{10ffff}'), GlyphPaint::Empty));
        assert!(font.lazy_fonts.iter().all(|f| f.font.get().is_none()));
        for ch in ['\u{2800}', '\u{feff}'] {
            assert!(matches!(font.paint_char(ch), GlyphPaint::Empty));
        }
        assert!(font.lazy_fonts[0].font.get().unwrap().is_some());
        assert!(font.lazy_fonts[1].font.get().is_none());
        assert!(font.fallback_fonts.borrow().is_empty());
    }

    #[test]
    fn lazy_chain_skips_invalid_primary_and_fallback_candidates() {
        let mut font = FontMetrics::load_sources(
            16.0,
            vec![
                baked_source("invalid", b"invalid font bytes"),
                baked_source(BAKED_MONO_NAME, BAKED_MONO),
                baked_source("invalid fallback", b"invalid font bytes"),
                FontSource::Path(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("assets/fonts/NotoSansSymbols2-Regular.ttf"),
                ),
            ],
        )
        .expect("invalid primary must not prevent bundled fallback");
        assert_eq!(font.fonts.len(), 1);
        assert!(matches!(
            font.paint_char('\u{23f8}'),
            GlyphPaint::Coverage(_)
        ));
        assert!(font.lazy_fonts[1].font.get().unwrap().is_some());
        font.lazy_fonts.insert(
            0,
            LazyFont::new(FontSource::Path(PathBuf::from(
                "/nonexistent/prismattyc-test-font.ttf",
            ))),
        );
        assert!(matches!(
            font.paint_char('\u{23f8}'),
            GlyphPaint::Coverage(_)
        ));
        assert!(font.lazy_fonts[0].bytes.get().unwrap().is_none());
    }

    #[test]
    fn system_outline_loads_nonblank_glyph_and_reuses_face() {
        let mut font = FontMetrics::load_baked(16.0).expect("bundled font");
        let (expected, bitmap) = font.fonts[0].rasterize('H', font.px);
        let face = crate::system_fonts::CoveringFace {
            path: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/fonts/JetBrainsMonoNerdFont-Regular.ttf"),
            index: 0,
        };
        // Pin discovery to a shipped fixture. Exercise actual file loading and
        // rasterization without requiring a particular installed system font.
        for ch in ['H', '\u{2800}', '\u{10ffff}'] {
            font.cover_cache.borrow_mut().insert(ch, Some(face.clone()));
        }
        font.fonts.clear();
        assert!(font.fallback_fonts.borrow().is_empty());
        for _ in 0..2 {
            let GlyphPaint::Coverage(glyph) = font.paint_char('H') else {
                panic!("a missing primary glyph must use the system outline");
            };
            assert_eq!(
                (glyph.width, glyph.height),
                (expected.width, expected.height)
            );
            assert_eq!((glyph.xmin, glyph.ymin), (expected.xmin, expected.ymin));
            assert_eq!(glyph.bitmap, bitmap);
            assert!(glyph.bitmap.iter().any(|&pixel| pixel != 0));
            assert_eq!(font.fallback_fonts.borrow().len(), 1);
        }
        // A valid blank glyph and an absent glyph must not paint .notdef ink.
        assert!(font.paint_system_outline('\u{2800}', font.px).is_none());
        assert!(font.paint_system_outline('\u{10ffff}', font.px).is_none());
        assert_eq!(font.fallback_fonts.borrow().len(), 1);
    }

    #[test]
    fn blank_whitespace_never_discovers_system_fallbacks() {
        let font = FontMetrics::load_baked(16.0).expect("bundled font");
        for ch in [' ', '\u{00a0}', '\u{2007}', '\u{202f}', '\u{3000}'] {
            assert!(matches!(
                font.paint_char_fitting(ch, font.cell_w as i32, font.cell_h as i32),
                GlyphPaint::Empty
            ));
            assert!(font.paint_system_outline(ch, font.px).is_none());
            assert!(font.cover_cache.borrow().is_empty(), "U+{:04X}", ch as u32);
            assert!(
                font.fallback_fonts.borrow().is_empty(),
                "U+{:04X}",
                ch as u32
            );
        }
    }

    #[test]
    fn blank_covered_glyphs_do_not_search_another_face() {
        let font = FontMetrics::load_baked(16.0).expect("bundled font");
        // Braille blank has advance width but no ink. BOM has neither.
        // Neither is whitespace according to char::is_whitespace.
        for ch in ['\u{2800}', '\u{feff}'] {
            assert!(!ch.is_whitespace());
            let gid = font.fonts[0].lookup_glyph_index(ch);
            assert_ne!(gid, 0, "bundled face must cover U+{:04X}", ch as u32);
            let (metrics, bitmap) = font.fonts[0].rasterize_indexed(gid, font.px);
            assert!(bitmap.is_empty());
            if ch == '\u{2800}' {
                assert!(metrics.advance_width > 0.0);
            }
            assert!(matches!(font.paint_char(ch), GlyphPaint::Empty));
            assert!(font.cover_cache.borrow().is_empty(), "U+{:04X}", ch as u32);
            assert!(
                font.fallback_fonts.borrow().is_empty(),
                "U+{:04X}",
                ch as u32
            );
        }
    }

    #[test]
    fn color_emoji_and_symbol_fallbacks_paint_ink() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        match font.paint_char('\u{23F5}') {
            GlyphPaint::Coverage(g) => {
                assert!(g.bitmap.iter().any(|&b| b > 0), "U+23F5 outline ink");
            }
            GlyphPaint::Color(_) => panic!("U+23F5 must be outline, not color emoji"),
            GlyphPaint::Empty => panic!("U+23F5 must come from bundled Noto Sans Symbols 2"),
        }
        for ch in ['\u{1F512}', '\u{1F4C1}', '\u{1F4C4}', '\u{2728}'] {
            match font.paint_char(ch) {
                GlyphPaint::Empty => {
                    if font.emoji_fonts.is_empty() {
                        continue;
                    }
                }
                GlyphPaint::Coverage(g) => {
                    assert!(
                        g.bitmap.iter().any(|&b| b > 0),
                        "U+{:04X} coverage",
                        ch as u32
                    );
                }
                GlyphPaint::Color(g) => {
                    assert!(
                        g.rgba.chunks_exact(4).any(|p| p[3] > 0),
                        "U+{:04X} color alpha",
                        ch as u32
                    );
                }
            }
        }
        assert!(matches!(font.paint_char('\u{FFFE}'), GlyphPaint::Empty));
    }

    /// Grok Working spinner: ⋅ : ⸬ ⁙ (U+22C5 / colon / U+2E2C / U+2059).
    /// Nerd primaries omit ⸬ and ⁙; a complete mono fallback must supply ink
    /// that stays inside the cell.
    #[test]
    fn grok_working_spinner_frames_paint_ink_in_cell() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let frames = ['\u{22c5}', ':', '\u{2e2c}', '\u{2059}'];
        let mut painted = 0usize;
        for ch in frames {
            if matches!(font.paint_char(ch), GlyphPaint::Empty) {
                continue;
            }
            painted += 1;
            let mut screen = Screen::new(4, 2, 10);
            screen.put_char(ch);
            let w = 4 * font.cell_w;
            let h = 2 * font.cell_h;
            let mut buf = vec![0u32; w * h];
            rasterize_screen(&screen, &font, false, 0, None, &mut buf, w);
            let bg = pack_rgb(DEFAULT_BG);
            let mut ink_cols = Vec::new();
            for x in 0..font.cell_w {
                if (0..font.cell_h).any(|y| buf[y * w + x] != bg) {
                    ink_cols.push(x);
                }
            }
            assert!(
                !ink_cols.is_empty(),
                "U+{:04X} must paint ink in the cell",
                ch as u32
            );
            let left = *ink_cols.first().unwrap();
            let right = *ink_cols.last().unwrap();
            assert!(
                right < font.cell_w,
                "U+{:04X} ink reaches col {right} past cell_w={}",
                ch as u32,
                font.cell_w
            );
            if matches!(ch, '\u{2e2c}' | '\u{2059}') {
                assert!(
                    left <= font.cell_w / 2,
                    "U+{:04X} leftmost ink at {left} — left dots clipped?",
                    ch as u32
                );
                assert!(
                    ink_cols.len() >= 3,
                    "U+{:04X} only {} ink columns — cluster clipped?",
                    ch as u32,
                    ink_cols.len()
                );
            }
        }
        assert!(
            painted >= 2,
            "need at least colon plus one dotted frame; painted={painted}"
        );
    }

    /// Codex uses U+25C9 FISHEYE for the active-subagent count. The bundled
    /// Nerd face rasterizes it one pixel wider than the mono cell at 16 px;
    /// accepting that bitmap and relying on the cell clip flattens its ring.
    #[test]
    fn codex_subagent_fisheye_fits_inside_one_cell() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let GlyphPaint::Coverage(glyph) =
            font.paint_char_fitting('\u{25c9}', font.cell_w as i32, font.cell_h as i32)
        else {
            panic!("U+25C9 must resolve to outline coverage");
        };
        assert!(
            glyph.width <= font.cell_w,
            "U+25C9 fitted width {} exceeds cell_w={} and will clip",
            glyph.width,
            font.cell_w
        );
    }

    #[test]
    fn nerd_primary_still_resolves_working_spinner_frames() {
        use std::path::Path;
        let path = Path::new("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf");
        if !path.exists() {
            return;
        }
        let Ok(font) = FontMetrics::load_with(16.0, Some(path), &[]) else {
            return;
        };
        for ch in ['\u{22c5}', '\u{2e2c}', '\u{2059}'] {
            assert!(
                !matches!(font.paint_char(ch), GlyphPaint::Empty),
                "U+{:04X} empty with JetBrains Nerd primary — mono fallback missing?",
                ch as u32
            );
        }
    }

    #[test]
    fn splash_paints_background_art_ink_and_spectrum() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let lines = crate::splash::layout(
            crate::splash::Page::Main,
            "0.0.0-test",
            0,
            Some(prismattyc_core::splash::SETTLED_MS),
        );
        let (w, h) = (640_usize, 400_usize);
        let bg = [0x10, 0x10, 0x18];
        let mut buffer = vec![0u32; w * h];
        rasterize_splash(
            &font,
            &lines,
            &mut buffer,
            w,
            h,
            bg,
            Some(prismattyc_core::splash::SETTLED_MS),
        );
        // Background fill reached the corners.
        assert_eq!(buffer[0], pack_rgb(bg));
        assert_eq!(buffer[h * w - 1], pack_rgb(bg));
        // Art painted ink (solid blocks) and at least one spectrum outline.
        let ink = pack_rgb(prismattyc_core::splash::INK);
        assert!(buffer.contains(&ink), "no ink art pixels");
        let spectrum_hit = prismattyc_core::splash::SPECTRUM
            .iter()
            .any(|rgb| buffer.contains(&pack_rgb(*rgb)));
        assert!(spectrum_hit, "no spectrum outline pixels");
    }

    #[test]
    fn splash_flare_star_overlays_the_letter_corner() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        // At rest the top-left star is light blue; nudged half a cell right
        // and up it must leave non-ink, non-background pixels inside the
        // P's corner cell (art column 0, row 0) and above the art row.
        let at = prismattyc_core::splash::SETTLED_MS + 2000;
        let lines = crate::splash::layout(crate::splash::Page::Main, "0.0.0-test", 0, Some(at));
        let (w, h) = (1100_usize, 420_usize);
        let bg = [0x10, 0x10, 0x18];
        let mut buffer = vec![0u32; w * h];
        rasterize_splash(&font, &lines, &mut buffer, w, h, bg, Some(at));
        let max_cols = lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
            .max()
            .unwrap();
        let col_x0 = (w - (max_cols * font.cell_w).min(w)) / 2;
        let row_y0 = (h - (lines.len() * font.cell_h).min(h)) / 2;
        let corner_x = col_x0 + ART_MARGIN_LEFT * font.cell_w;
        use prismattyc_core::splash::ART;
        let ink = pack_rgb(prismattyc_core::splash::INK);
        let bgp = pack_rgb(bg);
        // Pixels in the corner cell and the half-cell above it.
        let mut overlay = 0;
        for y in row_y0.saturating_sub(font.cell_h / 2)..row_y0 + font.cell_h {
            for x in corner_x..corner_x + font.cell_w {
                let px = buffer[y * w + x];
                if px != ink && px != bgp {
                    overlay += 1;
                }
            }
        }
        assert!(overlay > 0, "star did not overlay the P's corner");
        // Nudged up half a cell, the star and streak paint into the strip
        // above the art's first row, across the left margin and the corner.
        // Nothing else ever draws there.
        let strip = (row_y0 - font.cell_h / 2..row_y0)
            .flat_map(|y| (col_x0..corner_x + font.cell_w).map(move |x| (x, y)))
            .filter(|(x, y)| buffer[y * w + x] != bgp)
            .count();
        assert!(strip > 0, "flare glyphs were not nudged upward");
        // The rays below move with the star: the cell diagonally below-left
        // of the corner (row 1, col -2) has its right half painted, but the
        // far-left margin column's left half stays empty at every row.
        let far_left = (row_y0..row_y0 + ART.len() * font.cell_h)
            .flat_map(|y| (col_x0..col_x0 + font.cell_w / 2).map(move |x| (x, y)))
            .filter(|(x, y)| buffer[y * w + x] != bgp)
            .count();
        assert_eq!(
            far_left, 0,
            "left margin's first half-column should be empty"
        );
    }

    #[test]
    fn splash_rays_rotate_and_stay_out_of_the_art() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let (w, h) = (1100_usize, 420_usize);
        let bg = [0x10, 0x10, 0x18];
        let bgp = pack_rgb(bg);
        let base = prismattyc_core::splash::SETTLED_MS + 2000;
        let render = |ms: u64| {
            let lines = crate::splash::layout(crate::splash::Page::Main, "0.0.0-test", 0, Some(ms));
            let mut buffer = vec![0u32; w * h];
            rasterize_splash(&font, &lines, &mut buffer, w, h, bg, Some(ms));
            (lines, buffer)
        };
        let (lines, a) = render(base);
        // A quarter turn later the margin pixels differ (the arms moved);
        // the art itself is untouched because rays are clipped to it and
        // the flare levels barely move over a twinkle's quarter period.
        let (_, b) = render(base + (RAY_SPIN_MS / 4.0) as u64);
        let max_cols = lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
            .max()
            .unwrap();
        let col_x0 = (w - (max_cols * font.cell_w).min(w)) / 2;
        let row_y0 = (h - (lines.len() * font.cell_h).min(h)) / 2;
        let left_margin = col_x0..col_x0 + ART_MARGIN_LEFT * font.cell_w;
        let rows = row_y0.saturating_sub(font.cell_h)..row_y0 + ART.len() * font.cell_h;
        let painted = |buf: &[u32]| -> Vec<(usize, usize)> {
            rows.clone()
                .flat_map(|y| left_margin.clone().map(move |x| (x, y)))
                .filter(|(x, y)| buf[y * w + x] != bgp)
                .collect()
        };
        let (pa, pb) = (painted(&a), painted(&b));
        assert!(!pa.is_empty() && !pb.is_empty(), "rays not painted");
        assert_ne!(pa, pb, "rays did not move between frames");
        // Without a clock (topic-style call) the `╱` glyph cells paint as
        // glyphs instead; with one, rays are pixels: the frames differ.
        let lines0 = crate::splash::layout(crate::splash::Page::Main, "0.0.0-test", 0, Some(base));
        let mut plain = vec![0u32; w * h];
        rasterize_splash(&font, &lines0, &mut plain, w, h, bg, None);
        assert_ne!(painted(&plain), pa);
    }

    #[test]
    fn splash_ray_tapers_before_zero_alpha_endpoint() {
        let (stride, height) = (32_usize, 24_usize);
        let bg = pack_rgb([0, 0, 0]);
        let mut buffer = vec![bg; stride * height];
        let clip = RayClip {
            stride,
            height,
            art: (usize::MAX, usize::MAX, usize::MAX, usize::MAX),
        };

        // Use a non-half-pixel length so the endpoint is not reached by the
        // old truncated sample loop. The forward pixel is at the endpoint's
        // zero-alpha side and must remain untouched.
        draw_ray(
            &mut buffer,
            &clip,
            (10.0, 10.0),
            0.0,
            8.3,
            3.0,
            [255, 255, 255],
            1.0,
        );

        let pixel = |x: usize| unpack_rgb(buffer[10 * stride + x])[0];
        let run: Vec<u8> = (13..=18).map(pixel).collect();
        assert!(
            run.iter().any(|&p| p > 0),
            "ray start was not painted: {run:?}"
        );
        assert!(
            run.windows(2).all(|w| w[0] >= w[1]),
            "ray must be non-increasing along the arm: {run:?}"
        );
        let last = *run.last().unwrap();
        assert!(last < 40, "quadratic tail must be dim at the tip: {last}");
    }

    #[test]
    fn splash_ray_clamps_length_before_the_art_box() {
        let (stride, height) = (32_usize, 24_usize);
        let bg = pack_rgb([0, 0, 0]);
        let mut buffer = vec![bg; stride * height];
        let clip = RayClip {
            stride,
            height,
            art: (16, 0, 32, 24),
        };
        draw_ray(
            &mut buffer,
            &clip,
            (10.0, 10.0),
            0.0,
            20.0,
            2.0,
            [255, 255, 255],
            1.0,
        );
        let pixel = |x: usize| unpack_rgb(buffer[10 * stride + x])[0];
        assert!(pixel(12) > 0, "arm should paint in the margin");
        assert_eq!(pixel(16), 0, "arm must not enter the art box");
        assert_eq!(pixel(20), 0, "clamped arm must not reach far into the art");
        let run: Vec<u8> = (12..16).map(pixel).collect();
        assert!(
            run.windows(2).all(|w| w[0] >= w[1]),
            "clamped arm must taper before the art edge: {run:?}"
        );
    }

    #[test]
    fn splash_ray_leaves_a_star_centred_inside_the_art_box() {
        // The lower-right star's centre lies inside the art box (PT-88):
        // an arm that exits the box keeps its room past the edge, an arm
        // that stays inside gets none.
        let (stride, height) = (32_usize, 24_usize);
        let clip = RayClip {
            stride,
            height,
            art: (0, 0, 16, 24),
        };
        let center = (12.0, 12.0);
        let (start, len) = (2.0, 14.0);
        let out = clip.room(center, 1.0, 0.0, start, len);
        assert!(
            out > 4.0,
            "arm leaving the box has room past the edge: {out}"
        );
        assert_eq!(
            clip.room(center, -1.0, 0.0, start, len),
            0.0,
            "arm that stays inside the box has no room"
        );
        let bg = pack_rgb([0, 0, 0]);
        let mut buffer = vec![bg; stride * height];
        for theta in [0.0, std::f32::consts::PI] {
            draw_ray(
                &mut buffer,
                &clip,
                center,
                theta,
                len,
                start,
                [255, 255, 255],
                1.0,
            );
        }
        let pixel = |x: usize| unpack_rgb(buffer[12 * stride + x])[0];
        assert!(pixel(17) > 0, "exiting arm paints past the art edge");
        assert!(
            (0..16).all(|x| pixel(x) == 0),
            "nothing paints inside the art box"
        );
        let run: Vec<u8> = (16..26).map(pixel).collect();
        assert!(
            run.windows(2).all(|w| w[0] >= w[1]),
            "exiting arm still tapers: {run:?}"
        );
    }

    #[test]
    fn splash_both_stars_paint_pixel_rays_into_their_margins() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        use prismattyc_core::splash::{
            ART, ART_MARGIN, FLARE_GLYPH, FLARE_PEAK_GLYPH, RAY_GLYPH, STREAK_CORE_GLYPH,
            STREAK_GLYPH,
        };
        let (w, h) = (1100_usize, 420_usize);
        let bg = [0x10, 0x10, 0x18];
        let bgp = pack_rgb(bg);
        let ms = prismattyc_core::splash::SETTLED_MS + 2000;
        let lines = crate::splash::layout(crate::splash::Page::Main, "0.0.0-test", 0, Some(ms));
        let mut buffer = vec![0u32; w * h];
        rasterize_splash(&font, &lines, &mut buffer, w, h, bg, Some(ms));
        let max_cols = lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
            .max()
            .unwrap();
        let col_x0 = (w - (max_cols * font.cell_w).min(w)) / 2;
        let row_y0 = (h - (lines.len() * font.cell_h).min(h)) / 2;
        let (nudge_x, nudge_y) = (font.cell_w / 2, font.cell_h / 2);
        // Every cell rectangle a glyph paints into, with the flare group's
        // nudge applied, so only pixel arms remain outside the mask.
        let mut glyph_rects: Vec<(usize, usize)> = Vec::new();
        for (r, line) in lines.iter().enumerate() {
            let mut col = 0usize;
            for (text, _) in line {
                for ch in text.chars() {
                    let (x, y) = (col_x0 + col * font.cell_w, row_y0 + r * font.cell_h);
                    let flare = matches!(
                        ch,
                        FLARE_GLYPH | FLARE_PEAK_GLYPH | STREAK_GLYPH | STREAK_CORE_GLYPH
                    );
                    if flare {
                        let sx = if col < ART_MARGIN_LEFT {
                            x + nudge_x * 3 / 4
                        } else {
                            x - nudge_x - font.cell_w
                        };
                        glyph_rects.push((sx, y - nudge_y));
                    } else if ch != ' ' && ch != RAY_GLYPH {
                        glyph_rects.push((x, y));
                    }
                    col += 1;
                }
            }
        }
        let masked = |x: usize, y: usize| {
            glyph_rects
                .iter()
                .any(|&(gx, gy)| x >= gx && x < gx + font.cell_w && y >= gy && y < gy + font.cell_h)
        };
        let art_left = col_x0 + ART_MARGIN_LEFT * font.cell_w;
        let art_right = col_x0 + (art_frame_width() - ART_MARGIN) * font.cell_w;
        let rows = row_y0 - font.cell_h..row_y0 + ART.len() * font.cell_h;
        let count = |xs: std::ops::Range<usize>| {
            rows.clone()
                .flat_map(|y| xs.clone().map(move |x| (x, y)))
                .filter(|&(x, y)| !masked(x, y) && buffer[y * w + x] != bgp)
                .count()
        };
        let left = count(col_x0..art_left);
        let right = count(art_right..col_x0 + max_cols * font.cell_w);
        assert!(left > 0, "top-left star paints no pixel arms");
        assert!(right > 0, "lower-right star paints no pixel arms");
    }

    #[test]
    fn emoji_cells_paint_non_background_pixels() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        if font.emoji_fonts.is_empty() {
            return;
        }
        let mut screen = Screen::new(4, 2, 10);
        screen.put_char('📁');
        let w = 4 * font.cell_w;
        let h = 2 * font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen(&screen, &font, false, 0, None, &mut buf, w);
        let bg = pack_rgb(DEFAULT_BG);
        let ink = buf.iter().filter(|&&p| p != bg && p != 0).count();
        assert!(
            ink > 10,
            "expected color-emoji ink for 📁, got {ink} non-bg px"
        );
    }

    #[test]
    fn cjk_paints_when_system_face_covers_it() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        match font.paint_char('中') {
            GlyphPaint::Empty => {
                if crate::system_fonts::covering_face('中').is_none() {
                    return;
                }
                panic!("system CJK face found but 中 rasterized empty");
            }
            GlyphPaint::Coverage(g) => {
                assert!(g.bitmap.iter().any(|&b| b > 0), "CJK coverage ink");
            }
            GlyphPaint::Color(g) => {
                assert!(g.rgba.chunks_exact(4).any(|p| p[3] > 0), "CJK color alpha");
            }
        }
    }

    #[test]
    fn dual_presentation_stays_color_emoji_not_system_outline() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        if font.emoji_fonts.is_empty() {
            return;
        }
        // SMP hearts are emoji-default and almost never in the bundled
        // mono/symbol chain. Before the #216 reorder they could still
        // lose to a system text face that cmap-covers them.
        match font.paint_char('\u{1F49C}') {
            GlyphPaint::Color(g) => {
                assert!(
                    g.rgba.chunks_exact(4).any(|p| p[3] > 0),
                    "purple heart color alpha"
                );
            }
            GlyphPaint::Coverage(_) => {
                panic!("U+1F49C must stay color emoji, not a system outline fallback")
            }
            GlyphPaint::Empty => {}
        }
    }

    #[test]
    fn flag_cluster_paints_one_glyph_when_emoji_font_present() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        if font.emoji_fonts.is_empty() {
            return;
        }
        let mut screen = Screen::new(6, 2, 10);
        screen.put_char('\u{1F1FA}'); // U
        screen.put_char('\u{1F1F8}'); // S
        let row = screen.row(0).expect("row");
        assert!(!row[0].wide_cont);
        assert!(row[1].wide_cont, "US flag occupies two cells");
        let w = 6 * font.cell_w;
        let h = 2 * font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen(&screen, &font, false, 0, None, &mut buf, w);
        let bg = pack_rgb(DEFAULT_BG);
        let ink = buf.iter().filter(|&&p| p != bg && p != 0).count();
        assert!(
            ink > 10,
            "expected flag-emoji ink for 🇺🇸, got {ink} non-bg px"
        );
        let mut left = 0usize;
        let mut right = 0usize;
        for y in 0..font.cell_h {
            for x in 0..font.cell_w {
                if buf[y * w + x] != bg && buf[y * w + x] != 0 {
                    left += 1;
                }
            }
            for x in font.cell_w..(font.cell_w * 2) {
                if buf[y * w + x] != bg && buf[y * w + x] != 0 {
                    right += 1;
                }
            }
        }
        assert!(
            left > 0 && right > 0,
            "flag clipped in half: left={left} right={right} cell_w={}",
            font.cell_w
        );
    }

    #[test]
    fn selection_inverse_is_a_paint_overlay_only() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let screen = Screen::new(2, 1, 10);
        let before = screen.row(0).expect("row")[0].style;
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 0,
        };
        let w = 2 * font.cell_w;
        let h = font.cell_h;
        let mut buf = vec![0u32; w * h];
        rasterize_screen(&screen, &font, false, 0, Some(range), &mut buf, w);

        assert_eq!(screen.row(0).expect("row")[0].style, before);
        assert_eq!(buf[0], pack_rgb(DEFAULT_FG));
        assert_eq!(buf[font.cell_w], pack_rgb(DEFAULT_BG));
    }

    #[test]
    fn phase2a_offscreen_pane_offset_does_not_clear_neighbor() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut screen = Screen::new(2, 1, 0);
        screen.put_char('X');
        let width = 6 * font.cell_w;
        let height = font.cell_h;
        let sentinel = 0x0011_2233;
        let mut buffer = vec![sentinel; width * height];
        rasterize_screen_at(
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            2 * font.cell_w,
            0,
            2 * font.cell_w,
            font.cell_h,
        );

        let mut pane_changed = false;
        for y in 0..height {
            for x in 0..width {
                let pixel = buffer[y * width + x];
                if (2 * font.cell_w..4 * font.cell_w).contains(&x) {
                    pane_changed |= pixel != sentinel;
                } else {
                    assert_eq!(pixel, sentinel, "neighbor changed at ({x},{y})");
                }
            }
        }
        assert!(
            pane_changed,
            "pane paint should change its own pixel region"
        );
    }

    #[test]
    fn wide_guest_grid_does_not_paint_past_pane_clip() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut screen = Screen::new(4, 1, 0);
        for ch in ['A', 'B', 'C', 'D'] {
            screen.put_char(ch);
        }
        let width = 6 * font.cell_w;
        let height = font.cell_h;
        let sentinel = 0x00aa_bbcc;
        let mut buffer = vec![sentinel; width * height];
        let clip_w = 2 * font.cell_w;
        rasterize_screen_at(
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            clip_w,
            height,
        );
        for (x, pixel) in buffer.iter().enumerate().take(width).skip(clip_w) {
            assert_eq!(
                *pixel, sentinel,
                "column {x} past clip_w={clip_w} must stay empty"
            );
        }
        assert_ne!(buffer[0], sentinel, "visible cells must paint");
    }

    #[test]
    fn footer_and_focus_ring_and_unseen_badge_have_distinct_shapes() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 24 * font.cell_w;
        let height = 5 * font.cell_h;
        let sentinel = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![sentinel; width * height];

        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        // Full-window panes: chrome from y=0; footer overlay on bottom row.
        rasterize_pane_chrome(
            &mut buffer,
            width,
            4,
            0,
            width - 8,
            height,
            true,
            true,
            Some(0.25),
            None,
            true,
            focus,
        );
        rasterize_footer(
            &font,
            "PRISM | C-S-W close",
            &mut buffer,
            width,
            height,
            1,
            focus,
            OPAQUE_ALPHA,
        );

        // Thin (1px) focus ring in brand blue.
        assert_eq!(buffer[4], pack_rgb(focus));
        assert_ne!(buffer[width + 5], pack_rgb(focus)); // not thick double ring
        let footer_y = height - font.cell_h;
        // Footer bar background matches focus color.
        assert_eq!(buffer[footer_y * width + 2], pack_rgb(focus));
        // Right-end spectrum swatches (last color = ink).
        let last = FOCUS_BORDER_PALETTE.len() - 1;
        let swatch_x = width - FOCUS_SWATCH_PX;
        assert_eq!(
            buffer[footer_y * width + swatch_x],
            pack_rgb(FOCUS_BORDER_PALETTE[last].1)
        );
        // The live-activity dot owns the corner; the latched "!" badge sits
        // to its left.
        let badge_x = 4 + (width - 8) - 20;
        assert_eq!(buffer[(3) * width + badge_x], pack_rgb(UNSEEN_BADGE));
        assert_eq!(buffer[(5) * width + badge_x + 4], pack_rgb(CHROME_BG));
        let dot_x = 4 + (width - 8) - 8;
        assert_eq!(buffer[(7) * width + dot_x + 2], pack_rgb(ACTIVE_BADGE));
        assert_ne!(pack_rgb(ACTIVE_BADGE), pack_rgb(UNSEEN_BADGE));
        assert_eq!(
            buffer[(7) * width + badge_x + 11],
            sentinel,
            "gap between badge and dot"
        );
    }

    fn palette_test_rows() -> Vec<crate::palette::PaletteRow> {
        vec![
            crate::palette::PaletteRow::plain(
                "split_right".into(),
                "split the focused pane to the right".into(),
                "C-S-\\ · C-S-E".into(),
            ),
            crate::palette::PaletteRow::plain(
                "split_down".into(),
                "split the focused pane downward, and a very long description that keeps going past the chord column".into(),
                "C-S-- · C-S-D".into(),
            ),
        ]
    }

    #[test]
    fn rasterize_palette_paints_selected_row_inverse_and_chips() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let rows = palette_test_rows();
        let sections = [PaletteSection {
            header: "MATCHES",
            subtitle: "split",
            rows: &rows,
        }];
        let chips = ["All", "Panes", "Tabs"];
        let detail = crate::palette::PaletteDetail {
            name: "split_right".into(),
            text: "split the focused pane to the right. New pane inherits the cwd.".into(),
            chords: "C-S-\\ · C-S-E".into(),
            config_key: "split_right".into(),
        };
        let frame = PaletteFrame {
            query: Some("split"),
            chips: Some((&chips, 1)),
            sections: &sections,
            selected: 0,
            scroll: 0,
            detail: Some(&detail),
            footer: "Enter run",
        };
        let width = 80 * font.cell_w;
        let height = 24 * font.cell_h;
        let mut buffer = vec![pack_rgb(theme.default_bg); width * height];
        rasterize_palette(
            &font,
            &frame,
            theme,
            OverlaySurface::default(),
            &mut buffer,
            width,
            height,
            FOCUS_BORDER_PALETTE[0].1,
        );
        // Compact 80×24: today's 76-cell panel and cell-height rows.
        // fixed rows: query, chips, blank, detail 3, footer, padding = 8;
        // list: header + 2 rows = 3 → 11 rows tall.
        let panel_w = 76 * font.cell_w;
        let panel_h = 11 * font.cell_h;
        let panel_x = (width - panel_w) / 2;
        let panel_y = (height - panel_h) / 2;
        let pad = font.cell_h / 2;
        let selected_y = panel_y + pad + 3 * font.cell_h;
        let normal_y = panel_y + pad + 4 * font.cell_h;
        let selected_bg = theme.selection_bg.unwrap_or(theme.default_fg);
        assert_eq!(
            buffer[selected_y * width + panel_x + 2],
            pack_rgb(selected_bg),
            "selected row is inverse"
        );
        assert_eq!(
            buffer[normal_y * width + panel_x + 2],
            pack_rgb(theme.chrome_bg),
            "unselected row keeps the panel ground"
        );
        // The selected chip (index 1, "Panes") is filled with chrome_fg.
        let chip_y = panel_y + pad + font.cell_h + font.cell_h / 2;
        let all_chip_x = panel_x + 2 * font.cell_w;
        let panes_chip_x = all_chip_x + (3 + 2 + 1) * font.cell_w + font.cell_w / 2;
        assert_eq!(
            buffer[chip_y * width + panes_chip_x],
            pack_rgb(theme.chrome_fg),
            "selected chip is inverse"
        );
        // The detail box is overlay_bg.
        let detail_y = panel_y + pad + 7 * font.cell_h + font.cell_h / 2;
        assert_eq!(
            buffer[detail_y * width + panel_x + font.cell_w],
            pack_rgb(theme.overlay_bg),
            "detail box ground"
        );
    }

    fn mean_rgb(
        buffer: &[u32],
        stride: usize,
        x0: usize,
        x1: usize,
        y0: usize,
        y1: usize,
    ) -> [u8; 3] {
        let mut sums = [0u64; 3];
        let mut count = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let [r, g, b] = unpack_rgb(buffer[y * stride + x]);
                sums[0] += u64::from(r);
                sums[1] += u64::from(g);
                sums[2] += u64::from(b);
                count += 1;
            }
        }
        assert!(count > 0, "golden region must contain pixels");
        [
            (sums[0] / count) as u8,
            (sums[1] / count) as u8,
            (sums[2] / count) as u8,
        ]
    }

    fn mean_luminance(
        buffer: &[u32],
        stride: usize,
        x0: usize,
        x1: usize,
        y0: usize,
        y1: usize,
    ) -> u64 {
        let mut sum = 0u64;
        let mut count = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let [r, g, b] = unpack_rgb(buffer[y * stride + x]);
                sum += 2126 * u64::from(r) + 7152 * u64::from(g) + 722 * u64::from(b);
                count += 1;
            }
        }
        assert!(count > 0, "luminance region must contain pixels");
        sum / count
    }

    fn assert_frosted_gradient_golden(
        before: &[u32],
        after: &[u32],
        width: usize,
        overlay_rgb: [u8; 3],
    ) {
        assert_ne!(before, after, "translucent overlay must change the frame");
        let height = before.len() / width;
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0;
        let mut max_y = 0;
        for (index, (&old, &new)) in before.iter().zip(after).enumerate() {
            if old == new {
                continue;
            }
            let x = index % width;
            let y = index / width;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + 1);
            max_y = max_y.max(y + 1);
        }
        let x0 = min_x.saturating_add(3);
        let x1 = max_x.saturating_sub(3).min(width);
        let y0 = min_y.saturating_add(3);
        let y1 = max_y.saturating_sub(3).min(height);
        assert!(
            x0 < x1 && y0 + 6 < y1,
            "changed region is too small: {min_x}..{max_x}, {min_y}..{max_y}"
        );

        let split = (y1 - y0) / 3;
        let top = mean_rgb(after, width, x0, x1, y0, y0 + split);
        let bottom = mean_rgb(after, width, x0, x1, y1 - split, y1);
        assert!(top[0] > top[2], "top row must retain red tint: {top:?}");
        assert!(
            bottom[2] > bottom[0],
            "bottom row must retain blue tint: {bottom:?}"
        );

        let actual = mean_luminance(after, width, x0, x1, y0, y1);
        let expected = mean_luminance(
            &before
                .iter()
                .map(|&pixel| blend_pixel(pixel, overlay_rgb, OPAQUE_ALPHA, opacity_to_weight(0.7)))
                .collect::<Vec<_>>(),
            width,
            x0,
            x1,
            y0,
            y1,
        );
        assert!(
            actual >= expected / 2 && actual <= expected.saturating_mul(2),
            "frosted luminance {actual} must stay within the translucent blend band around {expected}"
        );
    }

    fn gradient_backdrop(width: usize, height: usize) -> Vec<u32> {
        let red = [220, 30, 30];
        let blue = [30, 30, 220];
        (0..height)
            .flat_map(|y| {
                let color = if y < height / 2 { red } else { blue };
                std::iter::repeat_n(pack_rgb(color), width)
            })
            .collect()
    }

    fn assert_palette_overlay_golden(font: &FontMetrics, theme: &Theme, frame: &PaletteFrame<'_>) {
        let width = 120 * font.cell_w;
        let height = 50 * font.cell_h;
        let mut frosted = gradient_backdrop(width, height);
        rasterize_palette(
            font,
            frame,
            theme,
            OverlaySurface::from_window(0.7, 8, false),
            &mut frosted,
            width,
            height,
            [240, 100, 20],
        )
        .expect("palette golden layout");
        let before = gradient_backdrop(width, height);
        assert_frosted_gradient_golden(&before, &frosted, width, theme.chrome_bg);

        let mut opaque_default = before.clone();
        rasterize_palette(
            font,
            frame,
            theme,
            OverlaySurface::default(),
            &mut opaque_default,
            width,
            height,
            [240, 100, 20],
        )
        .expect("opaque palette layout");
        let mut opaque_window = before;
        rasterize_palette(
            font,
            frame,
            theme,
            OverlaySurface::from_window(1.0, 0, false),
            &mut opaque_window,
            width,
            height,
            [240, 100, 20],
        )
        .expect("window opaque palette layout");
        assert_eq!(
            opaque_default, opaque_window,
            "opacity 1.0 must use the opaque golden path"
        );
    }

    #[test]
    fn palette_pickers_and_context_menus_keep_gradient_goldens() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let theme = default_theme();
        let palette_rows = vec![
            crate::palette::PaletteRow::plain(
                "split_right".into(),
                "split pane".into(),
                "C-S-\\".into(),
            ),
            crate::palette::PaletteRow::plain(
                "split_down".into(),
                "split pane down".into(),
                "C-S-D".into(),
            ),
        ];
        let palette_sections = [PaletteSection {
            header: "MATCHES",
            subtitle: "split",
            rows: &palette_rows,
        }];
        let palette_frame = PaletteFrame {
            query: Some("split"),
            chips: None,
            sections: &palette_sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter run",
        };
        assert_palette_overlay_golden(&font, theme, &palette_frame);

        let picker_rows = vec![
            crate::palette::PaletteRow::plain("alpha".into(), "2 sessions".into(), String::new()),
            crate::palette::PaletteRow::plain("beta".into(), "1 session".into(), String::new()),
        ];
        let picker_sections = [PaletteSection {
            header: "OPEN SPACE",
            subtitle: "",
            rows: &picker_rows,
        }];
        let picker_frame = PaletteFrame {
            query: None,
            chips: None,
            sections: &picker_sections,
            selected: 1,
            scroll: 0,
            detail: None,
            footer: "Enter open · Esc close",
        };
        assert_palette_overlay_golden(&font, theme, &picker_frame);

        let space_menu_rows = vec![
            crate::palette::PaletteRow::plain(
                "Open (switch)".into(),
                "switch".into(),
                String::new(),
            ),
            crate::palette::PaletteRow::plain(
                "Add to this window".into(),
                "keep panes".into(),
                String::new(),
            ),
            crate::palette::PaletteRow::plain("Delete…".into(), "remove".into(), String::new()),
        ];
        let space_menu_sections = [PaletteSection {
            header: "alpha · 2 sessions · 1 tab · a, b",
            subtitle: "",
            rows: &space_menu_rows,
        }];
        let space_menu_frame = PaletteFrame {
            query: None,
            chips: None,
            sections: &space_menu_sections,
            selected: 2,
            scroll: 0,
            detail: None,
            footer: "Enter choose · Esc close",
        };
        assert_palette_overlay_golden(&font, theme, &space_menu_frame);

        let pane_menu_rows = vec![
            crate::palette::PaletteRow::plain("Split right".into(), "split".into(), String::new()),
            crate::palette::PaletteRow::plain("Zoom".into(), "show pane".into(), String::new()),
            crate::palette::PaletteRow::plain("Close pane".into(), "close".into(), String::new()),
        ];
        let pane_menu_sections = [PaletteSection {
            header: "session · space alpha",
            subtitle: "",
            rows: &pane_menu_rows,
        }];
        let pane_menu_frame = PaletteFrame {
            query: None,
            chips: None,
            sections: &pane_menu_sections,
            selected: 1,
            scroll: 0,
            detail: None,
            footer: "Enter choose · Esc close",
        };
        assert_palette_overlay_golden(&font, theme, &pane_menu_frame);
    }

    #[test]
    fn save_space_modal_paints_each_typed_query_character() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let theme = default_theme();
        let hint = "type a name; Enter writes spaces/NAME.json";
        let rows = [crate::palette::PaletteRow::plain(
            hint.to_string(),
            String::new(),
            String::new(),
        )];
        let sections = [PaletteSection {
            header: "SAVE SPACE",
            subtitle: "save the current arrangement of tabs and panes as a space?",
            rows: &rows,
        }];
        let width = 120 * font.cell_w;
        let height = 40 * font.cell_h;
        let paint = |query: &str| {
            let frame = PaletteFrame {
                query: Some(query),
                chips: None,
                sections: &sections,
                selected: 0,
                scroll: 0,
                detail: None,
                footer: "Enter save · Esc cancel",
            };
            let mut buffer = vec![pack_rgb(theme.default_bg); width * height];
            rasterize_palette(
                &font,
                &frame,
                theme,
                OverlaySurface::default(),
                &mut buffer,
                width,
                height,
                FOCUS_BORDER_PALETTE[0].1,
            )
            .expect("save-space modal fits");
            buffer
        };
        let empty = paint("");
        let typed = paint("wori");
        let longer = paint("woriname");
        assert_ne!(
            empty, typed,
            "typed name must change query-row pixels; a caret-only field is the #364 miss"
        );
        assert_ne!(
            typed, longer,
            "each additional character must repaint the name field"
        );
        assert!(
            empty
                .iter()
                .any(|&pixel| pixel != pack_rgb(theme.default_bg)),
            "empty prompt still paints the modal"
        );
    }

    #[test]
    fn theme_picker_keeps_gradient_golden_and_opaque_path_identical() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let theme = default_theme();
        let rows = [
            ThemePickerRow {
                label: "Ghost",
                branch: false,
            },
            ThemePickerRow {
                label: "Monokai",
                branch: true,
            },
            ThemePickerRow {
                label: "Hive",
                branch: true,
            },
        ];
        let width = 120 * font.cell_w;
        let height = 50 * font.cell_h;
        let before = gradient_backdrop(width, height);
        let mut frosted = before.clone();
        rasterize_theme_picker(
            &font,
            &rows,
            Some(1),
            0,
            theme,
            THEME_PICKER_HINT_ROOT,
            OverlaySurface::from_window(0.7, 8, false),
            &mut frosted,
            width,
            height,
            [240, 100, 20],
        );
        assert_frosted_gradient_golden(&before, &frosted, width, theme.chrome_bg);

        let mut opaque_default = before.clone();
        rasterize_theme_picker(
            &font,
            &rows,
            Some(1),
            0,
            theme,
            THEME_PICKER_HINT_ROOT,
            OverlaySurface::default(),
            &mut opaque_default,
            width,
            height,
            [240, 100, 20],
        );
        let mut opaque_window = before;
        rasterize_theme_picker(
            &font,
            &rows,
            Some(1),
            0,
            theme,
            THEME_PICKER_HINT_ROOT,
            OverlaySurface::from_window(1.0, 0, false),
            &mut opaque_window,
            width,
            height,
            [240, 100, 20],
        );
        assert_eq!(
            opaque_default, opaque_window,
            "theme picker opacity 1.0 must be byte-identical"
        );
    }

    #[test]
    fn palette_description_never_reaches_the_chord_column() {
        for width in 20..=PALETTE_MAX_CELLS {
            for chord in 0..=24 {
                let (name_x, desc_x, desc_max, chord_x) =
                    palette_row_columns(width, chord, PALETTE_NAME_CELLS, 2, 1);
                assert_eq!(name_x + PALETTE_NAME_CELLS + 1, desc_x);
                assert!(
                    desc_x + desc_max < chord_x || desc_max == 0,
                    "width {width} chord {chord}: {desc_x}+{desc_max} vs {chord_x}"
                );
                if chord < width {
                    assert!(chord_x + chord <= width);
                }
                let (name_x, desc_x, desc_max, chord_x) =
                    palette_row_columns(width, chord, PALETTE_NAME_CELLS_FULL, 0, 2);
                assert_eq!(name_x + PALETTE_NAME_CELLS_FULL + 1, desc_x);
                assert!(
                    desc_x + desc_max < chord_x || desc_max == 0,
                    "roomy width {width} chord {chord}: {desc_x}+{desc_max} vs {chord_x}"
                );
            }
        }
        assert_eq!(ellipsized("abcdef", 4), "abc…");
        assert_eq!(ellipsized("abcd", 4), "abcd");
        assert_eq!(ellipsized("abcd", 0), "");
        let wrapped = wrap_lines("one two three four five", 9, 2);
        assert_eq!(wrapped, vec!["one two", "three fo…"]);
        assert_eq!(wrap_lines("short", 9, 2), vec!["short"]);
    }

    #[test]
    fn palette_geom_compact_at_eighty_by_twenty_four() {
        let geom = palette_geom(80, 24, 16);
        assert!(geom.compact);
        assert_eq!(geom.width_cells, 76);
        assert_eq!(geom.name_cells, PALETTE_NAME_CELLS);
        assert_eq!(geom.row_pitch_px, 16);
        assert_eq!(geom.query_h_px, 16);
        assert_eq!(geom.inset_cells, 1);
        assert_eq!(geom.chord_gap_cells, 1);
    }

    #[test]
    fn palette_geom_roomy_width_never_shrinks_below_compact() {
        for cols in [81_usize, 100, 105] {
            let roomy = palette_geom(cols, 30, 16);
            assert!(!roomy.compact, "{cols}x30 must be roomy");
            let compact_width = cols.saturating_sub(4).min(PALETTE_COMPACT_MAX_CELLS);
            assert!(
                roomy.width_cells >= compact_width,
                "{cols} cols: roomy {} < compact {compact_width}",
                roomy.width_cells
            );
        }
    }

    #[test]
    fn palette_scroll_keeps_window_when_selection_stays_visible() {
        let visible = 10;
        let total = 40;
        assert_eq!(palette_scroll_for_selection(5, 5, visible, total), 5);
        assert_eq!(palette_scroll_for_selection(5, 14, visible, total), 5);
        assert_eq!(palette_scroll_for_selection(5, 4, visible, total), 4);
        assert_eq!(palette_scroll_for_selection(5, 15, visible, total), 6);
        assert_eq!(
            palette_scroll_for_selection(5, 0, visible, total),
            0,
            "hovering the first visible-or-above row must not park on the last line"
        );
    }

    #[test]
    fn palette_geom_roomy_at_full_hd_grows_name_column() {
        let geom = palette_geom(240, 67, 16);
        assert!(!geom.compact);
        assert_eq!(geom.width_cells, PALETTE_MAX_CELLS);
        assert_eq!(geom.name_cells, PALETTE_NAME_CELLS_FULL);
        assert_eq!(geom.row_pitch_px, 22);
        assert_eq!(geom.query_h_px, 24);
        assert_eq!(geom.inset_cells, 2);
        assert_eq!(geom.chord_gap_cells, 2);
        let longest = crate::keybind::Action::all()
            .into_iter()
            .map(|action| action.name().chars().count())
            .max()
            .unwrap_or(0);
        assert!(
            longest <= geom.name_cells,
            "catalog id is {longest} cells; name column is {}",
            geom.name_cells
        );
    }

    #[test]
    fn palette_catalog_ids_fit_name_column_at_full_hd() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 1920;
        let height = 1080;
        let geom = palette_geom(width / font.cell_w, height / font.cell_h, font.cell_h);
        assert!(!geom.compact);
        for action in crate::keybind::Action::all() {
            let name = action.name();
            assert_eq!(
                ellipsized(&name, geom.name_cells),
                name,
                "{name} must not truncate at 1920×1080"
            );
        }
    }

    #[test]
    fn palette_fits_and_stays_compact_at_eighty_by_twenty_four() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let rows = palette_test_rows();
        let sections = [PaletteSection {
            header: "MATCHES",
            subtitle: "",
            rows: &rows,
        }];
        let frame = PaletteFrame {
            query: Some(""),
            chips: None,
            sections: &sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter run",
        };
        let width = 80 * font.cell_w;
        let height = 24 * font.cell_h;
        let layout = palette_layout(&font, &frame, width, height).expect("panel fits");
        assert!(layout.geom.compact);
        assert_eq!(layout.geom.width_cells, 76);
        assert_eq!(layout.geom.row_pitch_px, font.cell_h);
        assert!(layout.panel_h <= height);
        assert!(layout.panel_w <= width);
        let mut buffer = vec![pack_rgb(theme.default_bg); width * height];
        rasterize_palette(
            &font,
            &frame,
            theme,
            OverlaySurface::default(),
            &mut buffer,
            width,
            height,
            FOCUS_BORDER_PALETTE[0].1,
        );
        assert!(
            buffer
                .iter()
                .any(|&pixel| pixel != pack_rgb(theme.default_bg)),
            "compact panel must paint"
        );
    }

    #[test]
    fn palette_selected_highlight_taller_than_cell_when_roomy() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let rows = palette_test_rows();
        let sections = [PaletteSection {
            header: "MATCHES",
            subtitle: "",
            rows: &rows,
        }];
        let frame = PaletteFrame {
            query: Some(""),
            chips: None,
            sections: &sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter run",
        };
        let width = 1920;
        let height = 1080;
        let layout = palette_layout(&font, &frame, width, height).expect("roomy panel");
        assert!(!layout.geom.compact);
        assert!(layout.geom.row_pitch_px > font.cell_h);
        let mut buffer = vec![pack_rgb(theme.default_bg); width * height];
        rasterize_palette(
            &font,
            &frame,
            theme,
            OverlaySurface::default(),
            &mut buffer,
            width,
            height,
            FOCUS_BORDER_PALETTE[0].1,
        );
        let selected = layout.rows.iter().find(|row| row.global == 0).expect("row");
        let selected_bg = pack_rgb(theme.selection_bg.unwrap_or(theme.default_fg));
        let sample_x = layout.panel_x + 2;
        let band: usize = (0..layout.geom.row_pitch_px)
            .filter(|dy| {
                let y = selected.y + dy;
                y < height && buffer[y * width + sample_x] == selected_bg
            })
            .count();
        assert!(
            band > font.cell_h,
            "highlight band is {band}px; cell_h is {}",
            font.cell_h
        );
        let hit = palette_hit(&layout, sample_x, selected.y + layout.geom.row_pitch_px - 1);
        assert_eq!(hit, Some(0));
        let miss = palette_hit(&layout, sample_x, selected.y + layout.geom.row_pitch_px);
        assert_ne!(miss, Some(0));
    }

    #[test]
    fn classic_grid_underline_patterns_and_sgr_color() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        let underline = [0xff, 0x20, 0x40];
        let mut screen = Screen::new(1, 1, 0);
        screen.set_style(Style {
            foreground: Color::Rgb { r: 1, g: 2, b: 3 },
            underline: true,
            underline_style: UnderlineStyle::Curly,
            underline_color: Color::Rgb {
                r: underline[0],
                g: underline[1],
                b: underline[2],
            },
            ..Style::default()
        });
        screen.put_char('A');
        let mut buffer = vec![pack_rgb(DEFAULT_BG); cw * ch];
        rasterize_screen(&screen, &font, false, 0, None, &mut buffer, cw);
        let underline_px = pack_rgb(underline);
        assert!((0..cw).any(|x| buffer[(ch - 3) * cw + x] == underline_px));
        assert!((0..cw).any(|x| buffer[(ch - 2) * cw + x] == underline_px));
        assert!((0..cw).any(|x| buffer[(ch - 1) * cw + x] == underline_px));

        for style in [
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            let mut pattern = vec![0u32; cw * ch];
            draw_underline(&mut pattern, cw, 0, 0, cw, ch, ch, style, DEFAULT_FG);
            assert!(pattern.iter().any(|&pixel| pixel == pack_rgb(DEFAULT_FG)));
        }
    }

    #[test]
    fn preedit_underline_and_cursor_range_paint_distinct_pixels() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let width = 2 * font.cell_w;
        let height = font.cell_h;
        let mut buffer = vec![pack_rgb(theme.default_bg); width * height];
        rasterize_preedit_at(
            theme,
            &font,
            "ab",
            Some((0, 1)),
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
        );
        // The first cell is the inverse cursor range; the second stays normal.
        assert_eq!(buffer[font.cell_w / 2], pack_rgb(theme.default_fg));
        assert_eq!(
            buffer[font.cell_w + font.cell_w / 2],
            pack_rgb(theme.default_bg)
        );
        let underline_y = height - 2;
        assert_eq!(
            buffer[underline_y * width + font.cell_w / 2],
            pack_rgb(theme.default_bg),
            "selected preedit keeps an inverse underline"
        );
        assert_eq!(
            buffer[underline_y * width + font.cell_w + font.cell_w / 2],
            pack_rgb(theme.default_fg),
            "unselected preedit stays underlined"
        );
    }

    #[test]
    fn scroll_chip_paints_inverse_at_bottom_right() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 16 * font.cell_w;
        let height = 4 * font.cell_h;
        let bg = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![bg; width * height];
        rasterize_scroll_chip(
            &font,
            " 3/10 ",
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            DEFAULT_FG,
            DEFAULT_BG,
        );
        let y = height - font.cell_h;
        let x = width - font.cell_w; // last cell of the chip
        assert_eq!(
            buffer[y * width + x],
            pack_rgb(DEFAULT_FG),
            "chip background is inverse of the cell default"
        );
        assert_eq!(buffer[0], bg, "top-left stays the pane background");
        // Top of buffer is not a permanent menu chrome strip.
        assert_ne!(buffer[0], pack_rgb(default_theme().pane_backdrop));
    }

    #[test]
    fn bell_toast_paints_top_right_with_fill_and_contrast_ink() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 16 * font.cell_w;
        let height = 4 * font.cell_h;
        let bg = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![bg; width * height];
        let fill = [0x62, 0xa8, 0xff]; // focus-border blue
        rasterize_bell_toast(
            &font,
            " bell ",
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            fill,
            contrast_ink(fill),
        );
        // Top-right cell of the top row carries the fill.
        let x = width - font.cell_w;
        assert_eq!(buffer[x], pack_rgb(fill), "toast fill sits top-right");
        // Bottom-right stays the pane background (that is the scroll chip's).
        let y = height - font.cell_h;
        assert_eq!(buffer[y * width + x], bg, "bottom-right untouched");
    }

    #[test]
    fn walkthrough_caption_paints_inside_the_pane() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let pane_w = 40 * font.cell_w;
        let pane_h = 8 * font.cell_h;
        let view = crate::walkthrough::CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "C-S-\\".into(),
            show_me: true,
            skip: true,
        };
        let band = crate::walkthrough::caption_band(
            (0, 0, pane_w, pane_h),
            font.cell_w,
            font.cell_h,
            4,
            &view,
        )
        .expect("band");
        assert!(band.x + band.w <= pane_w);
        assert!(band.y + band.h <= pane_h);
        let empty = vec![pack_rgb(DEFAULT_BG); pane_w * pane_h];
        let mut buffer = empty.clone();
        rasterize_walkthrough_caption(
            &font,
            &view,
            band,
            &mut buffer,
            pane_w,
            DEFAULT_BG,
            DEFAULT_FG,
        );
        assert_ne!(buffer, empty, "caption changes pane pixels");
        assert_eq!(
            buffer[0], empty[0],
            "top-left of the pane stays empty when the band sits at the bottom"
        );
        let mid = (band.y + band.h / 2) * pane_w + band.x + band.w / 2;
        assert_ne!(buffer[mid], empty[mid], "band interior is painted");
    }

    #[test]
    fn walkthrough_line_two_clip_splits_plus_from_star_and_pins_gt() {
        let Ok(mut font) = FontMetrics::load(14.0) else {
            return;
        };
        font.cell_w = 9;
        font.cell_h = 17;
        let pad_x = 7;
        let pad_y = 5;
        let band_x = 13;
        let band_y = 4;
        let cell_w = font.cell_w;
        let cell_h = font.cell_h;
        let width = 400;
        let height = 120;
        let line2_y = band_y + pad_y + cell_h;
        let start_x = band_x + pad_x;
        let view = crate::walkthrough::CaptionView {
            caption: "Split.".into(),
            line2: "W".repeat(11),
            show_me: false,
            skip: true,
        };
        let last_painted = |clip_x: usize| -> Option<usize> {
            let band = crate::walkthrough::CaptionBand {
                x: band_x,
                y: band_y,
                w: 220,
                h: 2 * cell_h + pad_y * 2,
                dismiss: crate::walkthrough::CaptionRect {
                    x: band_x + 200,
                    y: band_y + pad_y,
                    w: cell_w,
                    h: cell_h,
                },
                show_me: None,
                skip: Some(crate::walkthrough::CaptionRect {
                    x: clip_x,
                    y: line2_y,
                    w: 6 * cell_w,
                    h: cell_h,
                }),
            };
            let empty = vec![pack_rgb(DEFAULT_BG); width * height];
            let mut buffer = empty.clone();
            rasterize_walkthrough_caption(
                &font,
                &view,
                band,
                &mut buffer,
                width,
                DEFAULT_BG,
                DEFAULT_FG,
            );
            let mut last = None;
            for px in start_x..clip_x.min(width) {
                if buffer[line2_y * width + px] != empty[line2_y * width + px] {
                    last = Some(px);
                }
            }
            last
        };
        // start_x = 13+7 = 20, not 13*7. Five glyphs vs four splits the clip.
        assert_eq!(start_x, 20);
        let on_boundary = last_painted(start_x + 5 * cell_w).expect("clip on glyph boundary");
        let past_boundary = last_painted(start_x + 5 * cell_w + 1).expect("clip one pixel past");
        let four = last_painted(start_x + 4 * cell_w).expect("four-glyph clip");
        assert!(on_boundary >= start_x + 4 * cell_w);
        assert!(past_boundary >= on_boundary);
        assert!(four < on_boundary);
    }

    #[test]
    fn walkthrough_line_two_clip_pins_gt_eq_ge_and_one_px_shadow() {
        let Ok(mut font) = FontMetrics::load(14.0) else {
            return;
        };
        font.cell_w = 9;
        font.cell_h = 17;
        let pad_x = 7;
        let pad_y = 5;
        let band_x = 13;
        let band_y = 4;
        let cell_w = font.cell_w;
        let cell_h = font.cell_h;
        let width = 400;
        let height = 120;
        // Production pad_x is font.cell_w. Fixture 7/9/13 keeps + * - distinct
        // (13+7=20, 13*7=91, 13-7=6) so those operators cannot hide.
        let start_x = band_x + cell_w;
        let line2_y = band_y + pad_y + cell_h;
        assert_eq!(pad_x, 7);
        assert_eq!(cell_w, 9);
        assert_eq!(band_x, 13);
        assert_eq!(start_x, 22);
        assert_eq!(line2_y, 26);
        assert_ne!(band_x + pad_x, band_x * pad_x);
        assert_ne!(band_x + pad_x, band_x.saturating_sub(pad_x));
        assert_ne!(line2_y, band_y * pad_y + cell_h);
        assert_ne!(line2_y, band_y + pad_y * cell_h);
        let n = 5;
        let clip_x = start_x + n * cell_w;
        let view = crate::walkthrough::CaptionView {
            caption: "Split.".into(),
            line2: "\u{2588}".repeat(11),
            show_me: false,
            skip: true,
        };
        let band = crate::walkthrough::CaptionBand {
            x: band_x,
            y: band_y,
            w: 220,
            h: 2 * cell_h + pad_y * 2,
            dismiss: crate::walkthrough::CaptionRect {
                x: band_x + 200,
                y: band_y + pad_y,
                w: cell_w,
                h: cell_h,
            },
            show_me: None,
            skip: Some(crate::walkthrough::CaptionRect {
                x: clip_x,
                // Off line two so [skip] ink cannot fill glyph n's cell.
                y: line2_y + 2 * cell_h,
                w: 6 * cell_w,
                h: cell_h,
            }),
        };
        let mut buffer = vec![pack_rgb(DEFAULT_BG); width * height];
        rasterize_walkthrough_caption(
            &font,
            &view,
            band,
            &mut buffer,
            width,
            DEFAULT_BG,
            DEFAULT_FG,
        );
        let fg_px = pack_rgb(DEFAULT_FG);
        let shadow_px = pack_rgb(mix_rgb(DEFAULT_BG, [0, 0, 0], 200));
        let fill_px = pack_argb(crate::walkthrough::CAPTION_ALPHA, DEFAULT_BG);
        let px = |x: usize, y: usize| buffer[y * width + x];
        assert_eq!(
            px(band_x + pad_x, line2_y),
            fill_px,
            "line-two origin is band.x+cell_w, not band.x+7"
        );
        let glyph_prev = start_x + (n - 1) * cell_w;
        let glyph_n = start_x + n * cell_w;
        assert_eq!(clip_x, glyph_n);
        assert_eq!(
            px(glyph_prev, line2_y),
            fg_px,
            "glyph n-1 present when clip is x+n*cell_w (`>=` and `==` drop it)"
        );
        assert_eq!(
            px(glyph_n, line2_y),
            fill_px,
            "glyph n absent when clip is x+n*cell_w"
        );
        assert_ne!(
            px(start_x, line2_y + cell_h),
            shadow_px,
            "first glyph shadow x offset is not 0"
        );
        for i in 0..n {
            let x = start_x + i * cell_w;
            assert_eq!(
                px(x, line2_y),
                fg_px,
                "line-two glyph {i} at ({x},{line2_y})"
            );
            assert_eq!(
                px(x + 1, line2_y + cell_h),
                shadow_px,
                "shadow 1px down-right of glyph {i}"
            );
            assert_ne!(
                px(x + 1, line2_y + cell_h + 1),
                shadow_px,
                "shadow y offset is not 2 for glyph {i}"
            );
        }
        assert_eq!(
            px(glyph_prev + cell_w, line2_y + 1),
            shadow_px,
            "last glyph right fringe is 1px shadow"
        );
        assert_ne!(
            px(glyph_prev + cell_w + 1, line2_y + 1),
            shadow_px,
            "shadow x offset is not 2"
        );
    }

    #[test]
    fn contrast_ink_follows_fill_luma() {
        assert_eq!(contrast_ink([0xff, 0xff, 0xff]), [0x12, 0x12, 0x14]);
        assert_eq!(contrast_ink([0x12, 0x12, 0x14]), CHROME_FG);
    }

    #[test]
    fn find_prompt_paints_inverse_at_bottom_left() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 16 * font.cell_w;
        let height = 4 * font.cell_h;
        let bg = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![bg; width * height];
        rasterize_find_prompt(
            &font,
            " Find: x█ 1/1 ",
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            0,
            DEFAULT_FG,
            DEFAULT_BG,
        );
        let y = height - font.cell_h;
        assert_eq!(
            buffer[y * width],
            pack_rgb(DEFAULT_FG),
            "find prompt background is inverse at bottom-left"
        );
        assert_eq!(buffer[0], bg, "top-left stays the pane background");
    }

    #[test]
    fn scrollbar_live_tail_thumb_is_at_bottom_oldest_at_top() {
        let max = 100usize;
        let bar = scrollbar_layout(0, 0, 80, 200, 0, max, 20).expect("history");
        assert_eq!(bar.thumb_y + bar.thumb_h, bar.track_y + bar.track_h);
        let top = scrollbar_layout(0, 0, 80, 200, max, max, 20).expect("oldest");
        assert_eq!(top.thumb_y, top.track_y);
        assert!(scrollbar_layout(0, 0, 80, 200, 0, 0, 20).is_none());
        let mid = scrollbar_layout(0, 0, 80, 200, 50, max, 20).expect("mid");
        let recovered = scrollbar_scroll_from_thumb_y(mid, mid.thumb_y, max);
        assert!(
            (recovered as i32 - 50).abs() <= 1,
            "round-trip scroll {recovered}"
        );
        assert!(bar.contains(bar.track_x, bar.thumb_y));
        assert!(bar.thumb_contains(bar.thumb_y));
    }

    #[test]
    fn scrollbar_track_does_not_intersect_cell_rect() {
        let cell_w = 10usize;
        let content_w = 8 * cell_w + 3;
        let gutter = SCROLLBAR_WIDTH_PX;
        let cols = (content_w - gutter) / cell_w;
        let cell_right = cols * cell_w;
        let bar = scrollbar_layout(content_w - gutter, 0, gutter, 200, 0, 10, 20).expect("history");
        assert!(
            bar.track_x >= cell_right,
            "bar {} overlaps cells ending at {cell_right}",
            bar.track_x
        );
        assert_eq!(crate::mux::SCROLLBAR_GUTTER_PX, SCROLLBAR_WIDTH_PX);
    }

    fn dump_screen(screen: &Screen) -> Vec<(char, u64)> {
        let mut out = Vec::new();
        for row in 0..screen.rows() {
            for col in 0..screen.columns() {
                let cell = screen.view_cell(0, row, col);
                out.push((cell.character, screen.content_epoch()));
            }
        }
        out
    }

    #[test]
    fn mail_letter_overlay_does_not_mutate_screen() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut screen = Screen::new(12, 4, 20);
        for c in "mail".chars() {
            screen.put_char(c);
        }
        let before = dump_screen(&screen);
        let width = 12 * font.cell_w;
        let height = 4 * font.cell_h;
        let mut buffer = vec![0u32; width * height];
        rasterize_screen_at(
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            usize::MAX,
            usize::MAX,
        );
        rasterize_mail_letter(&mut buffer, width, 0, 0, width, height, true, false);
        assert_eq!(dump_screen(&screen), before);
        let gx = MAIL_RING_INSET + 1;
        let gy = MAIL_RING_INSET;
        assert_eq!(buffer[gy * width + gx], pack_rgb(MAIL_LETTER));
    }

    #[test]
    fn mail_letter_shows_when_focused_and_clears_at_depth_zero() {
        let width = 40usize;
        let height = 24usize;
        let sentinel = pack_rgb(DEFAULT_BG);
        let mut on = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut on,
            width,
            0,
            0,
            width,
            height,
            true,
            false,
            None,
            None,
            true,
            focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
        );
        rasterize_mail_letter(&mut on, width, 0, 0, width, height, true, false);
        let gx = MAIL_RING_INSET + 1;
        let gy = MAIL_RING_INSET;
        assert_eq!(on[gy * width + gx], pack_rgb(MAIL_LETTER));
        let unseen_x = width - 20;
        assert_eq!(
            on[3 * width + unseen_x],
            sentinel,
            "unseen ! stays off when only mail is set"
        );
        // depth 0: no letter
        let mut off = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut off,
            width,
            0,
            0,
            width,
            height,
            true,
            false,
            None,
            None,
            true,
            focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
        );
        rasterize_mail_letter(&mut off, width, 0, 0, width, height, false, false);
        assert_eq!(off[gy * width + gx], sentinel);
    }

    #[test]
    fn mail_letter_is_independent_of_unseen_badge() {
        let width = 48usize;
        let height = 24usize;
        let sentinel = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut buffer,
            width,
            0,
            0,
            width,
            height,
            false,
            true,
            None,
            None,
            true,
            focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
        );
        rasterize_mail_letter(&mut buffer, width, 0, 0, width, height, true, false);
        let letter_x = MAIL_RING_INSET + 1;
        let letter_y = MAIL_RING_INSET;
        let badge_x = width - 20;
        assert_eq!(buffer[letter_y * width + letter_x], pack_rgb(MAIL_LETTER));
        assert_eq!(buffer[3 * width + badge_x], pack_rgb(UNSEEN_BADGE));
        assert!(letter_x + MAIL_GLYPH_W < badge_x);
    }

    #[test]
    fn amber_focus_mail_letter_uses_contrasting_pad() {
        let width = 40usize;
        let height = 24usize;
        let amber = focus_border_rgb(1); // palette index 1 is amber
        assert_eq!(amber, MAIL_LETTER);
        let mut buffer = vec![pack_rgb(amber); width * height];
        rasterize_mail_letter(&mut buffer, width, 0, 0, width, height, true, true);
        let pad = buffer[(MAIL_RING_INSET - 1) * width + MAIL_RING_INSET];
        assert_eq!(pad, pack_rgb(CHROME_BG));
        let ink = buffer[MAIL_RING_INSET * width + MAIL_RING_INSET + 1];
        assert_eq!(ink, pack_rgb(CHROME_FG));
        assert_ne!(ink, pack_rgb(amber));
    }

    #[test]
    fn close_glyph_falls_back_to_ascii_x_without_faces() {
        let mut font = FontMetrics::load_baked(16.0).expect("bundled font");
        font.fonts.clear();
        assert_eq!(font.pick_close_glyph(), 'x');
    }

    #[test]
    fn picked_close_glyph_ink_fits_the_cell() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let Some(right) = font.close_glyph_ink_right(font.close_glyph) else {
            return;
        };
        assert!(
            right <= font.cell_w as i32,
            "picked close U+{:04X} ink_right={right} must fit cell_w={}",
            font.close_glyph as u32,
            font.cell_w
        );
    }

    #[test]
    fn owner_nerd_font_skips_overwide_f467() {
        use std::path::Path;
        let path = Path::new("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf");
        if !path.exists() {
            return;
        }
        let Ok(font) = FontMetrics::load_with(16.0, Some(path), &[]) else {
            return;
        };
        let Some(f467) = font.close_glyph_ink_right('\u{F467}') else {
            return;
        };
        if f467 <= font.cell_w as i32 {
            return;
        }
        assert_ne!(
            font.close_glyph, '\u{F467}',
            "over-wide F467 must be skipped"
        );
        let picked = font.close_glyph_ink_right(font.close_glyph);
        assert!(
            picked.is_some_and(|right| right <= font.cell_w as i32),
            "picked close U+{:04X} must fit the cell",
            font.close_glyph as u32
        );
    }

    #[test]
    fn zoomed_tab_label_carries_the_zoom_mark() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let sentinel = pack_rgb(DEFAULT_BG);
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let render = |zoomed: bool| {
            let tabs = [
                crate::mux::TabInfo {
                    title: "main".into(),
                    selected: true,
                    unseen: false,
                    active: false,
                    attention: false,
                    zoomed,
                    handles: 0,
                    focused_handle: None,
                    handle_titles: Vec::new(),
                    handle_active: Vec::new(),
                    git_label: None,
                    pane_title: None,
                },
                crate::mux::TabInfo {
                    title: "tab".into(),
                    selected: false,
                    unseen: false,
                    active: false,
                    attention: false,
                    zoomed: false,
                    handles: 0,
                    focused_handle: None,
                    handle_titles: Vec::new(),
                    handle_active: Vec::new(),
                    git_label: None,
                    pane_title: None,
                },
            ];
            let mut buffer = vec![sentinel; width * bar_h];
            rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, 0, 0);
            buffer
        };
        let plain = render(false);
        let zoomed = render(true);
        let end_pad = crate::mux::effective_tab_end_pad(0, width);
        let mark_x = end_pad + TAB_LABEL_INSET + font.cell_w * "main ".len();
        let mark_w = font.cell_w * TAB_ZOOM_MARK.len();
        let mark_painted = (0..bar_h).any(|y| {
            (mark_x..mark_x + mark_w).any(|x| zoomed[y * width + x] != plain[y * width + x])
        });
        assert!(
            mark_painted,
            "zoomed tab must paint the {TAB_ZOOM_MARK} mark after its title"
        );
        let title_same = (0..bar_h)
            .all(|y| (end_pad..mark_x).all(|x| zoomed[y * width + x] == plain[y * width + x]));
        assert!(title_same, "the title itself must not move");
    }

    #[test]
    fn zoomed_tab_with_pane_title_paints_zoom_mark() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let sentinel = pack_rgb(DEFAULT_BG);
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let render = |zoomed: bool| {
            let tabs = [crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: Some("build".into()),
            }];
            let mut buffer = vec![sentinel; width * bar_h];
            rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, 0, 0);
            buffer
        };
        let plain = render(false);
        let zoomed = render(true);
        let end_pad = crate::mux::effective_tab_end_pad(0, width);
        let mark_x = end_pad + TAB_LABEL_INSET + font.cell_w * "build ".len();
        let mark_w = font.cell_w * TAB_ZOOM_MARK.len();
        let mark_painted = (0..bar_h).any(|y| {
            (mark_x..mark_x + mark_w).any(|x| zoomed[y * width + x] != plain[y * width + x])
        });
        assert!(
            mark_painted,
            "zoomed one-pane tab must keep {TAB_ZOOM_MARK} after pane_title"
        );
    }

    #[test]
    fn hovering_a_pane_handle_shows_its_title_in_the_title_row() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let tabs = vec![crate::mux::TabInfo {
            title: "main".into(),
            selected: true,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 2,
            focused_handle: Some(0),
            handle_titles: vec!["pane 1".into(), "build server".into()],
            handle_active: vec![false, false],
            git_label: None,
            pane_title: None,
        }];
        let paint = |hover: Option<crate::mux::StripHit>| {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                theme,
                &font,
                &tabs,
                &mut buffer,
                width,
                bar_h,
                [0, 0, 255],
                None,
                0,
                0,
                0,
                false,
                hover,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                None,
            );
            buffer
        };
        let pane = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 2, 2)
            .unwrap()
            .focused_id();
        let plain = paint(None);
        let hovered = paint(Some(crate::mux::StripHit::Pane {
            tab: 0,
            pane,
            handle: 1,
        }));
        let fallback_label = paint(Some(crate::mux::StripHit::Pane {
            tab: 0,
            pane,
            handle: 0,
        }));
        // Title row = rows 0..cell_h; the label region starts after the inset.
        let title_row_differs = |a: &[u32], b: &[u32]| {
            (0..font.cell_h)
                .any(|y| (TAB_LABEL_INSET..width / 2).any(|x| a[y * width + x] != b[y * width + x]))
        };
        assert!(
            title_row_differs(&plain, &hovered),
            "hovering a titled handle must replace the tab title text"
        );
        // Hovering a handle without a title paints its pane-number fallback.
        let fallback_label_differs = (0..font.cell_h).any(|y| {
            (TAB_LABEL_INSET..TAB_LABEL_INSET + font.cell_w * 6)
                .any(|x| plain[y * width + x] != fallback_label[y * width + x])
        });
        assert!(
            fallback_label_differs,
            "untitled handle must paint its pane fallback"
        );

        // A single-pane tab paints its pane title in place of the tab title.
        let paint_solo = |pane_title: Option<&str>| {
            let solo = vec![crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: pane_title.map(str::to_string),
            }];
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                theme,
                &font,
                &solo,
                &mut buffer,
                width,
                bar_h,
                [0, 0, 255],
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                None,
            );
            buffer
        };
        assert!(
            title_row_differs(&paint_solo(None), &paint_solo(Some("build server"))),
            "single-pane tab must paint the pane title"
        );

        let mut focused_tabs = tabs.clone();
        focused_tabs[0].pane_title = Some("focused osc".into());
        let paint_focused = |style: TitleRowStyle<'_>| {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                theme,
                &font,
                &focused_tabs,
                &mut buffer,
                width,
                bar_h,
                [0, 0, 255],
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                style,
                None,
            );
            buffer
        };
        assert!(
            title_row_differs(&paint_focused(TitleRowStyle::default()), &paint(None)),
            "focused mode paints the focused pane OSC title without hover"
        );
        let hover_only = TitleRowStyle {
            mode: crate::config::PaneTitlesMode::Hover,
            notice: None,
        };
        assert!(
            !title_row_differs(&paint_focused(hover_only), &paint(None)),
            "hover mode keeps the tab name until a handle is hovered"
        );
        let notice = TitleRowStyle {
            mode: crate::config::PaneTitlesMode::Focused,
            notice: Some((0, 1, "Waiting for you")),
        };
        let noticed = paint_focused(notice);
        assert!(
            title_row_differs(&noticed, &paint_focused(TitleRowStyle::default())),
            "an unfocused-pane notice replaces the title row"
        );
        let handle_w = crate::mux::pane_handle_w(font.cell_w);
        let handle_y = font.cell_h + 3;
        let noticed_chip = unpack_rgb(noticed[handle_y * width + handle_w + 1]);
        let idle_chip =
            unpack_rgb(paint_focused(TitleRowStyle::default())[handle_y * width + handle_w + 1]);
        assert_ne!(noticed_chip, idle_chip, "the noticed handle must tint");
    }

    #[test]
    fn active_handle_chip_and_tab_badge_breathe() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let tab = |handle_active: Vec<bool>, active: bool| {
            vec![crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active,
                attention: false,
                zoomed: false,
                handles: 2,
                focused_handle: Some(0),
                handle_titles: vec!["one".into(), "two".into()],
                handle_active,
                git_label: None,
                pane_title: None,
            }]
        };
        let paint = |tabs: &[crate::mux::TabInfo], pulse: Option<f32>| {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                theme,
                &font,
                tabs,
                &mut buffer,
                width,
                bar_h,
                [0, 0, 255],
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                pulse,
            );
            buffer
        };
        let idle = tab(vec![false, false], false);
        let working = tab(vec![false, true], false);
        assert_eq!(
            paint(&working, None),
            paint(&idle, None),
            "pulse None must stay byte-identical to a static chip"
        );

        let peak = paint(&working, Some(0.25));
        let trough = paint(&working, Some(0.75));
        let still = paint(&working, None);
        let (x0, _) = crate::mux::tab_slot_bounds(0, 1, width, 0, 0).unwrap();
        let handle_w = crate::mux::pane_handle_w(font.cell_w);
        let handle_y = font.cell_h + 3;
        let idle_px = unpack_rgb(peak[handle_y * width + x0 + 1]);
        let peak_px = unpack_rgb(peak[handle_y * width + x0 + handle_w + 1]);
        let trough_px = unpack_rgb(trough[handle_y * width + x0 + handle_w + 1]);
        let still_px = unpack_rgb(still[handle_y * width + x0 + handle_w + 1]);
        let under = active_chip_bg(theme, [0, 0, 255]);
        let base = pane_chip_fill_rgb(under, false);
        assert_eq!(idle_px, pane_chip_fill_rgb(under, true));
        assert_eq!(still_px, base);
        assert_eq!(peak_px, pulse_mix(base, theme.active_badge, 0.25));
        assert_eq!(trough_px, pulse_mix(base, theme.active_badge, 0.75));
        assert_ne!(peak_px, trough_px);
        assert_ne!(peak_px, still_px);
        assert_ne!(trough_px, still_px);

        let badged = tab(vec![false, false], true);
        let badge_none = paint(&badged, None);
        let badge_peak = paint(&badged, Some(0.25));
        let badge_trough = paint(&badged, Some(0.75));
        let (x0, slot_w) = crate::mux::tab_slot_bounds(0, 1, width, 0, 0).unwrap();
        let close = crate::mux::tab_close_left_with_inset(x0, slot_w, font.cell_w, 0).unwrap();
        let badge_x = close.saturating_sub(2 + crate::mux::TAB_BADGE_SIZE / 2);
        let badge_y = crate::mux::tab_badge_top(font.cell_h.min(bar_h));
        assert_eq!(
            unpack_rgb(badge_none[badge_y * width + badge_x]),
            theme.active_badge
        );
        assert_eq!(
            unpack_rgb(badge_peak[badge_y * width + badge_x]),
            pulse_color(0.25)
        );
        assert_eq!(
            unpack_rgb(badge_trough[badge_y * width + badge_x]),
            pulse_color(0.75)
        );
        assert_ne!(
            unpack_rgb(badge_peak[badge_y * width + badge_x]),
            unpack_rgb(badge_trough[badge_y * width + badge_x])
        );
    }

    #[test]
    fn tab_strip_paints_pane_handles_and_empty_end() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let sentinel = pack_rgb(DEFAULT_BG);
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [crate::mux::TabInfo {
            title: "main".into(),
            selected: true,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 2,
            focused_handle: None,
            handle_titles: Vec::new(),
            handle_active: Vec::new(),
            git_label: None,
            pane_title: None,
        }];
        let mut buffer = vec![sentinel; width * bar_h];
        rasterize_tab_strip_with_theme(
            default_theme(),
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            focus,
            None,
            0,
            0,
            0,
            true,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );
        let handle = buffer[(font.cell_h + 3) * width + 1];
        assert_ne!(handle, sentinel, "pane handle must paint ink");
        let end_x = width - font.cell_w;
        let end = buffer[3 * width + end_x];
        assert_ne!(end, sentinel, "empty-end drop target must paint ink");
        let inner = crate::mux::tab_slot_bounds(0, 1, width - font.cell_w, 0, 0).unwrap();
        assert_eq!(inner.0 + inner.1, width - font.cell_w);
    }

    #[test]
    fn active_tab_chip_fill_differs_from_chrome_bg_on_every_theme() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let tabs = [
            crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "other".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        for theme in crate::theme::builtins() {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                theme,
                &font,
                &tabs,
                &mut buffer,
                width,
                bar_h,
                [0, 0, 255],
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                None,
            );
            let (x0, _w0) = crate::mux::tab_slot_bounds(0, 2, width, 0, 0).unwrap();
            let (x1, _w1) = crate::mux::tab_slot_bounds(1, 2, width, 0, 0).unwrap();
            let y = 0;
            let active = unpack_rgb(buffer[y * width + x0 + 1]);
            let inactive = unpack_rgb(buffer[y * width + x1 + 1]);
            assert_eq!(
                active,
                active_tab_row_rgb(active_chip_bg(theme, [0, 0, 255]), y, bar_h),
                "theme {} active fill",
                theme.id
            );
            assert_eq!(
                inactive, theme.chrome_bg,
                "theme {} inactive fill",
                theme.id
            );
            let delta: u32 = (0..3)
                .map(|i| u32::from(active[i].abs_diff(theme.chrome_bg[i])))
                .sum();
            assert!(
                delta >= 40,
                "theme {} active chip must read as selected (delta {delta})",
                theme.id
            );
        }
    }

    #[test]
    fn hovered_active_tab_composes_a_bounded_delta_over_active_fill() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let pane = crate::mux::MuxRuntime::spawn("/bin/sh", &[], 2, 2)
            .unwrap()
            .focused_id();
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let tabs = [crate::mux::TabInfo {
            title: "main".into(),
            selected: true,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 2,
            focused_handle: Some(0),
            handle_titles: Vec::new(),
            handle_active: Vec::new(),
            git_label: None,
            pane_title: None,
        }];
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip_with_theme(
            theme,
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0xff, 0xb4, 0x54],
            None,
            0,
            0,
            0,
            false,
            Some(crate::mux::StripHit::Tab {
                index: 0,
                close: false,
            }),
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );
        let (x0, _) = crate::mux::tab_slot_bounds(0, 1, width, 0, 0).unwrap();
        let hovered = unpack_rgb(buffer[x0 + font.cell_w * 3]);
        let expected = crate::theme::hover_rgb(
            theme.variant,
            active_tab_row_rgb(active_chip_bg(theme, [0xff, 0xb4, 0x54]), 0, bar_h),
            theme.chrome_fg,
            crate::config::DEFAULT_HOVER_BLEND,
        );
        assert_eq!(
            hovered, expected,
            "hover composes over the active chip colour"
        );
        let max_delta = (0..3)
            .map(|i| hovered[i].abs_diff(active_chip_bg(theme, [0xff, 0xb4, 0x54])[i]))
            .max()
            .unwrap_or(0);
        assert!((4..=30).contains(&max_delta));

        let handle_y = font.cell_h + 3;
        let focused = unpack_rgb(buffer[handle_y * width + x0 + 1]);
        let other =
            unpack_rgb(buffer[handle_y * width + x0 + crate::mux::pane_handle_w(font.cell_w) + 1]);
        let under = active_chip_bg(theme, [0xff, 0xb4, 0x54]);
        assert_eq!(focused, pane_chip_fill_rgb(under, true));
        assert_eq!(other, pane_chip_fill_rgb(under, false));
        assert_eq!(
            unpack_rgb(buffer[(handle_y - 1) * width + x0 + 1]),
            pane_chip_outline_rgb(under),
            "chip outline is white at half alpha"
        );

        rasterize_tab_strip_with_theme(
            theme,
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0xff, 0xb4, 0x54],
            None,
            0,
            0,
            0,
            false,
            Some(crate::mux::StripHit::Pane {
                tab: 0,
                pane,
                handle: 0,
            }),
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );
        let focused_hover = unpack_rgb(buffer[handle_y * width + x0 + 1]);
        assert_eq!(
            focused_hover,
            crate::theme::hover_rgb(
                theme.variant,
                pane_chip_fill_rgb(under, true),
                theme.chrome_fg,
                crate::config::DEFAULT_HOVER_BLEND,
            ),
            "focused handle keeps its fill under hover"
        );
    }

    #[test]
    fn tab_active_bg_theme_override_wins() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut theme = default_theme().clone();
        theme.tab_active_bg = [0x11, 0x22, 0x33];
        theme.tab_active_bg_explicit = true;
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let tabs = [crate::mux::TabInfo {
            title: "main".into(),
            selected: true,
            unseen: false,
            active: false,
            attention: false,
            zoomed: false,
            handles: 0,
            focused_handle: None,
            handle_titles: Vec::new(),
            handle_active: Vec::new(),
            git_label: None,
            pane_title: None,
        }];
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip_with_theme(
            &theme,
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0, 0, 255],
            None,
            0,
            0,
            0,
            false,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );
        let (x0, _) = crate::mux::tab_slot_bounds(0, 1, width, 0, 0).unwrap();
        assert_eq!(
            unpack_rgb(buffer[x0 + 1]),
            active_tab_row_rgb([0x11, 0x22, 0x33], 0, bar_h)
        );
    }

    #[test]
    fn active_tab_gradient_runs_from_chip_colour_to_depth() {
        let base = [200, 100, 50];
        let depth = 128;
        assert_eq!(active_tab_gradient_rgb(base, depth, 39, 40), base);
        assert_eq!(active_tab_gradient_rgb(base, depth, 0, 40), [227, 177, 152]);
        assert_eq!(
            active_tab_gradient_rgb(base, ACTIVE_TAB_GRADIENT_DEPTH, 0, 40),
            [201, 104, 56],
            "the shipped depth is three percent of the way to white"
        );
        assert_eq!(
            active_tab_gradient_rgb(base, depth, 0, 1),
            base,
            "one-row bar keeps the chip colour"
        );
        let mid = active_tab_gradient_rgb(base, depth, 20, 41);
        assert_eq!(mid, [213, 138, 101]);
    }

    #[test]
    fn active_tab_gradient_depth_keeps_the_title_ink_at_wcag_aa() {
        // Light chip, dark ink: lightening only helps, full depth.
        let light = [0xd8, 0xe4, 0xf8];
        assert_ne!(contrast_ink(light), CHROME_FG);
        assert_eq!(active_tab_gradient_depth(light), ACTIVE_TAB_GRADIENT_DEPTH);
        // Dark chip, light ink: the far row is clamped where AA would break.
        let dark = active_chip_bg(default_theme(), [0, 0, 255]);
        assert_eq!(contrast_ink(dark), CHROME_FG);
        let depth = active_tab_gradient_depth(dark);
        let far = active_tab_gradient_rgb(dark, depth, 0, 2);
        assert!(contrast_ratio(contrast_ink(dark), far) >= 4.5);
        if depth < ACTIVE_TAB_GRADIENT_DEPTH {
            let one_deeper = mix_rgb(dark, ACTIVE_TAB_GRADIENT_TOWARD, depth + 1);
            assert!(
                contrast_ratio(contrast_ink(dark), one_deeper) < 4.5,
                "depth is the largest AA-safe stop"
            );
        }
        // The darkest grey whose light ink is barely AA cannot fade the full
        // way; the clamp stops short.
        let edge = (0..=255u8)
            .rev()
            .map(|g| [g, g, g])
            .find(|grey| {
                contrast_ink(*grey) == CHROME_FG
                    && contrast_ratio(contrast_ink(*grey), *grey) >= 4.5
            })
            .expect("some grey is barely AA with light ink");
        assert!(active_tab_gradient_depth(edge) < ACTIVE_TAB_GRADIENT_DEPTH);
        // Every built-in theme and focus colour: the far (top) row, where
        // the title glyphs sit, keeps the chosen ink at AA.
        for theme in crate::theme::builtins() {
            for index in 0..FOCUS_BORDER_PALETTE.len() {
                let bg = active_chip_bg(theme, focus_border_rgb(index));
                let depth = active_tab_gradient_depth(bg);
                let last = active_tab_gradient_rgb(bg, depth, 0, 2);
                let ratio = contrast_ratio(contrast_ink(bg), last);
                assert!(
                    ratio >= 4.5,
                    "theme {} focus {index}: ink on the far row {last:?} ratio {ratio:.2} < 4.5",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn active_tab_fades_down_the_bar_and_inactive_tab_stays_flat() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let theme = default_theme();
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h * 2;
        let tabs = vec![
            crate::mux::TabInfo {
                title: "a".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "b".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip_with_theme(
            theme,
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            [0, 0, 255],
            None,
            0,
            0,
            0,
            false,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
            TitleRowStyle::default(),
            None,
        );
        let active = active_chip_bg(theme, [0, 0, 255]);
        let (x0, _) = crate::mux::tab_slot_bounds(0, 2, width, 0, 0).unwrap();
        let (x1, _) = crate::mux::tab_slot_bounds(1, 2, width, 0, 0).unwrap();
        // Sample a column right of the title glyph and left of the close.
        let x = x0 + font.cell_w * 4;
        let depth = active_tab_gradient_depth(active);
        assert!(depth > 0, "the default chip can lighten at least a step");
        assert_eq!(
            unpack_rgb(buffer[x]),
            mix_rgb(active, ACTIVE_TAB_GRADIENT_TOWARD, depth),
            "top row is the lightened chip colour"
        );
        let low = bar_h - 1 - TAB_MARKER_H;
        assert_eq!(
            unpack_rgb(buffer[low * width + x]),
            active_tab_gradient_rgb(active, depth, low, bar_h),
            "row above the marker is nearly the chip colour"
        );
        let mut prev = unpack_rgb(buffer[x]);
        for y in 1..=low {
            let row = unpack_rgb(buffer[y * width + x]);
            assert!(
                (0..3).all(|i| row[i] <= prev[i]),
                "row {y} must not lighten over row {}",
                y - 1
            );
            prev = row;
        }
        let xi = x1 + font.cell_w * 4;
        assert_eq!(unpack_rgb(buffer[xi]), theme.chrome_bg);
        assert_eq!(unpack_rgb(buffer[low * width + xi]), theme.chrome_bg);
    }

    #[test]
    fn tab_strip_handles_do_not_overwrite_title_glyphs() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let chrome = pack_rgb(default_theme().chrome_bg);
        for (title, handles) in [("T", 2usize), ("TAB1", 2), ("TAB1TOOLONGX", 3)] {
            let width = 40 * font.cell_w;
            let bar_h = font.cell_h * 2;
            let tabs = [crate::mux::TabInfo {
                title: title.into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            }];
            let mut titled = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                default_theme(),
                &font,
                &[crate::mux::TabInfo {
                    handles: 0,
                    focused_handle: None,
                    handle_titles: Vec::new(),
                    handle_active: Vec::new(),
                    git_label: None,
                    pane_title: None,
                    ..tabs[0].clone()
                }],
                &mut titled,
                width,
                bar_h,
                focus,
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                None,
            );
            let mut both = vec![0u32; width * bar_h];
            rasterize_tab_strip_with_theme(
                default_theme(),
                &font,
                &tabs,
                &mut both,
                width,
                bar_h,
                focus,
                None,
                0,
                0,
                0,
                false,
                None,
                crate::config::DEFAULT_HOVER_BLEND,
                OPAQUE_ALPHA,
                TitleRowStyle::default(),
                None,
            );
            let title_h = font.cell_h;
            for y in 0..title_h {
                for x in 0..width {
                    let i = y * width + x;
                    if titled[i] != chrome {
                        assert_eq!(
                            both[i], titled[i],
                            "handle overwrote title {title:?} at ({x},{y})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tab_strip_present_only_with_two_tabs_and_marks_active_by_shape_and_color() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let sentinel = pack_rgb(DEFAULT_BG);
        let mut one = vec![sentinel; width * bar_h];
        rasterize_tab_strip(
            &font,
            &[crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            }],
            &mut one,
            width,
            0,
            focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
            None,
            0,
            0,
        );
        assert!(
            one.iter().all(|pixel| *pixel == sentinel),
            "bar_h 0 must leave the buffer unchanged"
        );

        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [
            crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "tab".into(),
                selected: false,
                unseen: true,
                active: true,
                attention: true,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        let mut buffer = vec![sentinel; width * bar_h];
        rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, 0, 0);

        let marker_y = bar_h.saturating_sub(1);
        let marker = tab_marker_rgb(focus);
        let end_pad = crate::mux::effective_tab_end_pad(0, width);
        let title_x = end_pad + TAB_LABEL_INSET;
        assert_eq!(
            marker,
            mix_rgb(
                active_chip_bg(default_theme(), focus),
                [0xff, 0xff, 0xff],
                128
            ),
            "marker is a white line at half alpha over the active chip"
        );
        assert_eq!(buffer[marker_y * width + end_pad], pack_rgb(marker));
        if bar_h >= 2 {
            assert_ne!(
                buffer[(bar_h - 2) * width + end_pad],
                pack_rgb(marker),
                "active-tab marker must be exactly 1px tall"
            );
        }
        let inactive_x = width / 2 + 4;
        assert_ne!(buffer[marker_y * width + inactive_x], pack_rgb(marker));
        let title_band = bar_h.saturating_sub(1);
        // The active chip is chrome_bg lifted 10 % (PT-143) and its title
        // paints in the contrast ink for that chip.
        let active_ink = contrast_ink(active_chip_bg(default_theme(), focus));
        let title_has_ink = (0..title_band).any(|y| {
            (title_x..title_x + font.cell_w).any(|x| buffer[y * width + x] == pack_rgb(active_ink))
        });
        assert!(
            title_has_ink,
            "active tab title must paint in the chip's contrast ink"
        );
        let inactive_title_has_focus = (0..title_band).any(|y| {
            (inactive_x..inactive_x + font.cell_w).any(|x| buffer[y * width + x] == pack_rgb(focus))
        });
        assert!(
            !inactive_title_has_focus,
            "inactive tab title must not use focus color"
        );
        let (x1, w1) = crate::mux::tab_slot_bounds(1, 2, width, 0, 0).unwrap();
        let close1 = crate::mux::tab_close_left(x1, w1, font.cell_w).unwrap();
        let active_x = close1.saturating_sub(8);
        let unseen_x = close1.saturating_sub(16);
        let attention_x = close1.saturating_sub(24);
        let badge_y = crate::mux::tab_badge_top(bar_h);
        assert_eq!(buffer[badge_y * width + active_x], pack_rgb(ACTIVE_BADGE));
        assert_eq!(buffer[badge_y * width + unseen_x], pack_rgb(UNSEEN_BADGE));
        assert_eq!(
            buffer[badge_y * width + attention_x],
            pack_rgb(ATTENTION_BADGE)
        );
        assert_ne!(active_x, unseen_x);
        assert_ne!(unseen_x, attention_x);
        assert_eq!(
            badge_y + crate::mux::TAB_BADGE_SIZE / 2,
            crate::mux::tab_chrome_center_y(bar_h)
        );
        let mut editing = vec![sentinel; width * bar_h];
        rasterize_tab_strip(
            &font,
            &tabs,
            &mut editing,
            width,
            bar_h,
            focus,
            Some((1, "ren", false)),
            0,
            0,
        );
        let edit_x = width / 2 + 4;
        assert_eq!(
            editing[4 * width + edit_x],
            pack_rgb(focus),
            "editing slot must fill with focus color (shape+color)"
        );
        assert_ne!(editing[4 * width + end_pad], pack_rgb(focus));
        assert_eq!(buffer[badge_y * width + active_x], pack_rgb(ACTIVE_BADGE));
    }

    #[test]
    fn tab_strip_respects_window_padding_on_first_title() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [
            crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "tab".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        for pad in [0usize, 5, 40] {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, pad, 0);
            let end_pad = crate::mux::effective_tab_end_pad(pad, width);
            let expected_x = end_pad + TAB_LABEL_INSET;
            assert_eq!(end_pad, pad, "rail end pad must equal window_pad={pad}");
            if end_pad > 0 {
                assert_eq!(
                    buffer[2 * width + end_pad - 1],
                    pack_rgb(default_theme().pane_backdrop),
                    "gutter before end_pad={end_pad} stays strip background"
                );
            }
            let active_ink = contrast_ink(active_chip_bg(default_theme(), focus));
            let painted = (0..bar_h.saturating_sub(1)).any(|y| {
                (expected_x..expected_x + font.cell_w)
                    .any(|x| buffer[y * width + x] == pack_rgb(active_ink))
            });
            assert!(
                painted,
                "first title glyph should start at pad+inset ({expected_x}) for pad={pad}"
            );
        }
    }

    #[test]
    fn tab_strip_gap_pixels_stay_chrome() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let pad = 5;
        let gap = 6;
        let tabs = [
            crate::mux::TabInfo {
                title: "main".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "tab".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip(
            &font,
            &tabs,
            &mut buffer,
            width,
            bar_h,
            focus,
            None,
            pad,
            gap,
        );
        let (x0, w0) = crate::mux::tab_slot_bounds(0, 2, width, pad, gap).unwrap();
        let (x1, _) = crate::mux::tab_slot_bounds(1, 2, width, pad, gap).unwrap();
        assert_eq!(x1, x0 + w0 + gap);
        for x in x0 + w0..x1 {
            for y in 0..bar_h {
                assert_eq!(
                    buffer[y * width + x],
                    pack_rgb(default_theme().pane_backdrop),
                    "gap x={x} y={y} must stay chrome"
                );
            }
        }
    }

    fn is_strip_ground(px: u32, focus: [u8; 3]) -> bool {
        let theme = default_theme();
        px == pack_rgb(theme.pane_backdrop)
            || px == pack_rgb(theme.chrome_bg)
            || px == pack_rgb(theme.tab_active_bg)
            || px == pack_rgb(PANE_BORDER)
            || px == pack_rgb(tab_marker_rgb(focus))
            || on_active_gradient(px, active_chip_bg(theme, focus))
    }

    /// What the strip paints on row `y` of an `h`-row active tab with chip
    /// colour `base`, before hover.
    fn active_tab_row_rgb(base: [u8; 3], y: usize, h: usize) -> [u8; 3] {
        active_tab_gradient_rgb(base, active_tab_gradient_depth(base), y, h)
    }

    /// `px` is some row of the active tab's fade.
    fn on_active_gradient(px: u32, top: [u8; 3]) -> bool {
        (0..=ACTIVE_TAB_GRADIENT_DEPTH)
            .any(|w| px == pack_rgb(mix_rgb(top, ACTIVE_TAB_GRADIENT_TOWARD, w)))
    }

    fn slot_has_non_bg_ink(
        buffer: &[u32],
        stride: usize,
        bar_h: usize,
        x0: usize,
        x1: usize,
    ) -> bool {
        let marker = pack_rgb(tab_marker_rgb(focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX)));
        (0..bar_h.saturating_sub(1)).any(|y| {
            (x0..x1).any(|x| {
                let px = buffer[y * stride + x];
                px != pack_rgb(default_theme().pane_backdrop)
                    && px != pack_rgb(PANE_BORDER)
                    && px != marker
            })
        })
    }

    #[test]
    fn last_tab_close_stays_inside_right_pad_at_awkward_width() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w + 7;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [
            crate::mux::TabInfo {
                title: "one".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "two".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "three".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        for pad in [0usize, 5, 40] {
            if pad.saturating_mul(2) >= width {
                continue;
            }
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, pad, 0);
            let n = tabs.len();
            let right = width.saturating_sub(crate::mux::effective_tab_end_pad(pad, width));
            let (x0, slot_w) = crate::mux::tab_slot_bounds(n - 1, n, width, pad, 0).unwrap();
            let slot_end = x0 + slot_w;
            assert!(
                slot_end <= right,
                "last slot must sit inside right pad={pad}: end={slot_end} right={right}"
            );
            let Some(close_left) = crate::mux::tab_close_left(x0, slot_w, font.cell_w) else {
                continue;
            };
            assert!(
                close_left + font.cell_w + crate::mux::TAB_CLOSE_INSET <= slot_end,
                "close cell + inset must fit in last slot pad={pad}"
            );
            assert!(
                slot_has_non_bg_ink(&buffer, width, bar_h, close_left, close_left + font.cell_w),
                "last-tab close glyph must paint at pad={pad}"
            );
            // End pad may receive bearings; the last window pixels must stay chrome.
            for x in width.saturating_sub(2)..width {
                for y in 0..bar_h.saturating_sub(1) {
                    let px = buffer[y * width + x];
                    assert!(
                        is_strip_ground(px, focus),
                        "close must not ride the window edge x={x} pad={pad}"
                    );
                }
            }
        }
    }

    #[test]
    fn nerd_close_ink_stays_inside_slot_at_owner_font() {
        use std::path::Path;
        let path = Path::new("/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf");
        if !path.exists() {
            return;
        }
        let Ok(font) = FontMetrics::load_with(16.0, Some(path), &[]) else {
            return;
        };
        let width = 800;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [
            crate::mux::TabInfo {
                title: "main".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "tab".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, 5, 0);
        let n = tabs.len();
        for i in 0..n {
            let (x0, slot_w) = crate::mux::tab_slot_bounds(i, n, width, 5, 0).unwrap();
            let slot_end = x0 + slot_w;
            let close_left = crate::mux::tab_close_left(x0, slot_w, font.cell_w).unwrap();
            assert!(
                slot_has_non_bg_ink(&buffer, width, bar_h, close_left, close_left + font.cell_w),
                "slot {i} close must paint"
            );
            for x in close_left + font.cell_w..slot_end {
                for y in 0..bar_h.saturating_sub(1) {
                    let px = buffer[y * width + x];
                    assert!(
                        is_strip_ground(px, focus),
                        "close ink still at slot edge x={x} slot={i}"
                    );
                }
            }
        }
        for x in width.saturating_sub(8)..width {
            for y in 0..bar_h.saturating_sub(1) {
                let px = buffer[y * width + x];
                assert!(
                    is_strip_ground(px, focus),
                    "window-edge gutter must stay empty x={x}"
                );
            }
        }
    }

    #[test]
    fn many_tabs_last_close_stays_off_the_glass() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 1001;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs: Vec<_> = (0..9)
            .map(|i| crate::mux::TabInfo {
                title: if i == 0 { "main".into() } else { "tab".into() },
                selected: i == 8,
                unseen: i > 0 && i < 8,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            })
            .collect();
        let mut buffer = vec![0u32; width * bar_h];
        rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, 5, 0);
        let n = tabs.len();
        for i in 0..n {
            let (x0, slot_w) = crate::mux::tab_slot_bounds(i, n, width, 5, 0).unwrap();
            let slot_end = x0 + slot_w;
            let close_left = crate::mux::tab_close_left(x0, slot_w, font.cell_w).unwrap();
            assert!(
                close_left + font.cell_w + crate::mux::TAB_CLOSE_INSET <= slot_end,
                "slot {i} close must keep the per-slot inset"
            );
            assert!(
                slot_has_non_bg_ink(&buffer, width, bar_h, close_left, close_left + font.cell_w),
                "slot {i} close must paint"
            );
            if i + 1 < n {
                for x in slot_end.saturating_sub(crate::mux::TAB_CLOSE_INSET)..slot_end {
                    for y in 0..bar_h.saturating_sub(1) {
                        let px = buffer[y * width + x];
                        assert!(
                            is_strip_ground(px, focus),
                            "interior close bled into divider gutter x={x} slot={i}"
                        );
                    }
                }
            }
        }
        let (x0, slot_w) = crate::mux::tab_slot_bounds(n - 1, n, width, 5, 0).unwrap();
        let close_left = crate::mux::tab_close_left(x0, slot_w, font.cell_w).unwrap();
        let end_pad = crate::mux::effective_tab_end_pad(5, width);
        assert!(
            close_left + font.cell_w <= width - end_pad,
            "close cell must sit inside the end pad"
        );
        assert!(
            slot_has_non_bg_ink(&buffer, width, bar_h, close_left, close_left + font.cell_w),
            "last of many tabs must still paint a close glyph"
        );
        for x in width.saturating_sub(2)..width {
            for y in 0..bar_h.saturating_sub(1) {
                let px = buffer[y * width + x];
                assert!(
                    px == pack_rgb(default_theme().pane_backdrop)
                        || px == pack_rgb(tab_marker_rgb(focus)),
                    "many-tab last close clipped at window x={x}"
                );
            }
        }
    }

    #[test]
    fn close_glyph_pixels_stay_inside_own_slot() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let width = 40 * font.cell_w + 3;
        let bar_h = font.cell_h;
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);
        let tabs = [
            crate::mux::TabInfo {
                title: "left".into(),
                selected: false,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
            crate::mux::TabInfo {
                title: "right".into(),
                selected: true,
                unseen: false,
                active: false,
                attention: false,
                zoomed: false,
                handles: 0,
                focused_handle: None,
                handle_titles: Vec::new(),
                handle_active: Vec::new(),
                git_label: None,
                pane_title: None,
            },
        ];
        for pad in [0usize, 5, 40] {
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip(&font, &tabs, &mut buffer, width, bar_h, focus, None, pad, 0);
            let n = tabs.len();
            for i in 0..n {
                let (x0, slot_w) = crate::mux::tab_slot_bounds(i, n, width, pad, 0).unwrap();
                let slot_end = x0 + slot_w;
                let Some(close_left) = crate::mux::tab_close_left(x0, slot_w, font.cell_w) else {
                    continue;
                };
                assert!(
                    slot_has_non_bg_ink(&buffer, width, bar_h, close_left, slot_end),
                    "slot {i} close must paint pad={pad}"
                );
                let gap_end = slot_end.saturating_add(TAB_LABEL_INSET).min(width);
                for x in slot_end.saturating_add(1)..gap_end {
                    for y in 0..bar_h.saturating_sub(1) {
                        let px = buffer[y * width + x];
                        assert!(
                            is_strip_ground(px, focus),
                            "close ink bled into neighbor gutter x={x} slot={i} pad={pad}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn light_cycle_traces_clockwise_and_leaves_remainder_neutral() {
        let width = 60usize;
        let height = 40usize;
        let sentinel = pack_rgb(DEFAULT_BG);
        let mut buffer = vec![sentinel; width * height];
        let focus = focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX);

        // Quarter sweep on a 40x30 pane at (10,5): perimeter 140, traced 35 —
        // the whole top edge (40) is not yet done, so only 35 top pixels trace.
        rasterize_pane_chrome(
            &mut buffer,
            width,
            10,
            5,
            40,
            30,
            true,
            false,
            None,
            Some(0.25),
            true,
            focus,
        );
        // Early top edge is traced in the focus color…
        assert_eq!(buffer[5 * width + 12], pack_rgb(focus));
        // …the 7x7 vehicle head has a focus-color shell and a bright core…
        assert_eq!(buffer[5 * width + 44], pack_rgb(focus));
        assert_ne!(buffer[7 * width + 44], pack_rgb(focus));
        assert_ne!(buffer[7 * width + 44], pack_rgb(PANE_BORDER));
        // …and the untraced remainder (bottom edge) is neutral, not focus.
        assert_eq!(buffer[34 * width + 30], pack_rgb(PANE_BORDER));
        // Left edge below the start is also untraced.
        assert_eq!(buffer[20 * width + 10], pack_rgb(PANE_BORDER));

        // head=false: same sweep leaves only the 1px trail — no vehicle box.
        let mut headless = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut headless,
            width,
            10,
            5,
            40,
            30,
            true,
            false,
            None,
            Some(0.25),
            false,
            focus,
        );
        assert_eq!(headless[5 * width + 44], pack_rgb(focus));
        assert_eq!(headless[7 * width + 44], sentinel);

        // Mid-sweep 0.6: perimeter 140, traced 84 — top (40) and right (30)
        // fully traced, bottom partially (14 px from the right), left still
        // neutral. Pins the clockwise walk beyond the top edge.
        let mut mid = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut mid,
            width,
            10,
            5,
            40,
            30,
            true,
            false,
            None,
            Some(0.6),
            false,
            focus,
        );
        assert_eq!(mid[5 * width + 49], pack_rgb(focus), "top edge done");
        assert_eq!(mid[20 * width + 49], pack_rgb(focus), "right edge done");
        assert_eq!(
            mid[34 * width + 40],
            pack_rgb(focus),
            "bottom partly traced"
        );
        assert_eq!(
            mid[34 * width + 20],
            pack_rgb(PANE_BORDER),
            "bottom remainder neutral"
        );
        assert_eq!(
            mid[20 * width + 10],
            pack_rgb(PANE_BORDER),
            "left edge untraced"
        );

        // Completed sweep (>=1.0) is byte-identical to the static border.
        let mut swept = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut swept,
            width,
            10,
            5,
            40,
            30,
            true,
            false,
            None,
            Some(1.0),
            true,
            focus,
        );
        let mut fixed = vec![sentinel; width * height];
        rasterize_pane_chrome(
            &mut fixed, width, 10, 5, 40, 30, true, false, None, None, true, focus,
        );
        assert_eq!(swept, fixed);
    }

    #[test]
    fn active_dot_breathes_with_pulse_phase() {
        // Peak phase is the exact badge color; trough is dimmer, and neither
        // matches the latched unseen badge hue.
        assert_eq!(pulse_color(0.25), ACTIVE_BADGE);
        let trough = pulse_color(0.75);
        assert_ne!(trough, ACTIVE_BADGE);
        assert_ne!(trough, UNSEEN_BADGE);
        for (dim, full) in trough.iter().zip(ACTIVE_BADGE.iter()) {
            assert!(dim <= full, "trough must dim toward the background");
        }
        // The trough stays clearly visible (>= 60% brightness), not off.
        assert!(
            trough[1] > DEFAULT_BG[1] + 0x40,
            "green channel must stay lit"
        );
    }

    #[test]
    fn focus_border_names_parse_and_cycle() {
        assert_eq!(parse_focus_border("blue"), Some(DEFAULT_FOCUS_BORDER_INDEX));
        assert_eq!(parse_focus_border("CORAL"), Some(0));
        assert_eq!(parse_focus_border("3"), Some(3));
        assert_eq!(parse_focus_border("nope"), None);
        assert_eq!(cycle_focus_border(FOCUS_BORDER_PALETTE.len() - 1), 0);
        assert_eq!(cycle_focus_border_back(0), FOCUS_BORDER_PALETTE.len() - 1);
        assert_eq!(cycle_focus_border_back(cycle_focus_border(3)), 3);
    }

    #[test]
    fn overlay_paint_clips_to_pane_content_rect() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        // Two side-by-side 4-col panes in one buffer; overlay is 6 cols wide.
        let pane_cols = 4;
        let pane_rows = 2;
        let width = pane_cols * 2 * cw;
        let height = pane_rows * ch;
        let mut buf = vec![0u32; width * height];
        let overlay = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 6,
            text: String::new(),
            runs: Vec::new(),
        };
        let clip_w = pane_cols * cw;
        rasterize_overlays_at(
            &[overlay],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            clip_w,
            height,
            pane_cols,
            pane_rows,
        );
        let overlay_px = pack_rgb(OVERLAY_BG);
        for y in 0..ch {
            for x in 0..clip_w {
                assert_eq!(buf[y * width + x], overlay_px, "inside pane at {x},{y}");
            }
            for x in clip_w..width {
                assert_eq!(buf[y * width + x], 0, "sibling pane at {x},{y}");
            }
        }
    }

    #[test]
    fn overlay_paint_does_not_cover_top_chrome() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        let chrome_h = ch;
        let cols = 4;
        let rows = 2;
        let width = cols * cw;
        let height = chrome_h + rows * ch;
        let mut buf = vec![0u32; width * height];
        let overlay = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 2,
            cols: 4,
            text: String::new(),
            runs: Vec::new(),
        };
        rasterize_overlays_at(
            &[overlay],
            &font,
            &mut buf,
            width,
            0,
            chrome_h,
            0,
            chrome_h,
            width,
            rows * ch,
            cols,
            rows,
        );
        let overlay_px = pack_rgb(OVERLAY_BG);
        for y in 0..chrome_h {
            for x in 0..width {
                assert_eq!(buf[y * width + x], 0, "chrome row {y}");
            }
        }
        assert_eq!(buf[chrome_h * width], overlay_px);
    }

    #[test]
    fn overlay_paint_clips_after_pane_shrink() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        // Region is 8 cols; pane shrank to 3 cols + 2px gap + sibling.
        let pane_cols = 3;
        let gap = 2;
        let width = pane_cols * cw + gap + pane_cols * cw;
        let height = ch;
        let mut buf = vec![0u32; width * height];
        let overlay = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 8,
            text: String::new(),
            runs: Vec::new(),
        };
        let clip_w = pane_cols * cw;
        rasterize_overlays_at(
            &[overlay],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            clip_w,
            height,
            pane_cols,
            1,
        );
        let overlay_px = pack_rgb(OVERLAY_BG);
        for (x, px) in buf.iter().take(clip_w).enumerate() {
            assert_eq!(*px, overlay_px, "inside shrunken pane at {x}");
        }
        for (x, px) in buf.iter().enumerate().take(width).skip(clip_w) {
            assert_eq!(*px, 0, "gap/sibling at {x} must stay empty");
        }
    }

    #[test]
    fn overlay_paint_clips_partially_scrolled_region() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        let width = 4 * cw;
        let height = 2 * ch;
        let mut buf = vec![0u32; width * height];
        let overlay = CellRectOverlay {
            row: -1,
            col: 0,
            rows: 2,
            cols: 4,
            text: String::new(),
            runs: Vec::new(),
        };
        rasterize_overlays_at(
            &[overlay],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            width,
            height,
            4,
            2,
        );
        let overlay_px = pack_rgb(OVERLAY_BG);
        assert!(
            buf.iter().take(width).all(|px| *px == overlay_px),
            "visible remainder of scrolled region"
        );
        assert!(
            buf.iter().skip(ch * width).take(width).all(|px| *px == 0),
            "row below the 1-row remainder must stay empty"
        );
    }

    #[test]
    fn styled_run_overlay_uses_palette_and_underline() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        let width = 4 * cw;
        let height = ch;
        let mut buf = vec![0u32; width * height];
        let overlay = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 2,
            text: String::new(),
            runs: vec![prismattyc_render::OverlayRun {
                text: "AB".into(),
                fg: Some(1),
                bg: Some(4),
                bold: false,
                italic: false,
                underline: true,
                inverse: false,
            }],
        };
        rasterize_overlays_at(
            &[overlay],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            width,
            height,
            4,
            1,
        );
        let expected_bg = pack_rgb(palette_rgb(4));
        assert_eq!(buf[0], expected_bg);
        let uy = ch.saturating_sub(2);
        assert_eq!(buf[uy * width], pack_rgb(palette_rgb(1)));
    }

    #[test]
    fn viewport_overlay_paints_above_cell_rect() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let cw = font.cell_w;
        let ch = font.cell_h;
        let width = cw;
        let height = ch;
        let mut buf = vec![0u32; width * height];
        let cell = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 1,
            text: String::new(),
            runs: vec![prismattyc_render::OverlayRun {
                text: " ".into(),
                fg: None,
                bg: Some(1),
                ..Default::default()
            }],
        };
        let viewport = CellRectOverlay {
            row: 0,
            col: 0,
            rows: 1,
            cols: 1,
            text: String::new(),
            runs: vec![prismattyc_render::OverlayRun {
                text: " ".into(),
                fg: None,
                bg: Some(2),
                ..Default::default()
            }],
        };
        rasterize_overlays_at(
            &[cell],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            width,
            height,
            1,
            1,
        );
        rasterize_overlays_at(
            &[viewport],
            &font,
            &mut buf,
            width,
            0,
            0,
            0,
            0,
            width,
            height,
            1,
            1,
        );
        assert_eq!(buf[0], pack_rgb(palette_rgb(2)));
    }

    #[test]
    fn shift_ink_into_clip_pulls_negative_origin_into_the_box() {
        assert_eq!(shift_ink_into_clip(-3, -4, 8, 10, [0, 0, 12, 14]), (0, 0));
        assert_eq!(shift_ink_into_clip(0, 8, 4, 8, [0, 0, 10, 12]), (0, 4));
    }

    #[test]
    fn mail_letter_glyph_fits_and_pins_to_cell_origin() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        if matches!(font.paint_char(MAIL_LETTER_GLYPH), GlyphPaint::Empty) {
            return;
        }
        let w = font.cell_w;
        let h = font.cell_h;
        let mut buffer = vec![0u32; w * h];
        blit_glyph(&mut buffer, w, &font, MAIL_LETTER_GLYPH, 0, 0, MAIL_LETTER);
        let ink: Vec<(usize, usize)> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer[y * w + x] != 0)
            .collect();
        assert!(!ink.is_empty(), "mail letter must paint ink");
        let min_x = ink.iter().map(|(x, _)| *x).min().unwrap();
        let min_y = ink.iter().map(|(_, y)| *y).min().unwrap();
        let max_x = ink.iter().map(|(x, _)| *x).max().unwrap();
        let max_y = ink.iter().map(|(_, y)| *y).max().unwrap();
        assert!(
            min_x <= 1,
            "letter must sit on the left edge, min_x={min_x}"
        );
        assert!(min_y <= 1, "letter must sit on the top edge, min_y={min_y}");
        assert!(
            max_x < w,
            "letter must not clip right, max_x={max_x} cell_w={w}"
        );
        assert!(
            max_y < h,
            "letter must not clip bottom, max_y={max_y} cell_h={h}"
        );
        assert!(
            max_y + 1 < h,
            "envelope must leave unused cell below, max_y={max_y} cell_h={h}"
        );
    }

    #[test]
    fn media_control_glyphs_paint_as_outline_from_bundled_symbols() {
        // Claude Code 2.1.238 auto-mode footer is U+23F5 (⏵⏵ auto mode on).
        // Pause/stop/record (U+23F8..U+23FA) and skip/play-pause (U+23ED..U+23EF)
        // are the sibling modes. The bundled Nerd and DejaVu faces omit this
        // Miscellaneous Technical block, so without a symbol fallback these
        // fall through to a color-emoji strike (macOS) or nothing (Linux).
        // Bundled Noto Sans Symbols 2 must supply outline Coverage, before
        // color-emoji fonts.
        let font = FontMetrics::load(16.0).expect("bundled font chain must load");
        for ch in [
            '\u{23ED}', '\u{23EE}', '\u{23EF}', '\u{23F4}', '\u{23F5}', '\u{23F6}', '\u{23F7}',
            '\u{23F8}', '\u{23F9}', '\u{23FA}',
        ] {
            assert!(
                matches!(font.paint_char(ch), GlyphPaint::Coverage(_)),
                "U+{:04X} must paint as an outline glyph from the bundled symbol face",
                ch as u32
            );
        }
    }

    #[test]
    fn mail_letter_does_not_fill_full_cell_pad() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        if matches!(font.paint_char(MAIL_LETTER_GLYPH), GlyphPaint::Empty) {
            return;
        }
        let mut screen = Screen::new(8, 2, 20);
        screen.set_style(Style {
            foreground: Color::Rgb {
                r: MAIL_LETTER[0],
                g: MAIL_LETTER[1],
                b: MAIL_LETTER[2],
            },
            background: Color::Rgb {
                r: 0x1b,
                g: 0x1e,
                b: 0x26,
            },
            ..Style::default()
        });
        screen.put_char(MAIL_LETTER_GLYPH);
        let width = 8 * font.cell_w;
        let height = 2 * font.cell_h;
        let mut buffer = vec![0u32; width * height];
        rasterize_screen_at(
            &screen,
            &font,
            false,
            CursorShape::Block,
            0,
            None,
            &mut buffer,
            width,
            0,
            0,
            usize::MAX,
            usize::MAX,
        );
        let pad = pack_rgb([0x1b, 0x1e, 0x26]);
        let default_bg = pack_rgb(DEFAULT_BG);
        let last = font.cell_h.saturating_sub(1);
        assert_eq!(
            buffer[last * width],
            default_bg,
            "bottom of the mail cell must not keep the full-cell pad"
        );
        assert_ne!(buffer[last * width], pad);
        let corner_has_ink = (0..2).any(|y| {
            (0..2).any(|x| buffer[y * width + x] != default_bg && buffer[y * width + x] != 0)
        });
        assert!(corner_has_ink, "envelope ink stays in the top-left corner");
    }

    /// Claude / Nerd title spinners sit above the mono baseline. Fitting
    /// must keep their ink inside the one-cell tab strip so the orb is
    /// not sheared at y=0.
    #[test]
    fn tab_title_spinner_ink_stays_inside_strip() {
        let Ok(font) = FontMetrics::load(16.0) else {
            return;
        };
        let candidates = [
            '\u{F110}', // nf-fa-spinner
            '\u{F013}', // nf-fa-cog
            '\u{25D0}', // ◐
            '\u{2736}', // ✶
            '\u{273B}', // ✻
            '\u{F0544}',
            '🟠',
            '🔄',
        ];
        let mut saw_overflow = false;
        for ch in candidates {
            if matches!(font.paint_char(ch), GlyphPaint::Empty) {
                continue;
            }
            if let GlyphPaint::Coverage(g) = font.paint_char(ch) {
                let oy = font.coverage_origin_y(&g);
                let right = g.xmin + g.width as i32;
                if oy < 0 || right > font.cell_w as i32 + 1 {
                    saw_overflow = true;
                    eprintln!(
                        "U+{:04X} overflows cell: oy={oy} h={} right={right} cell={}x{}",
                        ch as u32, g.height, font.cell_w, font.cell_h
                    );
                }
            }
            let fitted = font.paint_char_fitting(ch, font.cell_w as i32, font.cell_h as i32);
            if let GlyphPaint::Coverage(g) = fitted {
                let oy = font.coverage_origin_y(&g).max(0);
                assert!(
                    oy + g.height as i32 <= font.cell_h as i32 + 1,
                    "U+{:04X} fitted ink still taller than the strip (oy={oy} h={})",
                    ch as u32,
                    g.height
                );
            }

            let width = 20 * font.cell_w;
            let bar_h = font.cell_h;
            let mut buffer = vec![0u32; width * bar_h];
            rasterize_tab_strip(
                &font,
                &[
                    crate::mux::TabInfo {
                        title: format!("{ch} Claude"),
                        selected: true,
                        unseen: false,
                        active: false,
                        attention: false,
                        zoomed: false,
                        handles: 0,
                        focused_handle: None,
                        handle_titles: Vec::new(),
                        handle_active: Vec::new(),
                        git_label: None,
                        pane_title: None,
                    },
                    crate::mux::TabInfo {
                        title: "other".into(),
                        selected: false,
                        unseen: false,
                        active: false,
                        attention: false,
                        zoomed: false,
                        handles: 0,
                        focused_handle: None,
                        handle_titles: Vec::new(),
                        handle_active: Vec::new(),
                        git_label: None,
                        pane_title: None,
                    },
                ],
                &mut buffer,
                width,
                bar_h,
                focus_border_rgb(DEFAULT_FOCUS_BORDER_INDEX),
                None,
                0,
                0,
            );
            let (x0, _) = crate::mux::tab_slot_bounds(0, 2, width, 0, 0).unwrap();
            let cell_x0 = x0 + TAB_LABEL_INSET;
            let cell_x1 = (cell_x0 + font.cell_w).min(width);
            let mut ink_rows = 0usize;
            for y in 0..bar_h {
                if (cell_x0..cell_x1)
                    .any(|x| buffer[y * width + x] != pack_rgb(default_theme().pane_backdrop))
                {
                    ink_rows += 1;
                }
            }
            assert!(
                ink_rows >= 2,
                "U+{:04X} must leave visible ink in the title cell",
                ch as u32
            );
        }
        let _ = saw_overflow;
    }
}

#[cfg(test)]
mod active_chip_tests {
    use super::*;

    #[test]
    fn active_chip_ink_contrasts_on_every_builtin_and_focus_colour() {
        for theme in crate::theme::builtins() {
            for index in 0..FOCUS_BORDER_PALETTE.len() {
                let focus = focus_border_rgb(index);
                let bg = active_chip_bg(theme, focus);
                let ink = contrast_ink(bg);
                let ratio = contrast_ratio(ink, bg);
                assert!(
                    ratio >= 4.5,
                    "theme {} focus {index}: ink {ink:?} on {bg:?} ratio {ratio:.2} < 4.5",
                    theme.id
                );
                if !theme.tab_active_bg_explicit {
                    let lifted = match theme.variant {
                        crate::theme::ThemeVariant::Dark => {
                            relative_luminance(bg) > relative_luminance(theme.chrome_bg)
                        }
                        crate::theme::ThemeVariant::Light => {
                            relative_luminance(bg) < relative_luminance(theme.chrome_bg)
                        }
                    };
                    assert!(
                        lifted,
                        "theme {} focus {index}: {bg:?} does not lift off {:?}",
                        theme.id, theme.chrome_bg
                    );
                }
            }
        }
    }

    #[test]
    fn contrast_ratio_matches_wcag_reference_points() {
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.01);
        assert!((contrast_ratio([0x77, 0x77, 0x77], [255, 255, 255]) - 4.48).abs() < 0.05);
        assert_eq!(contrast_ink([0xff, 0xff, 0xff]), [0x12, 0x12, 0x14]);
        assert_eq!(contrast_ink([0x10, 0x10, 0x10]), CHROME_FG);
    }
}

#[cfg(test)]
mod space_rail_raster_tests {
    use super::*;
    use crate::mux::HostGeom;
    use crate::space_rail::{rail_thickness_px, RailChipView, RailLayout, RailSide};

    #[test]
    fn rail_names_preserve_native_glyph_pixels_and_clip_to_the_text_box() {
        let font = FontMetrics::load(16.0).expect("rail font");
        let text = "astra-spaces · astra-pc";
        let width = 24 * font.cell_w;
        let stride = width + 8;
        let ink = [230, 230, 230];
        let background = [40, 40, 40];
        let sentinel = pack_rgb([1, 2, 3]);
        let mut native = vec![pack_rgb(background); width * font.cell_h];
        for (index, ch) in text.chars().enumerate() {
            blit_glyph_in(
                &mut native,
                width,
                &font,
                ch,
                index * font.cell_w,
                0,
                ink,
                0,
                width,
                false,
            );
        }
        assert!(native.iter().any(|pixel| *pixel != pack_rgb(background)));
        for height in [font.cell_h, font.cell_h.saturating_sub(TAB_MARKER_H)] {
            let mut actual = vec![sentinel; stride * (font.cell_h + 8)];
            rasterize_rail_names(
                &font,
                text,
                &mut actual,
                stride,
                4,
                4,
                width,
                height,
                ink,
                background,
            );
            for y in 0..font.cell_h + 8 {
                for x in 0..stride {
                    let expected = if (4..4 + height).contains(&y) && (4..4 + width).contains(&x) {
                        native[(y - 4) * width + x - 4]
                    } else {
                        sentinel
                    };
                    assert_eq!(actual[y * stride + x], expected, "pixel at {x},{y}");
                }
            }
        }
    }

    fn chip(label: &str, current: bool) -> RailChipView {
        RailChipView {
            label: label.into(),
            pane_names: String::new(),
            current,
            focused: false,
            editing: None,
            confirm: false,
            plus: false,
        }
    }

    fn plus() -> RailChipView {
        RailChipView {
            label: "+".into(),
            pane_names: String::new(),
            current: false,
            focused: false,
            editing: None,
            confirm: false,
            plus: true,
        }
    }

    fn rgb_at(buffer: &[u32], stride: usize, x: usize, y: usize) -> [u8; 3] {
        unpack_rgb(buffer[y * stride + x])
    }

    #[test]
    fn rail_paints_left_aligned_chips_and_marks_the_current_one() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut geom = HostGeom::tight(font.cell_w, font.cell_h);
        geom.window_pad = 4;
        geom.rail_gap = 3;
        geom.rail_side = RailSide::Bottom;
        geom.rail_chip_cols = 8;
        geom.rail_px = rail_thickness_px(RailSide::Bottom, font.cell_w, font.cell_h, 8);
        let (width, height) = (60 * font.cell_w, 10 * font.cell_h);
        let names = vec!["a-very-long-space-name".to_string(), "cur".to_string()];
        let layout = RailLayout::for_window(geom, width, height, &names).unwrap();
        let theme = default_theme();
        let focus = [0xff, 0x80, 0x10];
        let danger = [0xf0, 0x40, 0x40];
        let mut buffer = vec![pack_argb(OPAQUE_ALPHA, theme.default_bg); width * height];
        let views = vec![
            chip("a-very-long-space-name", false),
            chip("cur", true),
            plus(),
        ];
        // Label-sized chips: the long name is capped at 8 cells, "cur" is
        // the 6-cell minimum.
        assert_eq!(layout.chip_px, vec![8 * font.cell_w, 6 * font.cell_w]);
        rasterize_space_rail(
            theme,
            &font,
            &layout,
            &views,
            &mut buffer,
            width,
            focus,
            danger,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
        );
        // The rail ground is the pane backdrop across the whole bottom row.
        assert_eq!(rgb_at(&buffer, width, 0, height - 1), theme.pane_backdrop);
        assert_eq!(
            rgb_at(&buffer, width, width - 1, height - 1),
            theme.pane_backdrop
        );
        // Chip 0 has ink inside its first cells (left-aligned text)...
        let (x0, y0, w0, h0) = layout.chip_bounds(0, 2).unwrap();
        let ink_in = |x_lo: usize, x_hi: usize| {
            (y0..y0 + h0).any(|y| {
                (x_lo..x_hi).any(|x| {
                    let px = rgb_at(&buffer, width, x, y);
                    px != theme.pane_backdrop && px != theme.default_bg
                })
            })
        };
        assert!(ink_in(x0, x0 + 2 * font.cell_w), "label starts at the left");
        // ...and the long name is cut before the close cell (ellipsized).
        let close_left = layout.close_left(x0, w0).unwrap();
        assert!(
            ellipsized("a-very-long-space-name", 8).ends_with('…'),
            "the label must ellipsize at chip width"
        );
        assert!(ink_in(close_left, x0 + w0), "the × is painted");
        // The current chip sits on tab_active_bg and carries the focus-colour
        // marker along its bottom.
        let (x1, y1, w1, h1) = layout.chip_bounds(1, 2).unwrap();
        assert_eq!(
            rgb_at(&buffer, width, x1 + w1 - 2, y1 + 2),
            active_chip_bg(theme, focus)
        );
        assert_ne!(active_chip_bg(theme, focus), theme.pane_backdrop);
        let marker = rgb_at(&buffer, width, x1 + w1 / 2, y1 + h1 - 1);
        assert_eq!(
            marker,
            blend_rgb(focus, theme.chrome_bg, TAB_MARKER_OPACITY)
        );
        // Its close cell shows a dot in the chip's contrast ink, not a ×.
        let cell_left = layout.close_left(x1, w1).unwrap();
        let dot_x = cell_left + font.cell_w / 2;
        let dot_y = y1 + crate::mux::tab_chrome_center_y(h1);
        assert_eq!(
            rgb_at(&buffer, width, dot_x, dot_y),
            contrast_ink(active_chip_bg(theme, focus))
        );
        // The + chip is narrow and painted.
        let (x2, _, w2, _) = layout.chip_bounds(2, 2).unwrap();
        assert!(w2 < w1);
        assert!(ink_in(x2, x2 + w2), "+ glyph painted");

        rasterize_space_rail(
            theme,
            &font,
            &layout,
            &views,
            &mut buffer,
            width,
            focus,
            danger,
            Some(crate::space_rail::RailHit::Chip {
                index: 1,
                close: false,
            }),
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
        );
        assert_eq!(
            rgb_at(&buffer, width, x1 + 2, y1 + 2),
            crate::theme::hover_rgb(
                theme.variant,
                active_chip_bg(theme, focus),
                theme.chrome_fg,
                crate::config::DEFAULT_HOVER_BLEND,
            ),
            "rail hover composes over the current chip colour"
        );
    }

    #[test]
    fn editing_and_confirm_chips_fill_with_focus_and_danger() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let mut geom = HostGeom::tight(font.cell_w, font.cell_h);
        geom.rail_side = RailSide::Left;
        geom.rail_chip_cols = 10;
        geom.rail_px = rail_thickness_px(RailSide::Left, font.cell_w, font.cell_h, 10);
        let (width, height) = (40 * font.cell_w, 8 * font.cell_h);
        let names = vec!["nam".to_string(), "delete? ⏎ · Esc".to_string()];
        let layout = RailLayout::for_window(geom, width, height, &names).unwrap();
        let theme = default_theme();
        let focus = [0x10, 0x90, 0xff];
        let danger = [0xf0, 0x40, 0x40];
        let mut buffer = vec![pack_argb(OPAQUE_ALPHA, theme.default_bg); width * height];
        let mut editing = chip("nam", false);
        editing.editing = Some(false);
        let mut confirm = chip("delete? ⏎ · Esc", false);
        confirm.confirm = true;
        let views = vec![editing, confirm, plus()];
        rasterize_space_rail(
            theme,
            &font,
            &layout,
            &views,
            &mut buffer,
            width,
            focus,
            danger,
            None,
            crate::config::DEFAULT_HOVER_BLEND,
            OPAQUE_ALPHA,
        );
        let (x0, y0, w0, _) = layout.chip_bounds(0, 2).unwrap();
        assert_eq!(rgb_at(&buffer, width, x0 + w0 - 2, y0 + 1), focus);
        let (x1, y1, w1, _) = layout.chip_bounds(1, 2).unwrap();
        assert_eq!(rgb_at(&buffer, width, x1 + w1 - 2, y1 + 1), danger);
        // A side rail stacks chips: chip 1 sits below chip 0, same x.
        assert_eq!(x0, x1);
        assert!(y1 > y0);
    }
}

#[cfg(test)]
mod overlay_surface_tests {
    use super::*;

    #[test]
    fn default_overlay_surface_keeps_opaque_fast_path() {
        assert_eq!(
            OverlaySurface::default(),
            OverlaySurface::from_window(1.0, 0, false)
        );
        assert!(OverlaySurface::default().is_opaque());
        assert!(!OverlaySurface::from_window(0.75, 0, false).is_opaque());

        let mut expected = vec![pack_argb(73, [9, 8, 7]); 4];
        let mut actual = expected.clone();
        fill_rect(&mut expected, 2, 0, 0, 2, 2, [120, 100, 80]);
        paint_overlay_surface(
            &mut actual,
            2,
            0,
            0,
            2,
            2,
            [120, 100, 80],
            OverlaySurface::default(),
        );
        assert_eq!(
            actual, expected,
            "opaque overlays must keep the fill_rect path"
        );
    }

    #[test]
    fn translucent_overlay_surface_blends_with_existing_pixels() {
        let backdrop = pack_rgb([20, 40, 60]);
        let foreground = [220, 200, 180];
        let mut buffer = vec![backdrop];
        let surface = OverlaySurface::from_window(0.5, 0, false);
        paint_overlay_surface(&mut buffer, 1, 0, 0, 1, 1, foreground, surface);
        assert_eq!(
            buffer[0],
            pack_rgb(mix_rgb([20, 40, 60], foreground, opacity_to_weight(0.5)))
        );
        assert_eq!(alpha_of(buffer[0]), OPAQUE_ALPHA);
    }

    #[test]
    fn blurred_overlay_surface_uses_a_blurred_backdrop() {
        let mut buffer = vec![
            pack_rgb([0, 0, 0]),
            pack_rgb([255, 255, 255]),
            pack_rgb([0, 0, 0]),
        ];
        paint_overlay_surface(
            &mut buffer,
            3,
            0,
            0,
            3,
            1,
            [0, 0, 0],
            OverlaySurface::from_window(0.0, 1, false),
        );
        assert!(buffer[1] != pack_rgb([255, 255, 255]));
        assert!(buffer[1] != pack_rgb([0, 0, 0]));
    }

    #[test]
    fn gradient_backdrop_stays_per_pixel_through_frosted_surface() {
        let width = 16;
        let height = 32;
        let theme_rgb = [96, 80, 64];
        let red = [220, 30, 30];
        let blue = [30, 30, 220];
        let mut buffer = vec![pack_rgb(red); width * height / 2];
        buffer.extend(std::iter::repeat_n(pack_rgb(blue), width * height / 2));
        let before = buffer.clone();
        let surface = OverlaySurface::from_window(0.7, 8, false);
        paint_overlay_surface(&mut buffer, width, 0, 0, width, height, theme_rgb, surface);

        let top = unpack_rgb(buffer[2 * width + width / 2]);
        let bottom = unpack_rgb(buffer[(height - 3) * width + width / 2]);
        assert!(
            top[0] > top[2],
            "top of the panel must retain red backdrop tint"
        );
        assert!(
            bottom[2] > bottom[0],
            "bottom of the panel must retain blue backdrop tint"
        );

        let weight = opacity_to_weight(0.7);
        let expected_luminance: u32 = before
            .iter()
            .map(|&pixel| {
                let [r, g, b] = unpack_rgb(blend_pixel(pixel, theme_rgb, OPAQUE_ALPHA, weight));
                2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b)
            })
            .sum::<u32>()
            / u32::try_from(before.len()).unwrap();
        let actual_luminance: u32 = buffer
            .iter()
            .map(|&pixel| {
                let [r, g, b] = unpack_rgb(pixel);
                2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b)
            })
            .sum::<u32>()
            / u32::try_from(buffer.len()).unwrap();
        assert!(
            actual_luminance.abs_diff(expected_luminance) <= 2_000,
            "frosted mean luminance {actual_luminance} must match pane blend {expected_luminance}"
        );
    }

    #[test]
    fn bordered_panel_leaves_backdrop_for_surface_sampling() {
        let width = 20;
        let height = 40;
        let panel_x = 2;
        let panel_y = 2;
        let panel_width = 16;
        let panel_height = 32;
        let red = [220, 30, 30];
        let blue = [30, 30, 220];
        let focus = [240, 100, 20];
        let mut buffer = vec![pack_rgb(red); width * height / 2];
        buffer.extend(std::iter::repeat_n(pack_rgb(blue), width * height / 2));

        paint_panel_shadow(
            &mut buffer,
            width,
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            [10, 10, 10],
        );
        paint_panel_border(
            &mut buffer,
            width,
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            focus,
        );
        paint_overlay_surface(
            &mut buffer,
            width,
            panel_x + 1,
            panel_y + 1,
            panel_width - 2,
            panel_height - 2,
            [96, 80, 64],
            OverlaySurface::from_window(0.7, 8, false),
        );

        let top = unpack_rgb(buffer[(panel_y + 3) * width + panel_x + 4]);
        let bottom = unpack_rgb(buffer[(panel_y + panel_height - 3) * width + panel_x + 4]);
        assert!(
            top[0] > top[2],
            "bordered panel top must retain red backdrop tint"
        );
        assert!(
            bottom[2] > bottom[0],
            "bordered panel bottom must retain blue backdrop tint"
        );
        assert_eq!(unpack_rgb(buffer[panel_y * width + panel_x + 4]), focus);
        assert_eq!(
            unpack_rgb(buffer[(panel_y + panel_height - 1) * width + panel_x + 4]),
            focus
        );
    }
    #[test]
    fn palette_secondary_and_selected_text_meet_contrast_in_every_builtin_theme() {
        for theme in crate::theme::builtins() {
            let muted = readable_ink(
                mix_rgb(theme.chrome_bg, theme.chrome_fg, 140),
                theme.chrome_bg,
            );
            let bg = theme.selection_bg.unwrap_or(theme.default_fg);
            let fg = readable_ink(theme.selection_fg.unwrap_or(theme.default_bg), bg);
            assert!(
                contrast_ratio(muted, theme.chrome_bg) >= 4.5,
                "{} secondary",
                theme.id
            );
            assert!(contrast_ratio(fg, bg) >= 4.5, "{} selected", theme.id);
        }
    }
}
