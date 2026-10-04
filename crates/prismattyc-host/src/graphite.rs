//! Graphite chrome (#104): design tokens, the bundled chrome font, and the
//! tabs bar.
//!
//! Everything here is reached only when `chrome_style = "graphite"`. Classic
//! paint, layout, and hit-testing never call into this module. Layout and
//! hit-testing share [`BarLayout`], so a click lands where the paint put the
//! chip.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use fontdue::{Font, FontSettings};

use crate::mux::{ChromeGeom, StripHit};
use crate::raster::{alpha_of, contrast_ratio, mix_rgb, pack_argb, raise_alpha, unpack_rgb};
use crate::theme::ThemeVariant;

pub(crate) type Rgb = [u8; 3];

/// A Graphite design size in pixels at window scale 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Design(pub f32);

impl Design {
    pub(crate) fn px(self, chrome: ChromeGeom) -> usize {
        chrome.px(self.0)
    }
}

/// Tabs bar height.
pub(crate) const TABS_BAR_H: Design = Design(44.0);
/// Pane title row height, inside the pane slot.
pub(crate) const PANE_HEADER_H: f32 = 28.0;
/// Bottom (or top) spaces bar height.
pub(crate) const RAIL_H: Design = Design(30.0);
/// Window edge, pane gap, and pane padding defaults when the user has not
/// set `window_padding_px`, `pane_gap_px`, or `pane_padding_px`.
pub(crate) const WINDOW_PAD: Design = Design(8.0);
pub(crate) const PANE_GAP: Design = Design(8.0);
pub(crate) const PANE_PAD: Design = Design(12.0);
const PANE_RADIUS: f32 = 8.0;
const HEADER_PAD_X: f32 = 12.0;
const HEADER_TEXT: f32 = 12.0;
const HEADER_DOT: f32 = 6.0;
const RAIL_CHIP_H: f32 = 22.0;
const RAIL_CHIP_PAD_X: f32 = 9.0;
const RAIL_TEXT: f32 = 12.0;
const RAIL_DOT: f32 = 6.0;
const RAIL_CLOSE_W: f32 = 18.0;
const RAIL_PLUS_LABEL: &str = "+ New space";

const BAR_PAD_X: f32 = 10.0;
const CHIP_H: f32 = 30.0;
const CHIP_RADIUS: f32 = 6.0;
const CHIP_PAD_X: f32 = 12.0;
const CHIP_GAP: f32 = 4.0;
const DOT: f32 = 7.0;
const INNER_GAP: f32 = 8.0;
const CLOSE_W: f32 = 16.0;
const CLOSE_END: f32 = 4.0;
const DROPDOWN_LABEL_MAX: f32 = 180.0;
const TAB_LABEL_MAX: f32 = 220.0;
const TAB_LABEL_MIN: f32 = 36.0;
const PLUS_W: f32 = 30.0;
const CMD_W: f32 = 260.0;
const UNDERLINE_H: f32 = 2.0;
const UNDERLINE_INSET: f32 = 10.0;
const ICON: f32 = 14.0;
const STROKE: f32 = 1.4;
const TAB_TEXT: f32 = 13.0;
const META_TEXT: f32 = 12.0;
const PILL_TEXT: f32 = 11.0;
const PILL_PAD_X: f32 = 6.0;
const PILL_H: f32 = 16.0;
const CMD_TEXT: f32 = 12.5;
const KEY_TEXT: f32 = 11.0;
const KEY_PAD_X: f32 = 6.0;
const KEY_H: f32 = 18.0;

const fn rgb(hex: u32) -> Rgb {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]
}

/// Graphite colours for one theme variant (design brief, "Tokens").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tokens {
    /// Window ground behind the bars and panes.
    pub ground: Rgb,
    /// Tabs bar fill and its bottom hairline.
    pub bar: Rgb,
    pub bar_line: Rgb,
    /// The short rule between the Space dropdown and the first tab.
    pub divider: Rgb,
    pub tab_active: Rgb,
    /// Light themes outline the active chip so it reads on a light bar.
    pub tab_active_line: Option<Rgb>,
    pub tab_hover: Rgb,
    pub text: Rgb,
    pub text_strong: Rgb,
    pub muted: Rgb,
    pub tab_text: Rgb,
    pub working: Rgb,
    pub unseen: Rgb,
    pub attention: Rgb,
    pub on_attention: Rgb,
    pub idle: Rgb,
    pub field: Rgb,
    pub field_line: Rgb,
    pub key: Rgb,
    pub key_line: Rgb,
    pub key_text: Rgb,
    /// Bottom spaces bar fill, its hairline, and the current-Space chip.
    pub status_bar: Rgb,
    pub status_line: Rgb,
    pub chip_active: Rgb,
    /// The `·` between status counts.
    pub separator: Rgb,
    /// Pane outline, title-row rule, and the focused pane's title row.
    pub hairline: Rgb,
    pub title_line: Rgb,
    pub title_focus: Rgb,
    pub title_focus_line: Rgb,
    pub muted_focus: Rgb,
    /// Unseen/mail status as text (the dot colour fails AA on light).
    pub unseen_text: Rgb,
}

pub(crate) const DARK: Tokens = Tokens {
    ground: rgb(0x101216),
    bar: rgb(0x15181d),
    bar_line: rgb(0x23272e),
    divider: rgb(0x2b3038),
    tab_active: rgb(0x252a33),
    tab_active_line: None,
    tab_hover: rgb(0x1f232a),
    text: rgb(0xe6e9ee),
    text_strong: rgb(0xf2f4f7),
    muted: rgb(0xa3abb8),
    tab_text: rgb(0xaeb5c1),
    working: rgb(0x4cc98a),
    unseen: rgb(0xf2b84b),
    attention: rgb(0xff7a6b),
    on_attention: rgb(0x1a0d0b),
    idle: rgb(0x6b7380),
    field: rgb(0x111317),
    field_line: rgb(0x2b3038),
    key: rgb(0x1f232a),
    key_line: rgb(0x2f343d),
    key_text: rgb(0xc6ccd6),
    status_bar: rgb(0x0d0f12),
    status_line: rgb(0x1e2228),
    chip_active: rgb(0x1f232a),
    separator: rgb(0x3a404a),
    hairline: rgb(0x262a32),
    title_line: rgb(0x22262d),
    title_focus: rgb(0x1c2330),
    title_focus_line: rgb(0x2a3140),
    muted_focus: rgb(0xb4bcc9),
    unseen_text: rgb(0xf2b84b),
};

pub(crate) const LIGHT: Tokens = Tokens {
    ground: rgb(0xe9ecf0),
    bar: rgb(0xeef0f3),
    bar_line: rgb(0xd5d9df),
    divider: rgb(0xd5d9df),
    tab_active: rgb(0xffffff),
    tab_active_line: Some(rgb(0xd5d9df)),
    tab_hover: rgb(0xe2e6eb),
    text: rgb(0x1f2329),
    text_strong: rgb(0x1f2329),
    muted: rgb(0x5b6472),
    tab_text: rgb(0x4a525e),
    working: rgb(0x1f8a55),
    unseen: rgb(0xb87a0a),
    attention: rgb(0xc2392b),
    on_attention: rgb(0xffffff),
    idle: rgb(0x8a929e),
    field: rgb(0xffffff),
    field_line: rgb(0xc9ced6),
    key: rgb(0xf4f6f8),
    key_line: rgb(0xc9ced6),
    key_text: rgb(0x1f2329),
    status_bar: rgb(0xe4e7ec),
    status_line: rgb(0xd5d9df),
    chip_active: rgb(0xffffff),
    separator: rgb(0xa9b0ba),
    hairline: rgb(0xd5d9df),
    title_line: rgb(0xe3e6ea),
    title_focus: rgb(0xeaf1fc),
    title_focus_line: rgb(0xcddcf3),
    muted_focus: rgb(0x4a525e),
    unseen_text: rgb(0x9a6200),
};

pub(crate) fn tokens(variant: ThemeVariant) -> &'static Tokens {
    match variant {
        ThemeVariant::Dark => &DARK,
        ThemeVariant::Light => &LIGHT,
    }
}

/// The accent is the focus colour, darkened on light bars until it keeps the
/// 3:1 a non-text indicator needs against the bar.
pub(crate) fn accent(tok: &Tokens, focus: Rgb) -> Rgb {
    let mut color = focus;
    for _ in 0..24 {
        if contrast_ratio(color, tok.bar) >= 3.0 {
            break;
        }
        color = if tok.bar == LIGHT.bar {
            mix_rgb(color, [0, 0, 0], 24)
        } else {
            mix_rgb(color, [255, 255, 255], 24)
        };
    }
    color
}

// ---------------------------------------------------------------------------
// Chrome font

/// IBM Plex Sans (SIL OFL 1.1). License: `assets/fonts/IBMPlexSans-OFL.txt`.
const PLEX_REGULAR: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");
const PLEX_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf");

/// Plex cap height is 698/1000 em; centring on it keeps labels optically
/// centred in chips.
const CAP_HEIGHT_EM: f32 = 0.698;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Face {
    Regular,
    SemiBold,
    /// Keycaps: the bundled terminal mono face.
    Mono,
}

struct Glyph {
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    advance: f32,
    coverage: Vec<u8>,
}

struct ChromeFont {
    regular: Font,
    semibold: Font,
    mono: Font,
    cache: Mutex<HashMap<(Face, char, u32), Arc<Glyph>>>,
}

const GLYPH_CACHE_MAX: usize = 4096;

fn chrome_font() -> Option<&'static ChromeFont> {
    static FONT: OnceLock<Option<ChromeFont>> = OnceLock::new();
    FONT.get_or_init(|| {
        let load = |bytes: &[u8]| Font::from_bytes(bytes, FontSettings::default()).ok();
        Some(ChromeFont {
            regular: load(PLEX_REGULAR)?,
            semibold: load(PLEX_SEMIBOLD)?,
            mono: load(crate::raster::BAKED_MONO)?,
            cache: Mutex::new(HashMap::new()),
        })
    })
    .as_ref()
}

impl ChromeFont {
    fn face(&self, face: Face) -> &Font {
        match face {
            Face::Regular => &self.regular,
            Face::SemiBold => &self.semibold,
            Face::Mono => &self.mono,
        }
    }

    fn glyph(&self, face: Face, ch: char, px: f32) -> Arc<Glyph> {
        let key = (face, ch, (px * 64.0).round() as u32);
        if let Ok(cache) = self.cache.lock() {
            if let Some(glyph) = cache.get(&key) {
                return Arc::clone(glyph);
            }
        }
        let (metrics, coverage) = self.face(face).rasterize(ch, px);
        let glyph = Arc::new(Glyph {
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            width: metrics.width,
            height: metrics.height,
            advance: metrics.advance_width,
            coverage,
        });
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= GLYPH_CACHE_MAX {
                cache.clear();
            }
            cache.insert(key, Arc::clone(&glyph));
        }
        glyph
    }

    fn kern(&self, face: Face, left: char, right: char, px: f32) -> f32 {
        self.face(face)
            .horizontal_kern(left, right, px)
            .unwrap_or(0.0)
    }
}

/// Advance width of `text` in pixels.
pub(crate) fn text_width(face: Face, px: f32, text: &str) -> f32 {
    let Some(font) = chrome_font() else {
        return 0.0;
    };
    let mut width = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        if let Some(prev) = prev {
            width += font.kern(face, prev, ch, px);
        }
        width += font.glyph(face, ch, px).advance;
        prev = Some(ch);
    }
    width
}

/// `text` cut to fit `max_w` pixels, ending in `…` when cut.
pub(crate) fn ellipsize(face: Face, px: f32, text: &str, max_w: f32) -> String {
    if text_width(face, px, text) <= max_w {
        return text.to_string();
    }
    let ellipsis = text_width(face, px, "…");
    let mut out = String::new();
    for ch in text.chars() {
        let mut next = out.clone();
        next.push(ch);
        if text_width(face, px, &next) + ellipsis > max_w {
            break;
        }
        out = next;
    }
    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed + "…"
    }
}

/// Draw `text` with its cap height centred on `center_y`, clipped to
/// `clip_x0..clip_x1`. Returns the pen position after the last glyph.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text(
    buffer: &mut [u32],
    stride: usize,
    x: f32,
    center_y: f32,
    face: Face,
    px: f32,
    text: &str,
    ink: Rgb,
    clip_x0: usize,
    clip_x1: usize,
) -> f32 {
    let Some(font) = chrome_font() else {
        return x;
    };
    let baseline = (center_y + px * CAP_HEIGHT_EM / 2.0).round();
    let mut pen = x;
    let mut prev = None;
    for ch in text.chars() {
        if let Some(prev) = prev {
            pen += font.kern(face, prev, ch, px);
        }
        let glyph = font.glyph(face, ch, px);
        let gx = pen.round() as i32 + glyph.xmin;
        let gy = baseline as i32 - glyph.ymin - glyph.height as i32;
        for row in 0..glyph.height {
            for col in 0..glyph.width {
                let cover = glyph.coverage[row * glyph.width + col];
                if cover == 0 {
                    continue;
                }
                let px_x = gx + col as i32;
                let px_y = gy + row as i32;
                if px_x < clip_x0 as i32 || px_x >= clip_x1 as i32 {
                    continue;
                }
                blend(buffer, stride, px_x, px_y, ink, f32::from(cover) / 255.0);
            }
        }
        pen += glyph.advance;
        prev = Some(ch);
    }
    pen
}

// ---------------------------------------------------------------------------
// Shapes

/// A pixel rectangle in window coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub(crate) const fn new(x: usize, y: usize, w: usize, h: usize) -> Self {
        Self { x, y, w, h }
    }

    pub(crate) fn contains(self, px: usize, py: usize) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub(crate) fn right(self) -> usize {
        self.x + self.w
    }

    fn center_y(self) -> f32 {
        self.y as f32 + self.h as f32 / 2.0
    }

    fn inset(self, by: usize) -> Self {
        Self::new(
            self.x + by,
            self.y + by,
            self.w.saturating_sub(by * 2),
            self.h.saturating_sub(by * 2),
        )
    }
}

/// Blend `ink` over one pixel at `coverage` (0..1). Alpha only rises, so
/// anti-aliased edges never punch holes in a translucent bar.
fn blend(buffer: &mut [u32], stride: usize, x: i32, y: i32, ink: Rgb, coverage: f32) {
    if x < 0 || y < 0 || coverage <= 0.0 || x as usize >= stride {
        return;
    }
    let Some(idx) = (y as usize)
        .checked_mul(stride)
        .and_then(|row| row.checked_add(x as usize))
    else {
        return;
    };
    let Some(dst) = buffer.get_mut(idx) else {
        return;
    };
    let a = coverage.min(1.0);
    let under = unpack_rgb(*dst);
    let mix = |f: u8, d: u8| (f32::from(f) * a + f32::from(d) * (1.0 - a)).round() as u8;
    let cover = (a * 255.0).round() as u8;
    *dst = pack_argb(
        raise_alpha(alpha_of(*dst), cover),
        [
            mix(ink[0], under[0]),
            mix(ink[1], under[1]),
            mix(ink[2], under[2]),
        ],
    );
}

fn set(buffer: &mut [u32], stride: usize, x: usize, y: usize, color: u32) {
    if x >= stride {
        return;
    }
    if let Some(dst) = y
        .checked_mul(stride)
        .and_then(|row| row.checked_add(x))
        .and_then(|idx| buffer.get_mut(idx))
    {
        *dst = color;
    }
}

/// Fill a rectangle with anti-aliased round corners. Interior pixels take
/// `alpha`; edge pixels blend.
pub(crate) fn fill_round_rect(
    buffer: &mut [u32],
    stride: usize,
    rect: Rect,
    radius: f32,
    fill: Rgb,
    alpha: u8,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let r = radius
        .min(rect.w as f32 / 2.0)
        .min(rect.h as f32 / 2.0)
        .max(0.0);
    let color = pack_argb(alpha, fill);
    let (x0, y0) = (rect.x as f32, rect.y as f32);
    let (x1, y1) = (x0 + rect.w as f32, y0 + rect.h as f32);
    for py in rect.y..rect.y + rect.h {
        let cy = py as f32 + 0.5;
        for px in rect.x..rect.x + rect.w {
            let cx = px as f32 + 0.5;
            let corner_x = if cx < x0 + r {
                Some(x0 + r)
            } else if cx > x1 - r {
                Some(x1 - r)
            } else {
                None
            };
            let corner_y = if cy < y0 + r {
                Some(y0 + r)
            } else if cy > y1 - r {
                Some(y1 - r)
            } else {
                None
            };
            let coverage = match (corner_x, corner_y) {
                (Some(ox), Some(oy)) => {
                    let d = ((cx - ox).powi(2) + (cy - oy).powi(2)).sqrt();
                    (r - d + 0.5).clamp(0.0, 1.0)
                }
                _ => 1.0,
            };
            if coverage >= 1.0 {
                set(buffer, stride, px, py, color);
            } else if coverage > 0.0 {
                blend(buffer, stride, px as i32, py as i32, fill, coverage);
            }
        }
    }
}

/// A round rectangle with a 1-pixel `line` border around `fill`.
fn outlined_round_rect(
    buffer: &mut [u32],
    stride: usize,
    rect: Rect,
    radius: f32,
    line: Rgb,
    fill: Rgb,
) {
    fill_round_rect(buffer, stride, rect, radius, line, 0xff);
    fill_round_rect(
        buffer,
        stride,
        rect.inset(1),
        (radius - 1.0).max(0.0),
        fill,
        0xff,
    );
}

/// Anti-aliased disc (status dot).
fn fill_circle(buffer: &mut [u32], stride: usize, cx: f32, cy: f32, r: f32, ink: Rgb) {
    shade(
        buffer,
        stride,
        cx - r - 1.0,
        cy - r - 1.0,
        cx + r + 1.0,
        cy + r + 1.0,
        ink,
        |x, y| {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            (r - d + 0.5).clamp(0.0, 1.0)
        },
    );
}

/// Anti-aliased ring of `width` centred on radius `r`.
fn stroke_circle(
    buffer: &mut [u32],
    stride: usize,
    cx: f32,
    cy: f32,
    r: f32,
    width: f32,
    ink: Rgb,
) {
    let reach = r + width;
    shade(
        buffer,
        stride,
        cx - reach,
        cy - reach,
        cx + reach,
        cy + reach,
        ink,
        |x, y| {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            (width / 2.0 - (d - r).abs() + 0.5).clamp(0.0, 1.0)
        },
    );
}

/// Anti-aliased line segment with round caps.
#[allow(clippy::too_many_arguments)]
fn stroke_line(
    buffer: &mut [u32],
    stride: usize,
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    width: f32,
    ink: Rgb,
) {
    let half = width / 2.0;
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = (dx * dx + dy * dy).max(f32::EPSILON);
    shade(
        buffer,
        stride,
        ax.min(bx) - width,
        ay.min(by) - width,
        ax.max(bx) + width,
        ay.max(by) + width,
        ink,
        |x, y| {
            let t = (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0);
            let (nx, ny) = (ax + t * dx, ay + t * dy);
            let d = ((x - nx).powi(2) + (y - ny).powi(2)).sqrt();
            (half - d + 0.5).clamp(0.0, 1.0)
        },
    );
}

/// Run `coverage` at every pixel centre inside a float box and blend.
#[allow(clippy::too_many_arguments)]
fn shade(
    buffer: &mut [u32],
    stride: usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    ink: Rgb,
    coverage: impl Fn(f32, f32) -> f32,
) {
    let (px0, py0) = (x0.floor().max(0.0) as i32, y0.floor().max(0.0) as i32);
    let (px1, py1) = (x1.ceil() as i32, y1.ceil() as i32);
    for py in py0..py1 {
        for px in px0..px1 {
            let c = coverage(px as f32 + 0.5, py as f32 + 0.5);
            if c > 0.0 {
                blend(buffer, stride, px, py, ink, c);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tabs bar

/// Status dot on a tab: what a glance at the bar should tell you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dot {
    Attention,
    Working,
    Unseen,
    Idle,
}

impl Dot {
    /// Attention first, then live output, then unseen output.
    pub(crate) fn for_tab(attention: bool, active: bool, unseen: bool) -> Self {
        if attention {
            Self::Attention
        } else if active {
            Self::Working
        } else if unseen {
            Self::Unseen
        } else {
            Self::Idle
        }
    }
}

/// What one tab chip shows, decided by the caller from `TabInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabText {
    pub label: String,
    /// Muted text after the label: `4 panes`, `4 panes · zoomed`.
    pub meta: Option<String>,
    pub dot: Dot,
    /// Shows the `needs you` pill.
    pub attention: bool,
    pub selected: bool,
}

/// One laid-out tab chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabSlot {
    pub chip: Rect,
    pub close: Rect,
    pub label: String,
    pub meta: Option<String>,
    pub dot: Dot,
    pub attention: bool,
    pub selected: bool,
}

/// The whole tabs bar, in window pixels. Paint and hit-testing read this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarLayout {
    pub bar: Rect,
    pub dropdown: Rect,
    pub space_label: String,
    pub divider_x: usize,
    /// Tabs that fit, in order. Index `i` is tab `i`.
    pub tabs: Vec<TabSlot>,
    pub plus: Rect,
    /// The `Run a command` field; dropped first when the bar is narrow.
    pub command: Option<Rect>,
    pub chord: String,
    pub chrome: ChromeGeom,
}

fn chip_width(chrome: ChromeGeom, tab: &TabText, label_w: f32) -> f32 {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let mut w = s(CHIP_PAD_X) + s(DOT) + s(INNER_GAP) + label_w;
    if let Some(meta) = &tab.meta {
        w += s(INNER_GAP) + text_width(Face::Regular, s(META_TEXT), meta);
    }
    if tab.attention {
        w += s(INNER_GAP) + pill_width(chrome);
    }
    w + s(INNER_GAP) + s(CLOSE_W) + s(CLOSE_END)
}

fn pill_width(chrome: ChromeGeom) -> f32 {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    text_width(Face::SemiBold, s(PILL_TEXT), "needs you") + 2.0 * s(PILL_PAD_X)
}

/// Lay out the tabs bar across `width` pixels starting at window row `y`.
pub(crate) fn bar_layout(
    chrome: ChromeGeom,
    width: usize,
    y: usize,
    space_label: &str,
    tabs: &[TabText],
    chord: &str,
) -> BarLayout {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let p = |d: f32| chrome.px(d);
    let bar = Rect::new(0, y, width, TABS_BAR_H.px(chrome));
    let chip_h = p(CHIP_H).min(bar.h);
    let chip_y = y + (bar.h - chip_h) / 2;
    let mut x = p(BAR_PAD_X);

    let space_label = ellipsize(
        Face::SemiBold,
        s(TAB_TEXT),
        space_label,
        s(DROPDOWN_LABEL_MAX),
    );
    let dropdown_w = (s(10.0)
        + s(ICON)
        + s(INNER_GAP)
        + text_width(Face::SemiBold, s(TAB_TEXT), &space_label)
        + s(INNER_GAP)
        + s(10.0)
        + s(10.0))
    .ceil() as usize;
    let dropdown = Rect::new(x, chip_y, dropdown_w, chip_h);
    x += dropdown_w + p(6.0);
    let divider_x = x;
    x += 1 + p(6.0);

    let right_edge = width.saturating_sub(p(BAR_PAD_X));
    let cmd_w = p(CMD_W);
    let plus_w = p(PLUS_W);
    let gap = p(CHIP_GAP);
    let fits = |label_cap: f32, command: bool| -> bool {
        let tabs_w: f32 = tabs
            .iter()
            .map(|tab| {
                let label_w = text_width(Face::Regular, s(TAB_TEXT), &tab.label).min(label_cap);
                chip_width(chrome, tab, label_w).ceil() + gap as f32
            })
            .sum();
        let end = x as f32 + tabs_w + plus_w as f32;
        let limit = if command {
            right_edge.saturating_sub(cmd_w + p(INNER_GAP)) as f32
        } else {
            right_edge as f32
        };
        end <= limit
    };
    let mut command = cmd_w + p(INNER_GAP) + x + plus_w <= right_edge;
    if command && !fits(s(TAB_LABEL_MAX), true) {
        command = fits(s(TAB_LABEL_MIN), true);
    }
    // Largest label cap that fits, by halving the search interval.
    let mut cap = s(TAB_LABEL_MAX);
    if !fits(cap, command) {
        let (mut lo, mut hi) = (s(TAB_LABEL_MIN), cap);
        for _ in 0..12 {
            let mid = (lo + hi) / 2.0;
            if fits(mid, command) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        cap = lo;
    }
    let tab_limit = if command {
        right_edge.saturating_sub(cmd_w + p(INNER_GAP))
    } else {
        right_edge
    }
    .saturating_sub(plus_w);
    let mut slots = Vec::with_capacity(tabs.len());
    for tab in tabs {
        let label = ellipsize(Face::Regular, s(TAB_TEXT), &tab.label, cap);
        let label_w = text_width(Face::Regular, s(TAB_TEXT), &label);
        let w = chip_width(chrome, tab, label_w).ceil() as usize;
        if x + w > tab_limit {
            break;
        }
        let chip = Rect::new(x, chip_y, w, chip_h);
        let close_w = p(CLOSE_W);
        let close = Rect::new(
            chip.right().saturating_sub(p(CLOSE_END) + close_w),
            chip_y,
            close_w,
            chip_h,
        );
        slots.push(TabSlot {
            chip,
            close,
            label,
            meta: tab.meta.clone(),
            dot: tab.dot,
            attention: tab.attention,
            selected: tab.selected,
        });
        x += w + gap;
    }
    let plus = Rect::new(x, chip_y, plus_w, chip_h);
    let command =
        command.then(|| Rect::new(right_edge.saturating_sub(cmd_w), chip_y, cmd_w, chip_h));
    BarLayout {
        bar,
        dropdown,
        space_label,
        divider_x,
        tabs: slots,
        plus,
        command,
        chord: chord.to_string(),
        chrome,
    }
}

/// What sits under a window pixel in the tabs bar.
pub(crate) fn bar_hit(
    layout: &BarLayout,
    px: usize,
    py: usize,
    reserve_end: bool,
) -> Option<StripHit> {
    if !layout.bar.contains(px, py) {
        return None;
    }
    if layout.dropdown.contains(px, py) {
        return Some(StripHit::SpaceMenu);
    }
    for (index, tab) in layout.tabs.iter().enumerate() {
        if tab.chip.contains(px, py) {
            return Some(StripHit::Tab {
                index,
                close: tab.close.contains(px, py),
            });
        }
    }
    if layout.plus.contains(px, py) {
        return Some(StripHit::NewTab);
    }
    if layout.command.is_some_and(|rect| rect.contains(px, py)) {
        return Some(StripHit::Command);
    }
    let after_tabs = px >= layout.plus.x;
    (reserve_end && after_tabs).then_some(StripHit::EmptyEnd)
}

/// Inputs for one tabs-bar paint.
pub(crate) struct BarPaint<'a> {
    pub layout: &'a BarLayout,
    pub tok: &'a Tokens,
    pub accent: Rgb,
    pub hover: Option<StripHit>,
    /// `chrome_opacity` for the bar ground; chips, text, and dots stay opaque.
    pub bar_alpha: u8,
    /// Inline rename: tab index, buffer, select-all.
    pub editing: Option<(usize, &'a str, bool)>,
}

pub(crate) fn paint_tabs_bar(buffer: &mut [u32], stride: usize, paint: &BarPaint<'_>) {
    let layout = paint.layout;
    let tok = paint.tok;
    let chrome = layout.chrome;
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let p = |d: f32| chrome.px(d);
    let bar = layout.bar;
    if bar.w == 0 || bar.h == 0 {
        return;
    }
    // Ground and bottom hairline.
    let ground = pack_argb(paint.bar_alpha, tok.bar);
    for y in bar.y..bar.y + bar.h.saturating_sub(1) {
        for x in bar.x..bar.right().min(stride) {
            set(buffer, stride, x, y, ground);
        }
    }
    let line = pack_argb(paint.bar_alpha.max(0xff / 2), tok.bar_line);
    for x in bar.x..bar.right().min(stride) {
        set(buffer, stride, x, bar.y + bar.h - 1, line);
    }

    // Space dropdown.
    let dd = layout.dropdown;
    if paint.hover == Some(StripHit::SpaceMenu) {
        fill_round_rect(buffer, stride, dd, s(CHIP_RADIUS), tok.tab_hover, 0xff);
    }
    let icon_x = dd.x as f32 + s(10.0);
    grid_icon(
        buffer,
        stride,
        icon_x,
        dd.center_y(),
        s(ICON),
        s(STROKE),
        tok.muted,
    );
    let text_x = icon_x + s(ICON) + s(INNER_GAP);
    let end = draw_text(
        buffer,
        stride,
        text_x,
        dd.center_y(),
        Face::SemiBold,
        s(TAB_TEXT),
        &layout.space_label,
        tok.text,
        dd.x,
        dd.right(),
    );
    chevron_down(
        buffer,
        stride,
        end + s(INNER_GAP),
        dd.center_y(),
        s(10.0),
        s(STROKE),
        tok.muted,
    );

    // Divider.
    let div_h = p(18.0);
    let div_y = bar.y + (bar.h.saturating_sub(div_h)) / 2;
    for y in div_y..div_y + div_h {
        set(
            buffer,
            stride,
            layout.divider_x,
            y,
            pack_argb(0xff, tok.divider),
        );
    }

    // Tabs.
    for (index, tab) in layout.tabs.iter().enumerate() {
        paint_tab(buffer, stride, paint, index, tab);
    }

    // `+`.
    let plus = layout.plus;
    if paint.hover == Some(StripHit::NewTab) {
        fill_round_rect(buffer, stride, plus, s(CHIP_RADIUS), tok.tab_hover, 0xff);
    }
    let (cx, cy) = (plus.x as f32 + plus.w as f32 / 2.0, plus.center_y());
    let arm = s(6.0);
    stroke_line(
        buffer,
        stride,
        cx - arm,
        cy,
        cx + arm,
        cy,
        s(1.5),
        tok.tab_text,
    );
    stroke_line(
        buffer,
        stride,
        cx,
        cy - arm,
        cx,
        cy + arm,
        s(1.5),
        tok.tab_text,
    );

    // `Run a command`.
    if let Some(field) = layout.command {
        let fill = if paint.hover == Some(StripHit::Command) {
            tok.tab_hover
        } else {
            tok.field
        };
        outlined_round_rect(buffer, stride, field, s(CHIP_RADIUS), tok.field_line, fill);
        let icon_cx = field.x as f32 + s(10.0) + s(6.5);
        search_icon(
            buffer,
            stride,
            icon_cx,
            field.center_y(),
            s(13.0),
            s(1.5),
            tok.muted,
        );
        let key_px = s(KEY_TEXT);
        let key_w =
            (text_width(Face::Mono, key_px, &layout.chord) + 2.0 * s(KEY_PAD_X)).ceil() as usize;
        let key_h = p(KEY_H);
        let key = Rect::new(
            field.right().saturating_sub(p(8.0) + key_w),
            field.y + field.h.saturating_sub(key_h) / 2,
            key_w,
            key_h,
        );
        draw_text(
            buffer,
            stride,
            icon_cx + s(6.5) + s(10.0),
            field.center_y(),
            Face::Regular,
            s(CMD_TEXT),
            "Run a command",
            tok.muted,
            field.x,
            key.x.saturating_sub(p(4.0)),
        );
        if !layout.chord.is_empty() && key.x > field.x {
            outlined_round_rect(buffer, stride, key, s(4.0), tok.key_line, tok.key);
            draw_text(
                buffer,
                stride,
                key.x as f32 + s(KEY_PAD_X),
                key.center_y(),
                Face::Mono,
                key_px,
                &layout.chord,
                tok.key_text,
                key.x,
                key.right(),
            );
        }
    }
}

fn paint_tab(buffer: &mut [u32], stride: usize, paint: &BarPaint<'_>, index: usize, tab: &TabSlot) {
    let tok = paint.tok;
    let chrome = paint.layout.chrome;
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let p = |d: f32| chrome.px(d);
    let chip = tab.chip;
    let editing = paint
        .editing
        .filter(|(at, _, _)| *at == index)
        .map(|(_, text, all)| (text, all));
    let hovered = matches!(paint.hover, Some(StripHit::Tab { index: at, .. }) if at == index);
    let close_hover = paint.hover == Some(StripHit::Tab { index, close: true });
    if editing.is_some() {
        outlined_round_rect(
            buffer,
            stride,
            chip,
            s(CHIP_RADIUS),
            paint.accent,
            tok.tab_active,
        );
    } else if tab.selected {
        match tok.tab_active_line {
            Some(line) => {
                outlined_round_rect(buffer, stride, chip, s(CHIP_RADIUS), line, tok.tab_active)
            }
            None => fill_round_rect(buffer, stride, chip, s(CHIP_RADIUS), tok.tab_active, 0xff),
        }
    } else if hovered {
        fill_round_rect(buffer, stride, chip, s(CHIP_RADIUS), tok.tab_hover, 0xff);
    }
    let cy = chip.center_y();
    let mut x = chip.x as f32 + s(CHIP_PAD_X);
    let dot_r = s(DOT) / 2.0;
    match tab.dot {
        Dot::Idle => stroke_circle(
            buffer,
            stride,
            x + dot_r,
            cy,
            dot_r - s(0.75),
            s(1.5),
            tok.idle,
        ),
        Dot::Working => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.working),
        Dot::Unseen => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.unseen),
        Dot::Attention => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.attention),
    }
    x += s(DOT) + s(INNER_GAP);
    let clip_end = tab.close.x;
    if let Some((text, select_all)) = editing {
        let w = text_width(Face::Regular, s(TAB_TEXT), text);
        if select_all {
            let sel = Rect::new(
                x.floor() as usize,
                chip.y + p(6.0),
                w.ceil() as usize,
                chip.h.saturating_sub(p(12.0)),
            );
            fill_round_rect(buffer, stride, sel, s(2.0), paint.accent, 0xff);
        }
        let ink = if select_all {
            crate::raster::contrast_ink(paint.accent)
        } else {
            tok.text_strong
        };
        let end = draw_text(
            buffer,
            stride,
            x,
            cy,
            Face::Regular,
            s(TAB_TEXT),
            text,
            ink,
            chip.x,
            clip_end,
        );
        if !select_all {
            let caret_x = end.round() as usize + 1;
            for y in chip.y + p(8.0)..chip.y + chip.h.saturating_sub(p(8.0)) {
                set(buffer, stride, caret_x, y, pack_argb(0xff, tok.text_strong));
            }
        }
        return;
    }
    let ink = if tab.selected {
        tok.text_strong
    } else {
        tok.tab_text
    };
    x = draw_text(
        buffer,
        stride,
        x,
        cy,
        Face::Regular,
        s(TAB_TEXT),
        &tab.label,
        ink,
        chip.x,
        clip_end,
    );
    if let Some(meta) = &tab.meta {
        x = draw_text(
            buffer,
            stride,
            x + s(INNER_GAP),
            cy,
            Face::Regular,
            s(META_TEXT),
            meta,
            tok.muted,
            chip.x,
            clip_end,
        );
    }
    if tab.attention {
        let pill_w = pill_width(paint.layout.chrome).ceil() as usize;
        let pill_h = p(PILL_H);
        let pill = Rect::new(
            (x + s(INNER_GAP)).round() as usize,
            chip.y + chip.h.saturating_sub(pill_h) / 2,
            pill_w,
            pill_h,
        );
        if pill.right() <= clip_end {
            fill_round_rect(
                buffer,
                stride,
                pill,
                pill_h as f32 / 2.0,
                tok.attention,
                0xff,
            );
            draw_text(
                buffer,
                stride,
                pill.x as f32 + s(PILL_PAD_X),
                pill.center_y(),
                Face::SemiBold,
                s(PILL_TEXT),
                "needs you",
                tok.on_attention,
                pill.x,
                pill.right(),
            );
        }
    }
    if hovered {
        let close = tab.close;
        let (ccx, ccy) = (close.x as f32 + close.w as f32 / 2.0, close.center_y());
        if close_hover {
            fill_circle(
                buffer,
                stride,
                ccx,
                ccy,
                s(8.0),
                mix_rgb(tok.tab_active, tok.text, 40),
            );
        }
        let arm = s(3.5);
        let ink = if close_hover {
            tok.text_strong
        } else {
            tok.muted
        };
        stroke_line(
            buffer,
            stride,
            ccx - arm,
            ccy - arm,
            ccx + arm,
            ccy + arm,
            s(1.4),
            ink,
        );
        stroke_line(
            buffer,
            stride,
            ccx + arm,
            ccy - arm,
            ccx - arm,
            ccy + arm,
            s(1.4),
            ink,
        );
    }
    if tab.selected {
        let inset = p(UNDERLINE_INSET);
        let under = Rect::new(
            chip.x + inset,
            paint.layout.bar.y + paint.layout.bar.h - p(UNDERLINE_H),
            chip.w.saturating_sub(inset * 2),
            p(UNDERLINE_H),
        );
        fill_round_rect(buffer, stride, under, s(1.0), paint.accent, 0xff);
    }
}

fn grid_icon(buffer: &mut [u32], stride: usize, x: f32, cy: f32, size: f32, width: f32, ink: Rgb) {
    let cell = size * 4.5 / 14.0;
    let y = cy - size / 2.0;
    for (ox, oy) in [(1.5, 1.5), (8.0, 1.5), (1.5, 8.0), (8.0, 8.0)] {
        let (rx, ry) = (x + size * ox / 14.0, y + size * oy / 14.0);
        stroke_line(buffer, stride, rx, ry, rx + cell, ry, width, ink);
        stroke_line(
            buffer,
            stride,
            rx + cell,
            ry,
            rx + cell,
            ry + cell,
            width,
            ink,
        );
        stroke_line(
            buffer,
            stride,
            rx + cell,
            ry + cell,
            rx,
            ry + cell,
            width,
            ink,
        );
        stroke_line(buffer, stride, rx, ry + cell, rx, ry, width, ink);
    }
}

fn chevron_down(
    buffer: &mut [u32],
    stride: usize,
    x: f32,
    cy: f32,
    size: f32,
    width: f32,
    ink: Rgb,
) {
    let (a, b, c) = (size * 0.2, size * 0.5, size * 0.8);
    let top = cy - size * 0.1;
    stroke_line(
        buffer,
        stride,
        x + a,
        top - size * 0.1,
        x + b,
        top + size * 0.2,
        width,
        ink,
    );
    stroke_line(
        buffer,
        stride,
        x + b,
        top + size * 0.2,
        x + c,
        top - size * 0.1,
        width,
        ink,
    );
}

fn search_icon(
    buffer: &mut [u32],
    stride: usize,
    cx: f32,
    cy: f32,
    size: f32,
    width: f32,
    ink: Rgb,
) {
    let r = size * 4.0 / 13.0;
    let (ox, oy) = (cx - size * 1.0 / 13.0, cy - size * 1.0 / 13.0);
    stroke_circle(buffer, stride, ox, oy, r, width, ink);
    let start = r * 0.75;
    stroke_line(
        buffer,
        stride,
        ox + start,
        oy + start,
        ox + size * 6.5 / 13.0,
        oy + size * 6.5 / 13.0,
        width,
        ink,
    );
}

// ---------------------------------------------------------------------------
// Rounded outlines

/// Signed distance from a pixel centre to a rounded rectangle's edge
/// (negative inside).
fn round_rect_sd(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (bx, by) = ((x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
    let (qx, qy) = ((x - cx).abs() - bx, (y - cy).abs() - by);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Anti-aliased `width`-pixel band just inside a rounded rectangle's edge.
/// Only the edge strips are visited, so cost scales with the perimeter.
#[allow(clippy::too_many_arguments)]
fn stroke_round_rect(
    buffer: &mut [u32],
    stride: usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    width: f32,
    ink: Rgb,
) {
    if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
        return;
    }
    let r = radius.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
    let coverage = |x: f32, y: f32| {
        let sd = round_rect_sd(x, y, x0, y0, x1, y1, r);
        (0.5 - sd).min(sd + width + 0.5).clamp(0.0, 1.0)
    };
    let reach = r + width + 1.0;
    // Top and bottom bands (corners included), then the straight sides.
    shade(
        buffer,
        stride,
        x0 - 1.0,
        y0 - 1.0,
        x1 + 1.0,
        y0 + reach,
        ink,
        coverage,
    );
    shade(
        buffer,
        stride,
        x0 - 1.0,
        y1 - reach,
        x1 + 1.0,
        y1 + 1.0,
        ink,
        coverage,
    );
    shade(
        buffer,
        stride,
        x0 - 1.0,
        y0 + reach,
        x0 + width + 1.0,
        y1 - reach,
        ink,
        coverage,
    );
    shade(
        buffer,
        stride,
        x1 - width - 1.0,
        y0 + reach,
        x1 + 1.0,
        y1 - reach,
        ink,
        coverage,
    );
}

// ---------------------------------------------------------------------------
// Panes

/// Right-hand status in a pane title row, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneStatus {
    Attention,
    Mail(u32),
    Unseen,
    Running,
    Focused,
    Quiet,
}

impl PaneStatus {
    pub(crate) fn decide(
        attention: bool,
        mail: u32,
        unseen: bool,
        running: bool,
        focused: bool,
    ) -> Self {
        if attention {
            Self::Attention
        } else if mail > 0 {
            Self::Mail(mail)
        } else if unseen {
            Self::Unseen
        } else if running {
            Self::Running
        } else if focused {
            Self::Focused
        } else {
            Self::Quiet
        }
    }
}

/// One pane title row.
pub(crate) struct PaneHeader<'a> {
    pub name: &'a str,
    pub meta: Option<&'a str>,
    pub dot: Dot,
    pub status: PaneStatus,
    pub focused: bool,
}

/// The pane slot as a rounded card: `ground` fills the corners, `surface`
/// the inside. Run before the terminal rows are painted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_pane_surface(
    buffer: &mut [u32],
    stride: usize,
    chrome: ChromeGeom,
    slot: Rect,
    ground: Rgb,
    ground_alpha: u8,
    surface: Rgb,
    surface_alpha: u8,
) {
    let r = chrome.px(PANE_RADIUS);
    let ground_px = pack_argb(ground_alpha, ground);
    // Only the corner squares can show ground; the rest is the card.
    for (cx, cy) in [
        (slot.x, slot.y),
        (slot.right().saturating_sub(r), slot.y),
        (slot.x, (slot.y + slot.h).saturating_sub(r)),
        (
            slot.right().saturating_sub(r),
            (slot.y + slot.h).saturating_sub(r),
        ),
    ] {
        for y in cy..cy + r {
            for x in cx..cx + r {
                set(buffer, stride, x, y, ground_px);
            }
        }
    }
    fill_round_rect(buffer, stride, slot, r as f32, surface, surface_alpha);
}

/// Title row, outline, and (multi-pane) focus ring, painted after the
/// terminal rows. The 2-pixel ring is 1 pixel on the slot edge and 1 in the
/// gap outside it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_pane_chrome(
    buffer: &mut [u32],
    stride: usize,
    chrome: ChromeGeom,
    tok: &Tokens,
    accent: Rgb,
    slot: Rect,
    surface: Rgb,
    header: &PaneHeader<'_>,
    ring: bool,
) {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let p = |d: f32| chrome.px(d);
    let r = s(PANE_RADIUS);
    let head_h = p(PANE_HEADER_H).min(slot.h);
    if slot.w < p(40.0) || head_h == 0 {
        return;
    }
    let focus_row = header.focused && ring;
    // Title row ground: the focus tint, or the pane surface.
    let row_fill = if focus_row { tok.title_focus } else { surface };
    let head = Rect::new(slot.x, slot.y, slot.w, head_h);
    fill_round_rect(buffer, stride, head, r, row_fill, 0xff);
    let lower = Rect::new(slot.x, slot.y + head_h / 2, slot.w, head_h - head_h / 2);
    for y in lower.y..lower.y + lower.h {
        for x in lower.x..lower.right() {
            set(buffer, stride, x, y, pack_argb(0xff, row_fill));
        }
    }
    let rule = if focus_row {
        tok.title_focus_line
    } else {
        tok.title_line
    };
    for x in slot.x..slot.right() {
        set(
            buffer,
            stride,
            x,
            slot.y + head_h - 1,
            pack_argb(0xff, rule),
        );
    }
    // Dot, name, meta.
    let cy = slot.y as f32 + (head_h - 1) as f32 / 2.0;
    let mut x = slot.x as f32 + s(HEADER_PAD_X);
    let dot_r = s(HEADER_DOT) / 2.0;
    match header.dot {
        Dot::Idle => stroke_circle(
            buffer,
            stride,
            x + dot_r,
            cy,
            dot_r - s(0.6),
            s(1.2),
            tok.idle,
        ),
        Dot::Working => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.working),
        Dot::Unseen => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.unseen),
        Dot::Attention => fill_circle(buffer, stride, x + dot_r, cy, dot_r, tok.attention),
    }
    x += s(HEADER_DOT) + s(INNER_GAP);
    let px = s(HEADER_TEXT);
    let status_w = status_width(chrome, header.status);
    let text_end = slot
        .right()
        .saturating_sub(p(HEADER_PAD_X) + status_w.ceil() as usize + p(INNER_GAP));
    let (face, name_ink) = if focus_row {
        (Face::SemiBold, tok.text_strong)
    } else {
        (Face::Regular, tok.text)
    };
    let name = ellipsize(face, px, header.name, (text_end as f32 - x).max(0.0));
    x = draw_text(
        buffer, stride, x, cy, face, px, &name, name_ink, slot.x, text_end,
    );
    if let Some(meta) = header.meta.filter(|m| !m.is_empty()) {
        let start = x + s(INNER_GAP);
        let meta = ellipsize(Face::Regular, px, meta, (text_end as f32 - start).max(0.0));
        let ink = if focus_row {
            tok.muted_focus
        } else {
            tok.muted
        };
        draw_text(
            buffer,
            stride,
            start,
            cy,
            Face::Regular,
            px,
            &meta,
            ink,
            slot.x,
            text_end,
        );
    }
    // Status on the right.
    let mut sx = slot.right() as f32 - s(HEADER_PAD_X) - status_w;
    let muted = if focus_row {
        tok.muted_focus
    } else {
        tok.muted
    };
    match header.status {
        PaneStatus::Attention => {
            draw_text(
                buffer,
                stride,
                sx,
                cy,
                Face::SemiBold,
                px,
                "needs you",
                tok.attention,
                slot.x,
                slot.right(),
            );
        }
        PaneStatus::Mail(count) => {
            envelope(buffer, stride, sx, cy, s(14.0), s(1.3), tok.unseen);
            sx += s(14.0) + s(6.0);
            draw_text(
                buffer,
                stride,
                sx,
                cy,
                Face::Regular,
                px,
                &count.to_string(),
                tok.unseen_text,
                slot.x,
                slot.right(),
            );
        }
        PaneStatus::Unseen => {
            draw_text(
                buffer,
                stride,
                sx,
                cy,
                Face::Regular,
                px,
                "new output",
                tok.unseen_text,
                slot.x,
                slot.right(),
            );
        }
        PaneStatus::Running => {
            draw_text(
                buffer,
                stride,
                sx,
                cy,
                Face::Regular,
                px,
                "running",
                muted,
                slot.x,
                slot.right(),
            );
        }
        PaneStatus::Focused => {
            draw_text(
                buffer,
                stride,
                sx,
                cy,
                Face::Regular,
                px,
                "focused",
                muted,
                slot.x,
                slot.right(),
            );
        }
        PaneStatus::Quiet => {}
    }
    // Outline, then the focus ring over it.
    let (x0, y0) = (slot.x as f32, slot.y as f32);
    let (x1, y1) = (slot.right() as f32, (slot.y + slot.h) as f32);
    if ring && header.focused {
        stroke_round_rect(
            buffer,
            stride,
            x0 - 1.0,
            y0 - 1.0,
            x1 + 1.0,
            y1 + 1.0,
            r + 1.0,
            2.0,
            accent,
        );
    } else {
        stroke_round_rect(buffer, stride, x0, y0, x1, y1, r, 1.0, tok.hairline);
    }
}

fn status_width(chrome: ChromeGeom, status: PaneStatus) -> f32 {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let px = s(HEADER_TEXT);
    match status {
        PaneStatus::Attention => text_width(Face::SemiBold, px, "needs you"),
        PaneStatus::Mail(count) => {
            s(14.0) + s(6.0) + text_width(Face::Regular, px, &count.to_string())
        }
        PaneStatus::Unseen => text_width(Face::Regular, px, "new output"),
        PaneStatus::Running => text_width(Face::Regular, px, "running"),
        PaneStatus::Focused => text_width(Face::Regular, px, "focused"),
        PaneStatus::Quiet => 0.0,
    }
}

fn envelope(buffer: &mut [u32], stride: usize, x: f32, cy: f32, w: f32, width: f32, ink: Rgb) {
    let h = w * 11.0 / 14.0;
    let (x0, y0, x1, y1) = (
        x + width / 2.0,
        cy - h / 2.0 + width / 2.0,
        x + w - width / 2.0,
        cy + h / 2.0 - width / 2.0,
    );
    stroke_line(buffer, stride, x0, y0, x1, y0, width, ink);
    stroke_line(buffer, stride, x1, y0, x1, y1, width, ink);
    stroke_line(buffer, stride, x1, y1, x0, y1, width, ink);
    stroke_line(buffer, stride, x0, y1, x0, y0, width, ink);
    let mid = (x0 + x1) / 2.0;
    stroke_line(buffer, stride, x0, y0, mid, cy + h * 0.1, width, ink);
    stroke_line(buffer, stride, mid, cy + h * 0.1, x1, y0, width, ink);
}

// ---------------------------------------------------------------------------
// Spaces bar (bottom or top)

/// Chip width for a Space name. The right `RAIL_CLOSE_W` is the close
/// target, drawn only on hover.
pub(crate) fn rail_chip_width(chrome: ChromeGeom, label: &str, current: bool) -> usize {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let dot = if current { s(RAIL_DOT) + s(7.0) } else { 0.0 };
    (s(RAIL_CHIP_PAD_X)
        + dot
        + text_width(Face::Regular, s(RAIL_TEXT), label)
        + s(6.0)
        + s(RAIL_CLOSE_W))
    .ceil() as usize
}

pub(crate) fn rail_close_width(chrome: ChromeGeom) -> usize {
    chrome.px(RAIL_CLOSE_W)
}

pub(crate) fn rail_plus_width(chrome: ChromeGeom) -> usize {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    (2.0 * s(RAIL_CHIP_PAD_X) + text_width(Face::Regular, s(RAIL_TEXT), RAIL_PLUS_LABEL)).ceil()
        as usize
}

/// One chip on the spaces bar.
pub(crate) struct RailChip<'a> {
    /// The rail's full-height slot for this chip.
    pub slot: Rect,
    pub label: &'a str,
    pub current: bool,
    /// Keyboard focus (`space_rail_focus`).
    pub focused: bool,
    pub editing: bool,
    pub hovered: bool,
    pub close_hovered: bool,
}

fn rail_chip_rect(chrome: ChromeGeom, slot: Rect) -> Rect {
    let h = chrome.px(RAIL_CHIP_H).min(slot.h);
    Rect::new(slot.x, slot.y + (slot.h - h) / 2, slot.w, h)
}

/// Bar ground with its hairline on the pane side.
pub(crate) fn paint_rail_bar(
    buffer: &mut [u32],
    stride: usize,
    tok: &Tokens,
    bar: Rect,
    line_on_top: bool,
    alpha: u8,
) {
    let ground = pack_argb(alpha, tok.status_bar);
    for y in bar.y..bar.y + bar.h {
        for x in bar.x..bar.right() {
            set(buffer, stride, x, y, ground);
        }
    }
    let line_y = if line_on_top {
        bar.y
    } else {
        (bar.y + bar.h).saturating_sub(1)
    };
    for x in bar.x..bar.right() {
        set(buffer, stride, x, line_y, pack_argb(0xff, tok.status_line));
    }
}

pub(crate) fn paint_rail_chip(
    buffer: &mut [u32],
    stride: usize,
    chrome: ChromeGeom,
    tok: &Tokens,
    accent: Rgb,
    chip: &RailChip<'_>,
) {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let rect = rail_chip_rect(chrome, chip.slot);
    if chip.editing || chip.focused {
        let fill = if chip.current {
            tok.chip_active
        } else {
            tok.status_bar
        };
        outlined_round_rect(buffer, stride, rect, s(5.0), accent, fill);
    } else if chip.current {
        fill_round_rect(buffer, stride, rect, s(5.0), tok.chip_active, 0xff);
    } else if chip.hovered {
        fill_round_rect(buffer, stride, rect, s(5.0), tok.tab_hover, 0xff);
    }
    let cy = rect.center_y();
    let mut x = rect.x as f32 + s(RAIL_CHIP_PAD_X);
    if chip.current {
        fill_circle(
            buffer,
            stride,
            x + s(RAIL_DOT) / 2.0,
            cy,
            s(RAIL_DOT) / 2.0,
            accent,
        );
        x += s(RAIL_DOT) + s(7.0);
    }
    let close_x = rect.right().saturating_sub(chrome.px(RAIL_CLOSE_W));
    let ink = if chip.current || chip.editing {
        tok.text
    } else {
        tok.muted
    };
    let label = ellipsize(
        Face::Regular,
        s(RAIL_TEXT),
        chip.label,
        (close_x as f32 - x).max(0.0),
    );
    draw_text(
        buffer,
        stride,
        x,
        cy,
        Face::Regular,
        s(RAIL_TEXT),
        &label,
        ink,
        rect.x,
        close_x,
    );
    if chip.hovered && !chip.editing {
        let ccx = close_x as f32 + s(RAIL_CLOSE_W) / 2.0 - s(2.0);
        if chip.close_hovered {
            fill_circle(
                buffer,
                stride,
                ccx,
                cy,
                s(7.0),
                mix_rgb(tok.chip_active, tok.text, 40),
            );
        }
        let arm = s(3.0);
        let ink = if chip.close_hovered {
            tok.text_strong
        } else {
            tok.muted
        };
        stroke_line(
            buffer,
            stride,
            ccx - arm,
            cy - arm,
            ccx + arm,
            cy + arm,
            s(1.3),
            ink,
        );
        stroke_line(
            buffer,
            stride,
            ccx + arm,
            cy - arm,
            ccx - arm,
            cy + arm,
            s(1.3),
            ink,
        );
    }
}

/// `+ New space` (or the overflow chip's label).
pub(crate) fn paint_rail_button(
    buffer: &mut [u32],
    stride: usize,
    chrome: ChromeGeom,
    tok: &Tokens,
    slot: Rect,
    label: &str,
    hovered: bool,
) {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let rect = rail_chip_rect(chrome, slot);
    if hovered {
        fill_round_rect(buffer, stride, rect, s(5.0), tok.tab_hover, 0xff);
    }
    draw_text(
        buffer,
        stride,
        rect.x as f32 + s(RAIL_CHIP_PAD_X),
        rect.center_y(),
        Face::Regular,
        s(RAIL_TEXT),
        label,
        tok.muted,
        rect.x,
        rect.right(),
    );
}

pub(crate) const RAIL_PLUS: &str = RAIL_PLUS_LABEL;

/// Right-aligned status: `N working · N needs you · Hold Ctrl Shift for
/// shortcuts`. Items drop from the left when the chips leave no room.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_rail_status(
    buffer: &mut [u32],
    stride: usize,
    chrome: ChromeGeom,
    tok: &Tokens,
    bar: Rect,
    left_limit: usize,
    working: usize,
    attention: usize,
) {
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let px = s(RAIL_TEXT);
    let mut items: Vec<(Option<Rgb>, String)> = Vec::new();
    if working > 0 {
        items.push((Some(tok.working), format!("{working} working")));
    }
    if attention > 0 {
        items.push((Some(tok.attention), format!("{attention} needs you")));
    }
    items.push((None, "Hold Ctrl Shift for shortcuts".to_string()));
    let dot_w = s(RAIL_DOT) + s(6.0);
    let sep_w = s(18.0);
    let item_w = |item: &(Option<Rgb>, String)| {
        text_width(Face::Regular, px, &item.1) + if item.0.is_some() { dot_w } else { 0.0 }
    };
    let right = bar.right() as f32 - s(10.0);
    let room = right - left_limit as f32 - s(16.0);
    while !items.is_empty() {
        let total: f32 = items.iter().map(item_w).sum::<f32>() + sep_w * (items.len() - 1) as f32;
        if total <= room {
            break;
        }
        items.remove(0);
    }
    let total: f32 =
        items.iter().map(item_w).sum::<f32>() + sep_w * items.len().saturating_sub(1) as f32;
    let cy = bar.center_y();
    let mut x = right - total;
    for (index, (dot, text)) in items.iter().enumerate() {
        if index > 0 {
            fill_circle(buffer, stride, x + sep_w / 2.0, cy, s(1.3), tok.separator);
            x += sep_w;
        }
        if let Some(color) = dot {
            fill_circle(
                buffer,
                stride,
                x + s(RAIL_DOT) / 2.0,
                cy,
                s(RAIL_DOT) / 2.0,
                *color,
            );
            x += dot_w;
        }
        x = draw_text(
            buffer,
            stride,
            x,
            cy,
            Face::Regular,
            px,
            text,
            tok.muted,
            left_limit,
            bar.right(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scale(milli: u32) -> ChromeGeom {
        ChromeGeom {
            graphite: true,
            scale_milli: milli,
        }
    }

    fn tab(label: &str, selected: bool) -> TabText {
        TabText {
            label: label.into(),
            meta: None,
            dot: Dot::Idle,
            attention: false,
            selected,
        }
    }

    #[test]
    fn design_sizes_follow_the_window_scale() {
        assert_eq!(TABS_BAR_H.px(scale(1000)), 44);
        assert_eq!(TABS_BAR_H.px(scale(2000)), 88);
        assert_eq!(TABS_BAR_H.px(scale(1500)), 66);
    }

    #[test]
    fn token_text_pairs_meet_wcag_aa() {
        for tok in [&DARK, &LIGHT] {
            for (fg, bg) in [
                (tok.text, tok.bar),
                (tok.tab_text, tok.bar),
                (tok.muted, tok.bar),
                (tok.text_strong, tok.tab_active),
                (tok.muted, tok.tab_active),
                (tok.on_attention, tok.attention),
                (tok.muted, tok.field),
                (tok.key_text, tok.key),
            ] {
                assert!(contrast_ratio(fg, bg) >= 4.5, "{fg:?} on {bg:?}");
            }
        }
    }

    #[test]
    fn accent_keeps_three_to_one_on_both_bars() {
        for focus in [rgb(0x62a8ff), rgb(0xffd866), rgb(0x4cd18b), rgb(0xff6b6b)] {
            for tok in [&DARK, &LIGHT] {
                assert!(contrast_ratio(accent(tok, focus), tok.bar) >= 3.0);
            }
        }
        assert_eq!(accent(&DARK, rgb(0x62a8ff)), rgb(0x62a8ff));
    }

    #[test]
    fn chrome_font_measures_and_ellipsizes() {
        let wide = text_width(Face::Regular, 13.0, "release-pipeline-build-logs");
        assert!(wide > 100.0);
        let cut = ellipsize(Face::Regular, 13.0, "release-pipeline-build-logs", 60.0);
        assert!(cut.ends_with('…'));
        assert!(text_width(Face::Regular, 13.0, &cut) <= 60.0);
        assert_eq!(ellipsize(Face::Regular, 13.0, "grid", 60.0), "grid");
    }

    #[test]
    fn layout_and_hit_test_agree() {
        let tabs = vec![tab("grid", true), tab("codex", false), tab("muse", false)];
        let layout = bar_layout(scale(1000), 1440, 0, "release-lab", &tabs, "Ctrl Shift P");
        assert_eq!(layout.tabs.len(), 3);
        assert!(layout.command.is_some());
        let mid = |r: Rect| (r.x + r.w / 2, r.y + r.h / 2);
        let (x, y) = mid(layout.dropdown);
        assert_eq!(bar_hit(&layout, x, y, false), Some(StripHit::SpaceMenu));
        for (index, slot) in layout.tabs.iter().enumerate() {
            let (x, y) = (slot.chip.x + 4, slot.chip.y + slot.chip.h / 2);
            assert_eq!(
                bar_hit(&layout, x, y, false),
                Some(StripHit::Tab {
                    index,
                    close: false
                })
            );
            let (x, y) = mid(slot.close);
            assert_eq!(
                bar_hit(&layout, x, y, false),
                Some(StripHit::Tab { index, close: true })
            );
        }
        let (x, y) = mid(layout.plus);
        assert_eq!(bar_hit(&layout, x, y, false), Some(StripHit::NewTab));
        let (x, y) = mid(layout.command.unwrap());
        assert_eq!(bar_hit(&layout, x, y, false), Some(StripHit::Command));
        assert_eq!(bar_hit(&layout, 5, 60, false), None, "below the bar");
        // Chips never overlap and stay left of the command field.
        let field = layout.command.unwrap();
        for pair in layout.tabs.windows(2) {
            assert!(pair[0].chip.right() <= pair[1].chip.x);
        }
        assert!(layout.plus.right() <= field.x);
    }

    #[test]
    fn narrow_bar_drops_the_command_field_then_shortens_labels() {
        let tabs: Vec<_> = (0..6)
            .map(|i| tab(&format!("a-very-long-session-name-{i}"), i == 0))
            .collect();
        let wide = bar_layout(scale(1000), 2400, 0, "release-lab", &tabs, "Ctrl Shift P");
        assert!(wide.command.is_some());
        assert_eq!(wide.tabs.len(), 6);
        let narrow = bar_layout(scale(1000), 900, 0, "release-lab", &tabs, "Ctrl Shift P");
        assert!(narrow.command.is_none());
        assert!(narrow.tabs.iter().any(|slot| slot.label.ends_with('…')));
        assert!(narrow.plus.right() <= 900);
    }

    #[test]
    fn retina_layout_doubles_the_bar() {
        let tabs = vec![tab("grid", true)];
        let one = bar_layout(scale(1000), 1440, 0, "lab", &tabs, "Ctrl Shift P");
        let two = bar_layout(scale(2000), 2880, 0, "lab", &tabs, "Ctrl Shift P");
        assert_eq!(two.bar.h, one.bar.h * 2);
        assert!(two.tabs[0].chip.w > one.tabs[0].chip.w * 2 - 4);
    }

    #[test]
    fn pane_chrome_rings_the_focused_pane_and_outlines_the_rest() {
        let (w, h) = (300usize, 200usize);
        let slot = Rect::new(20, 20, 260, 160);
        let accent = rgb(0x5aa2ff);
        let paint = |focused: bool| {
            let mut buffer = vec![pack_argb(0xff, DARK.ground); w * h];
            paint_pane_surface(
                &mut buffer,
                w,
                scale(1000),
                slot,
                DARK.ground,
                0xff,
                rgb(0x121214),
                0xff,
            );
            paint_pane_chrome(
                &mut buffer,
                w,
                scale(1000),
                &DARK,
                accent,
                slot,
                rgb(0x121214),
                &PaneHeader {
                    name: "notes",
                    meta: Some("release-lab"),
                    dot: Dot::Working,
                    status: PaneStatus::decide(false, 0, false, false, focused),
                    focused,
                },
                true,
            );
            buffer
        };
        let top_mid = slot.y * w + slot.x + slot.w / 2;
        let focused = paint(true);
        assert_eq!(
            unpack_rgb(focused[top_mid]),
            accent,
            "ring on the slot edge"
        );
        assert_eq!(
            unpack_rgb(focused[top_mid - w]),
            accent,
            "and one pixel outside"
        );
        // Title row carries the focus tint just inside the ring.
        assert_eq!(
            unpack_rgb(focused[(slot.y + 4) * w + slot.x + slot.w - 4]),
            DARK.title_focus
        );
        let quiet = paint(false);
        assert_eq!(unpack_rgb(quiet[top_mid]), DARK.hairline);
        assert_eq!(
            unpack_rgb(quiet[top_mid - w]),
            DARK.ground,
            "no ring outside"
        );
        // The corner pixel stays ground: the card is rounded.
        assert_eq!(unpack_rgb(quiet[slot.y * w + slot.x]), DARK.ground);
    }

    #[test]
    fn paint_draws_the_active_underline_in_the_accent() {
        let tabs = vec![tab("grid", true), tab("codex", false)];
        let layout = bar_layout(scale(1000), 800, 0, "lab", &tabs, "");
        let mut buffer = vec![0u32; 800 * layout.bar.h];
        let accent = rgb(0x5aa2ff);
        paint_tabs_bar(
            &mut buffer,
            800,
            &BarPaint {
                layout: &layout,
                tok: &DARK,
                accent,
                hover: None,
                bar_alpha: 0xff,
                editing: None,
            },
        );
        let chip = layout.tabs[0].chip;
        let y = layout.bar.h - 1;
        let x = chip.x + chip.w / 2;
        assert_eq!(unpack_rgb(buffer[y * 800 + x]), accent);
        // Bar ground in the gap left of the first chip.
        assert_eq!(unpack_rgb(buffer[2 * 800 + 1]), DARK.bar);
    }
}
