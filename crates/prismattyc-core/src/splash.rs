//! Shared launch-splash content for the `prismattyc` CLI (ANSI, host terminal)
//! and `prismattyc-host` (rasterized, in-window). One source so the art, tips,
//! and brand palette cannot drift between the two front-ends.

/// Sub-pages reachable from the splash menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    WhatsNew,
    Docs,
    Changelog,
    Walkthrough,
}

pub const ART: &[&str] = &[
    r"██████╗ ██████╗ ██╗███████╗███╗   ███╗ █████╗ ████████╗████████╗██╗   ██╗ ██████╗",
    r"██╔══██╗██╔══██╗██║██╔════╝████╗ ████║██╔══██╗╚══██╔══╝╚══██╔══╝╚██╗ ██╔╝██╔════╝",
    r"██████╔╝██████╔╝██║███████╗██╔████╔██║███████║   ██║      ██║    ╚████╔╝ ██║",
    r"██╔═══╝ ██╔══██╗██║╚════██║██║╚██╔╝██║██╔══██║   ██║      ██║     ╚██╔╝  ██║",
    r"██║     ██║  ██║██║███████║██║ ╚═╝ ██║██║  ██║   ██║      ██║      ██║   ╚██████╗",
    r"╚═╝     ╚═╝  ╚═╝╚═╝╚══════╝╚═╝     ╚═╝╚═╝  ╚═╝   ╚═╝      ╚═╝      ╚═╝    ╚═════╝",
];

pub const TAGLINE: &str = "classic terminal, modern surface";

/// Brand "Continuous beam" facet colors — the focus-border rainbow
/// (`assets/brand/README.md`). Keep in sync with `FOCUS_BORDER_PALETTE`
/// in prismattyc-host; duplicated here so the prismattyc binary doesn't depend
/// on the host crate for seven RGB values. Ink (#D0D0D0) is the letter fill.
pub const SPECTRUM: &[[u8; 3]] = &[
    [0xff, 0x6e, 0x63], // coral
    [0xff, 0xb4, 0x54], // amber
    [0xff, 0xe0, 0x66], // yellow
    [0x7b, 0xd8, 0x8f], // green
    [0x62, 0xa8, 0xff], // blue
    [0x7b, 0x8c, 0xfa], // indigo
    [0x9b, 0x8c, 0xf5], // violet
];
pub const INK: [u8; 3] = [0xd0, 0xd0, 0xd0];
/// Hyperlink text: the spectrum blue, distinct from both ink and dim.
pub const LINK: [u8; 3] = [0x62, 0xa8, 0xff];

pub const TIPS: &[&str] = &[
    "pmux new work -- bash  starts a named session you can reattach to.",
    "pmux attach work  jumps back into a running session.",
    "pmux mail send operator-id --summary hi  mails an agent's mailbox.",
    "prismattyc update --all  pulls main and reinstalls the Prismattyc binaries.",
    "Selections copy over OSC52 — even through SSH.",
    "prismattyc --no-splash skips this screen; PRISMATTYC_NO_SPLASH=1 makes it stick.",
    #[cfg(target_os = "macos")]
    "Ctrl+Alt+3 splits into 3 even columns. Ctrl+Alt+2…9 for other layouts.",
    #[cfg(not(target_os = "macos"))]
    "Ctrl+Shift+F3 splits into 3 even columns. Ctrl+Shift+F2…F9 for other layouts.",
    #[cfg(target_os = "macos")]
    "Ctrl+Alt+4 splits into 2×2 quadrants (macOS). On Linux: Ctrl+Shift+F4.",
    #[cfg(not(target_os = "macos"))]
    "Ctrl+Shift+F4 splits into 2×2 quadrants.",
    "Ctrl+Shift+\\ horizontal split; Ctrl+Shift+- vertical split.",
    "Alt+Arrow moves focus between panes.",
    "Ctrl+Shift+W closes the focused pane.",
    "Ctrl+Shift+T new tab; Ctrl+Shift+1…9 select tabs.",
];

pub const WHATS_NEW: &[&str] = &[
    "Prismattyc: Switchboard mail is folded into the mux — pmux mail is the CLI.",
    "Agents get mailboxes: pmux new --headless NAME --agent NAME, then pmux mail watch.",
    "macOS support: the mux server, death-watch, and supervisor run on Darwin.",
];

pub const REPO_URL: &str = "https://github.com/brandanmajeske/Prismattyc";

/// Pick the day's tip deterministically (no rand dependency).
#[must_use]
pub fn tip_index(day_of_year: u32) -> usize {
    (day_of_year as usize) % TIPS.len()
}

/// Days since the epoch, modulo 366 — the tip-rotation seed.
///
/// **Not wasm-safe.** `SystemTime::now()` panics on `wasm32-unknown-unknown`
/// (no clock without `js-sys`), so `prismattyc-labs` and any other browser
/// surface must not call this or [`tip_index`] with it. Pass a day from the
/// host instead.
pub fn day_of_year() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    // Good enough for tip rotation; exact calendar day is irrelevant.
    (days % 366) as u32
}

/// Art glyph color: solid blocks in brand ink, outline strokes banded
/// left-to-right across the spectrum in swatch order. `index` is the
/// character column and `width` the line's character count.
#[must_use]
pub fn art_glyph_rgb(ch: char, index: usize, width: usize) -> [u8; 3] {
    match ch {
        '█' => INK,
        _ => {
            let band = index * SPECTRUM.len() / width.max(1);
            SPECTRUM[band.min(SPECTRUM.len() - 1)]
        }
    }
}

// ---------------------------------------------------------------------------
// Attract-mode animation. Pure functions of elapsed wall time so the CLI
// (ANSI) and the host (rasterized) front-ends render the same frame for the
// same millisecond, and tests can pin any instant.
//
// Timeline (ms since the splash appeared):
//   0 .. INTRO_MS            beam sweep: a white head crosses the art on a
//                            diagonal, lighting letters as it passes and
//                            leaving the spectrum behind it.
//   then, repeating          hold · reflection left→right · hold ·
//                            reflection right→left.
// Two lens flares sit just outside the art, behind the top-left corner of
// the first letter and the lower-right corner of the last. They twinkle
// during holds and flash when the beam or a reflection reaches them. Flare
// glyphs (star, rays, streak) are drawn only in the margins; inside the art
// a flare only tints, so no letter stroke is ever replaced.
// ---------------------------------------------------------------------------

/// One art cell: glyph and colour. Spaces are never painted.
pub type ArtCell = (char, [u8; 3]);

/// Beam-sweep duration.
pub const INTRO_MS: u64 = 1100;
/// One reflection pass (edge to edge).
pub const REFLECT_MS: u64 = 1400;
/// Rest between passes ("pause a few seconds").
pub const HOLD_MS: u64 = 2600;
/// Frame period the front-ends tick at while something sweeps (~30 fps).
pub const FRAME_MS: u64 = 33;
/// Frame period during a hold: only the flare twinkle moves.
pub const HOLD_FRAME_MS: u64 = 80;
/// Blank columns left of the art: room for the top-left flare's streak and
/// rays. Front-ends indent the text lines below the art by the same amount
/// so they stay aligned with the first letter. Equal to [`ART_MARGIN`] so
/// both streaks run out the same distance (PT-88); the beam's start column
/// is [`BEAM_X_MIN`], not this margin.
pub const ART_MARGIN_LEFT: usize = 8;
/// Blank columns right of the art: room for the lower-right flare's streak.
pub const ART_MARGIN: usize = 8;
/// Slanted column the beam and reflection band start from, left of the
/// art. Fixed independently of [`ART_MARGIN_LEFT`] so a wider margin does
/// not slow the intro or shift the flare flash instants.
const BEAM_X_MIN: f32 = -4.0;

/// Full attract loop period after the intro.
pub const LOOP_MS: u64 = 2 * (HOLD_MS + REFLECT_MS);

/// Sparkle glyph at a flare centre. In DejaVu Sans Mono and Noto Sans
/// Symbols 2, both bundled with the host as fallbacks.
pub const FLARE_GLYPH: char = '✦';
/// Sparkle glyph while a flare flashes (twelve-point star, same fonts).
pub const FLARE_PEAK_GLYPH: char = '✹';
/// Horizontal ray glyph for a flare streak where it crosses empty cells.
pub const STREAK_GLYPH: char = '─';
/// Heavy ray glyph for the streak cells next to a flare centre.
pub const STREAK_CORE_GLYPH: char = '━';
/// Diagonal ray glyph: down-left from the top-left flare, up-right from the
/// lower-right one. Both directions are the same stroke.
pub const RAY_GLYPH: char = '╱';

const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
/// Cool white the flares tint toward (a touch of the spectrum blue).
const FLARE_WHITE: [u8; 3] = [0xf2, 0xf7, 0xff];
/// Flare core colour at rest; blends to white as it flashes.
const FLARE_TINT: [u8; 3] = [0xa8, 0xd0, 0xff];
/// Darkest a streak ray gets before it fades out entirely.
const STREAK_DARK: [u8; 3] = [0x1c, 0x22, 0x2e];

/// Diagonal slant of the beam and reflection fronts: columns per row. Cells
/// are ~2:1, so 1.5 columns per row reads as a ~50° chrome-logo diagonal.
const SLANT: f32 = 1.5;
/// Width of the beam head's white-to-spectrum tail, in slanted columns.
const HEAD_W: f32 = 7.0;
/// Reflection band half-width (halo), in slanted columns.
const BAND_HALF: f32 = 8.0;
/// Reflection bright core half-width.
const CORE_HALF: f32 = 2.2;
/// How long the intro flash on a flare decays.
const FLASH_MS: f32 = 700.0;
/// Twinkle period during holds.
const TWINKLE_MS: f32 = 900.0;
/// Reflection distance (slanted columns) over which a flare brightens as
/// the band approaches it.
const FLARE_REACH: f32 = 12.0;
/// Flare glow radius in rows (columns count half: cells are ~2:1).
const GLOW_RADIUS: f32 = 2.8;

fn lerp_rgb(a: [u8; 3], b: [u8; 3], k: f32) -> [u8; 3] {
    let k = k.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

fn smoothstep(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    k * k * (3.0 - 2.0 * k)
}

/// One lens flare anchored just outside a corner of the art.
struct Flare {
    row: usize,
    /// Art column (0 = first column of `ART`); negative in the left margin.
    col: i64,
    /// Slanted coordinate, for beam/reflection arrival.
    x: f32,
    /// Row step and column step of its diagonal rays.
    ray: (i64, i64),
    /// Twinkle phase offset so the two flares do not blink together.
    phase: f32,
}

/// Art geometry: unpadded width, row count, slanted extent, flares.
struct Geometry {
    width: usize,
    rows: usize,
    /// Slanted coordinate the beam starts from ([`BEAM_X_MIN`]).
    x_min: f32,
    /// Largest slanted coordinate any cell can have (right margin, last row).
    x_max: f32,
    flares: [Flare; 2],
}

fn geometry() -> Geometry {
    let width = ART.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let rows = ART.len();
    let last_row = rows.saturating_sub(1);
    let first_col = ART
        .first()
        .and_then(|l| l.chars().position(|c| c != ' '))
        .unwrap_or(0);
    // Lower-right anchor: the last row that carries solid blocks (the
    // bottom of the white letters), not the outline-only row under it.
    let block_row = ART
        .iter()
        .rposition(|l| l.contains('█'))
        .unwrap_or(last_row);
    let last_col = ART
        .get(block_row)
        .map(|l| {
            let chars: Vec<char> = l.chars().collect();
            chars.iter().rposition(|c| *c != ' ').unwrap_or(0)
        })
        .unwrap_or(0);
    // Centres sit in the margin cell directly beside each block corner.
    let tl = first_col as i64 - 1;
    let br = last_col as i64 + 1;
    let flares = [
        Flare {
            row: 0,
            col: tl,
            x: tl as f32,
            ray: (1, -1),
            phase: 0.0,
        },
        Flare {
            row: block_row,
            col: br,
            x: br as f32 + block_row as f32 * SLANT,
            ray: (-1, 1),
            phase: 0.5,
        },
    ];
    Geometry {
        width,
        rows,
        x_min: BEAM_X_MIN,
        x_max: (width + ART_MARGIN) as f32 + last_row as f32 * SLANT,
        flares,
    }
}

/// Where the reflection band centre is at `elapsed_ms`, in slanted columns,
/// or None during a hold. Left→right first, then right→left.
fn reflection_center(geo: &Geometry, elapsed_ms: u64) -> Option<f32> {
    if elapsed_ms < INTRO_MS {
        return None;
    }
    let u = (elapsed_ms - INTRO_MS) % LOOP_MS;
    let leg = HOLD_MS + REFLECT_MS;
    let (lo, hi) = (geo.x_min - BAND_HALF, geo.x_max + BAND_HALF);
    let (start, end, t) = if u < HOLD_MS {
        return None;
    } else if u < leg {
        (lo, hi, u - HOLD_MS)
    } else if u < leg + HOLD_MS {
        return None;
    } else {
        (hi, lo, u - leg - HOLD_MS)
    };
    let q = t as f32 / REFLECT_MS as f32;
    Some(start + (end - start) * q)
}

/// Slanted distance the beam head travels during the intro.
fn beam_span(geo: &Geometry) -> f32 {
    geo.x_max - geo.x_min + HEAD_W + 2.0
}

/// Beam head position during the intro, in slanted columns. Cells ahead of
/// the head are unlit; the head runs past the far edge so the tail clears.
fn beam_head(geo: &Geometry, elapsed_ms: u64) -> f32 {
    let p = (elapsed_ms as f32 / INTRO_MS as f32).clamp(0.0, 1.0);
    geo.x_min - 1.0 + p * beam_span(geo)
}

/// Brightness 0..1 of one flare at `elapsed_ms`: intro flash when the beam
/// reaches it, then twinkle plus a surge whenever a reflection pass comes
/// near.
fn flare_intensity(geo: &Geometry, flare: &Flare, elapsed_ms: u64) -> f32 {
    // Instant the beam head crosses the flare cell.
    let t_pass = INTRO_MS as f32 * (flare.x - geo.x_min + 1.0) / beam_span(geo);
    let t = elapsed_ms as f32;
    if elapsed_ms < INTRO_MS && beam_head(geo, elapsed_ms) < flare.x {
        return 0.0;
    }
    let flash = (1.0 - (t - t_pass) / FLASH_MS).clamp(0.0, 1.0);
    let twinkle = 0.32 + 0.14 * ((t / TWINKLE_MS + flare.phase) * std::f32::consts::TAU).sin();
    let surge = reflection_center(geo, elapsed_ms)
        .map(|c| (1.0 - (c - flare.x).abs() / FLARE_REACH).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let live = twinkle + surge * (1.0 - twinkle);
    live.max(flash).clamp(0.0, 1.0)
}

/// Total frame width: left margin, art, right margin.
#[must_use]
pub fn art_frame_width() -> usize {
    ART_MARGIN_LEFT + ART.iter().map(|l| l.chars().count()).max().unwrap_or(0) + ART_MARGIN
}

/// Render the word art at `elapsed_ms` since the splash appeared. Every row
/// is [`ART_MARGIN_LEFT`] + `ART` width + [`ART_MARGIN`] cells; spaces carry
/// [`INK`] and are never painted.
#[must_use]
pub fn art_frame(elapsed_ms: u64) -> Vec<Vec<ArtCell>> {
    let geo = geometry();
    let total_w = ART_MARGIN_LEFT + geo.width + ART_MARGIN;
    let head = if elapsed_ms < INTRO_MS {
        Some(beam_head(&geo, elapsed_ms))
    } else {
        None
    };
    let band = reflection_center(&geo, elapsed_ms);
    let flares: Vec<(&Flare, f32)> = geo
        .flares
        .iter()
        .map(|f| (f, flare_intensity(&geo, f, elapsed_ms)))
        .collect();

    let mut rows = Vec::with_capacity(geo.rows);
    for (r, line) in ART.iter().enumerate() {
        let mut cells: Vec<ArtCell> = Vec::with_capacity(total_w);
        let chars: Vec<char> = line.chars().collect();
        for fc in 0..total_w {
            // Art column; negative inside the left margin.
            let ca = fc as i64 - ART_MARGIN_LEFT as i64;
            let ch = if ca >= 0 {
                chars.get(ca as usize).copied().unwrap_or(' ')
            } else {
                ' '
            };
            let x = ca as f32 + r as f32 * SLANT;
            let in_margin = ca < 0 || ca >= geo.width as i64;

            // Intro: hidden ahead of the beam head.
            if let Some(h) = head {
                if x > h {
                    cells.push((' ', INK));
                    continue;
                }
            }

            // Flare contributions: centre, ray, streak, glow.
            let mut center: Option<f32> = None;
            let mut ray: Option<f32> = None;
            let mut streak: f32 = 0.0;
            let mut streak_core = false;
            let mut tint: f32 = 0.0;
            for (flare, f) in &flares {
                let f = *f;
                let dx = ca - flare.col;
                let dy = r as i64 - flare.row as i64;
                if dx == 0 && dy == 0 {
                    center = Some(f);
                    continue;
                }
                let ray_len = if f >= 0.7 {
                    3
                } else if f >= 0.35 {
                    2
                } else {
                    1
                };
                for k in 1..=ray_len {
                    if dy == k * flare.ray.0 && dx == k * flare.ray.1 && ch == ' ' {
                        let strength = f * (1.0 - k as f32 / (ray_len as f32 + 1.0));
                        ray = Some(ray.map_or(strength, |s: f32| s.max(strength)));
                    }
                }
                let dist = ((dx as f32 * 0.5).powi(2) + (dy as f32).powi(2)).sqrt();
                let glow = f * (1.0 - dist / GLOW_RADIUS).max(0.0);
                let mut s = 0.0;
                if dy == 0 {
                    // Fade to nothing inside the margin the streak runs
                    // into, so the tail never ends in a hard cut at the
                    // frame edge (PT-58): `room` is the cell count between
                    // the flare and that edge, and the edge cell itself
                    // stays blank.
                    let room = if dx < 0 {
                        flare.col + ART_MARGIN_LEFT as i64
                    } else {
                        (geo.width + ART_MARGIN) as i64 - 1 - flare.col
                    };
                    let len = (3.0 + 16.0 * f).min(room.max(1) as f32);
                    s = f * (1.0 - (dx as f32).abs() / len).max(0.0);
                    if s > streak {
                        streak = s;
                        streak_core = dx.abs() <= 2 && f >= 0.5;
                    }
                }
                tint = tint.max(glow + s * 0.8);
            }

            // Flare glyphs live in the margins only; inside the art the
            // flare is a tint, so letter strokes stay intact.
            let (glyph, mut rgb) = if let (Some(f), true) = (center, in_margin) {
                let glyph = if f >= 0.7 {
                    FLARE_PEAK_GLYPH
                } else {
                    FLARE_GLYPH
                };
                (glyph, lerp_rgb(FLARE_TINT, WHITE, f))
            } else if let (Some(k), true) = (ray, in_margin) {
                (RAY_GLYPH, lerp_rgb(STREAK_DARK, FLARE_TINT, k))
            } else if ch == ' ' {
                if streak > 0.10 && in_margin {
                    let glyph = if streak_core {
                        STREAK_CORE_GLYPH
                    } else {
                        STREAK_GLYPH
                    };
                    (glyph, lerp_rgb(STREAK_DARK, FLARE_TINT, streak))
                } else {
                    cells.push((' ', INK));
                    continue;
                }
            } else {
                (ch, art_glyph_rgb(ch, ca as usize, geo.width))
            };

            // Beam tail: white at the head, fading to the base colour.
            if let Some(h) = head {
                let d = h - x;
                if d < HEAD_W {
                    // Hot core: the leading 1.5 columns are pure white.
                    rgb = lerp_rgb(rgb, WHITE, (HEAD_W - d) / (HEAD_W - 1.5));
                }
            }
            // Reflection: bright core inside a softer halo.
            if let Some(c) = band {
                let d = (x - c).abs();
                let halo = smoothstep(1.0 - d / BAND_HALF) * 0.5;
                let core = smoothstep(1.0 - d / CORE_HALF);
                rgb = lerp_rgb(rgb, WHITE, (halo + core).min(0.95));
            }
            // Flare glow and streak tint everything nearby.
            // Outline strokes take only a little tint, so their spectrum
            // colour stays readable next to a flare.
            if center.is_none() {
                let amount = if ch == '█' || in_margin {
                    tint
                } else {
                    tint * 0.35
                };
                rgb = lerp_rgb(rgb, FLARE_WHITE, amount);
            }
            cells.push((glyph, rgb));
        }
        rows.push(cells);
    }
    rows
}

/// The word art with no motion: plain ink and spectrum bands, no flares,
/// padded to the same width as [`art_frame`] so layouts line up. Used when
/// the splash animation is turned off.
#[must_use]
pub fn art_static() -> Vec<Vec<ArtCell>> {
    let width = ART.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let total_w = ART_MARGIN_LEFT + width + ART_MARGIN;
    ART.iter()
        .map(|line| {
            let chars: Vec<char> = line.chars().collect();
            (0..total_w)
                .map(|fc| {
                    let ch = fc
                        .checked_sub(ART_MARGIN_LEFT)
                        .and_then(|c| chars.get(c).copied())
                        .unwrap_or(' ');
                    if ch == ' ' {
                        (' ', INK)
                    } else {
                        (ch, art_glyph_rgb(ch, fc - ART_MARGIN_LEFT, width))
                    }
                })
                .collect()
        })
        .collect()
}

/// Brightness 0..1 of each flare at `elapsed_ms`, top-left first, then
/// lower-right. The host uses it to size and colour pixel-drawn rays.
#[must_use]
pub fn flare_intensities(elapsed_ms: u64) -> [f32; 2] {
    let geo = geometry();
    [
        flare_intensity(&geo, &geo.flares[0], elapsed_ms),
        flare_intensity(&geo, &geo.flares[1], elapsed_ms),
    ]
}

/// True once the intro has finished and the animation is in a hold: only
/// the outline scroll and flare twinkle move. Front-ends use this to lower
/// the frame rate between passes.
#[must_use]
pub fn in_hold(elapsed_ms: u64) -> bool {
    elapsed_ms >= INTRO_MS && reflection_center(&geometry(), elapsed_ms).is_none()
}

/// The first hold after the intro: the art at rest, useful for tests that
/// want the settled colours.
pub const SETTLED_MS: u64 = INTRO_MS + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_index_rotates_and_stays_in_bounds() {
        for day in 0..1000 {
            assert!(tip_index(day) < TIPS.len());
        }
        assert_eq!(tip_index(0), 0);
        assert_eq!(tip_index(TIPS.len() as u32), 0);
    }

    #[test]
    fn art_glyph_colors_split_ink_and_spectrum() {
        assert_eq!(art_glyph_rgb('█', 0, 40), INK);
        // First outline column is coral, last is violet.
        assert_eq!(art_glyph_rgb('╗', 0, 40), SPECTRUM[0]);
        assert_eq!(art_glyph_rgb('║', 39, 40), SPECTRUM[6]);
    }

    fn flat(frame: &[Vec<ArtCell>]) -> String {
        frame
            .iter()
            .map(|row| row.iter().map(|(c, _)| *c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const L: usize = ART_MARGIN_LEFT;

    #[test]
    fn frame_rows_are_uniform_width_with_margins() {
        let width = ART.iter().map(|l| l.chars().count()).max().unwrap();
        let frame = art_frame(SETTLED_MS);
        assert_eq!(frame.len(), ART.len());
        for row in &frame {
            assert_eq!(row.len(), L + width + ART_MARGIN);
        }
        assert_eq!(art_frame_width(), L + width + ART_MARGIN);
    }

    #[test]
    fn intro_starts_dark_and_ends_fully_lit() {
        let dark = art_frame(0);
        assert!(dark.iter().flatten().all(|(c, _)| *c == ' '));
        let lit = flat(&art_frame(SETTLED_MS));
        for (row, line) in ART.iter().enumerate() {
            let rendered: String = lit
                .lines()
                .nth(row)
                .unwrap()
                .chars()
                .skip(L)
                .take(line.chars().count())
                .collect();
            // Every original glyph is present, unchanged: flare glyphs never
            // land inside the art.
            for (i, (want, got)) in line.chars().zip(rendered.chars()).enumerate() {
                assert_eq!(want, got, "row {row} col {i}");
            }
        }
    }

    #[test]
    fn intro_head_is_white_and_lights_left_to_right() {
        let mid = art_frame(INTRO_MS / 2);
        let art0: Vec<char> = ART[0].chars().collect();
        let row0 = &mid[0];
        // Every lit column is left of every column the beam has not reached
        // (a stroke in ART rendered as blank).
        let last_lit = row0.iter().rposition(|(c, _)| *c != ' ').unwrap();
        let first_hidden = art0
            .iter()
            .enumerate()
            .position(|(i, want)| *want != ' ' && row0[L + i].0 == ' ')
            .map(|i| L + i)
            .unwrap();
        assert!(
            last_lit < first_hidden,
            "lit {last_lit} hidden {first_hidden}"
        );
        assert!(last_lit > L && first_hidden < L + art0.len());
        // Somewhere near the head a cell is pure white.
        assert!(mid.iter().flatten().any(|(_, rgb)| *rgb == WHITE));
    }

    #[test]
    fn settled_frame_keeps_ink_and_full_spectrum() {
        let frame = art_frame(SETTLED_MS);
        assert!(frame
            .iter()
            .flatten()
            .any(|(c, rgb)| *c == '█' && *rgb == INK));
        for swatch in SPECTRUM {
            assert!(
                frame.iter().flatten().any(|(_, rgb)| rgb == swatch),
                "missing spectrum color {swatch:?}"
            );
        }
        assert!(in_hold(SETTLED_MS));
    }

    #[test]
    fn streaks_taper_and_stop_short_of_the_frame_edge() {
        // Walk outward from each flare along its streak row: brightness
        // never rises, and the streak ends before the outermost margin
        // cell, so a narrow margin cannot cut the tail (PT-58).
        let block_row = ART.iter().rposition(|l| l.contains('█')).unwrap();
        let last_col = ART[block_row]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| *c != ' ')
            .unwrap();
        let br = L + last_col + 1;
        let width = art_frame_width();
        let is_streak = |c: char| c == STREAK_GLYPH || c == STREAK_CORE_GLYPH;
        let lum = |rgb: [u8; 3]| u32::from(rgb[0]) + u32::from(rgb[1]) + u32::from(rgb[2]);
        for ms in [
            60,
            INTRO_MS - 20,
            SETTLED_MS,
            SETTLED_MS + FLASH_MS as u64 + 50,
        ] {
            let frame = art_frame(ms);
            let left: Vec<&ArtCell> = frame[0][..L - 1].iter().rev().collect();
            let right: Vec<&ArtCell> = frame[block_row][br + 1..width].iter().collect();
            for (side, cells) in [("top-left", left), ("lower-right", right)] {
                let run: Vec<u32> = cells
                    .iter()
                    .take_while(|c| is_streak(c.0))
                    .map(|c| lum(c.1))
                    .collect();
                assert!(
                    run.len() < cells.len(),
                    "{ms}ms {side}: streak reaches the frame edge: {:?}",
                    cells.iter().map(|c| c.0).collect::<String>()
                );
                assert!(
                    run.windows(2).all(|w| w[0] >= w[1]),
                    "{ms}ms {side}: streak brightens outward: {run:?}"
                );
            }
        }
        // Peak flash still leaves a visible streak on both sides.
        assert!(is_streak(art_frame(60)[0][L - 2].0));
        assert!(is_streak(art_frame(INTRO_MS - 20)[block_row][br + 1].0));
    }

    #[test]
    fn streaks_run_out_equally_on_both_sides_at_full_flash() {
        // Equal margins give both streaks the same room, so at each flare's
        // peak flash the two runs are the same length (PT-88).
        assert_eq!(ART_MARGIN_LEFT, ART_MARGIN);
        let block_row = ART.iter().rposition(|l| l.contains('█')).unwrap();
        let last_col = ART[block_row]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| *c != ' ')
            .unwrap();
        let br = L + last_col + 1;
        let is_streak = |c: &ArtCell| c.0 == STREAK_GLYPH || c.0 == STREAK_CORE_GLYPH;
        let left = art_frame(60)[0][..L - 1]
            .iter()
            .rev()
            .take_while(|c| is_streak(c))
            .count();
        let right = art_frame(INTRO_MS - 20)[block_row][br + 1..]
            .iter()
            .take_while(|c| is_streak(c))
            .count();
        assert_eq!(left, right, "left {left} right {right}");
        assert!(left >= 4, "peak streak is short: {left}");
        assert!(left < L - 1, "streak reaches the frame edge");
    }

    #[test]
    fn beam_start_is_decoupled_from_the_left_margin() {
        // The intro sweep and the flash instants depend on x_min, which is
        // fixed at BEAM_X_MIN rather than the (wider) left margin.
        let geo = geometry();
        assert_eq!(geo.x_min, BEAM_X_MIN);
        assert!(geo.x_min > -(ART_MARGIN_LEFT as f32));
        // The top-left flare flashes within the first 60 ms, as before the
        // margin widened; the lower-right one only once the beam arrives.
        let [tl, br] = flare_intensities(60);
        assert!(tl > 0.9 && br == 0.0, "tl {tl} br {br}");
        assert!(flare_intensities(INTRO_MS - 20)[1] > 0.7);
    }

    #[test]
    fn flares_sit_on_opposite_corners_with_rays_into_margins() {
        let rest = art_frame(SETTLED_MS + FLASH_MS as u64 + 50);
        // Lower-right anchor row: the bottom row of the block letters.
        let last_row = ART.iter().rposition(|l| l.contains('█')).unwrap();
        assert_eq!(last_row, ART.len() - 2);
        let last_col = ART[last_row]
            .chars()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|c| *c != ' ')
            .unwrap();
        // Top-left: the margin cell behind the P's corner; rays run down-left.
        assert_eq!(rest[0][L - 1].0, FLARE_GLYPH);
        assert_eq!(rest[0][L].0, ART[0].chars().next().unwrap());
        assert_eq!(rest[1][L - 2].0, RAY_GLYPH);
        assert!(rest[0][..L - 1]
            .iter()
            .any(|(c, _)| *c == STREAK_GLYPH || *c == STREAK_CORE_GLYPH));
        // Lower-right: the margin cell beside the C's bottom block corner;
        // rays run up-right. The outline-only row below carries nothing.
        let br = L + last_col + 1;
        assert_eq!(rest[last_row][br].0, FLARE_GLYPH);
        assert_eq!(rest[last_row - 1][br + 1].0, RAY_GLYPH);
        assert!(rest[last_row][br + 1..]
            .iter()
            .any(|(c, _)| *c == STREAK_GLYPH || *c == STREAK_CORE_GLYPH));
        assert_eq!(rest[last_row + 1][br].0, ' ');
        // The top-left flare flashes as the beam starts; the lower-right one
        // as it finishes. Twinkle keeps changing brightness during a hold.
        assert_eq!(art_frame(60)[0][L - 1].0, FLARE_PEAK_GLYPH);
        assert_eq!(art_frame(INTRO_MS - 20)[last_row][br].0, FLARE_PEAK_GLYPH);
        let a = art_frame(SETTLED_MS + 1500)[0][L - 1].1;
        let b = art_frame(SETTLED_MS + 1500 + TWINKLE_MS as u64 / 2)[0][L - 1].1;
        assert_ne!(a, b);
        // Nothing before the beam reaches the far corner.
        assert_eq!(art_frame(0)[last_row][br].0, ' ');
        // A flaring flare never draws a glyph inside the art's columns.
        let flash = art_frame(60);
        for (r, line) in ART.iter().enumerate() {
            for (i, want) in line.chars().enumerate() {
                let got = flash[r][L + i].0;
                assert!(got == want || got == ' ', "row {r} col {i}: {got:?}");
            }
        }
    }

    #[test]
    fn static_art_is_plain_and_same_width_as_frames() {
        let plain = art_static();
        let frame = art_frame(SETTLED_MS);
        assert_eq!(plain.len(), frame.len());
        for (row, line) in ART.iter().enumerate() {
            assert_eq!(plain[row].len(), frame[row].len());
            let text: String = plain[row].iter().map(|(c, _)| *c).collect();
            assert_eq!(
                text.trim_end(),
                format!("{}{}", " ".repeat(L), line).trim_end()
            );
        }
        // No flare glyphs anywhere; ink and every swatch present.
        assert!(!plain.iter().flatten().any(|(c, _)| matches!(
            *c,
            FLARE_GLYPH | FLARE_PEAK_GLYPH | RAY_GLYPH | STREAK_GLYPH | STREAK_CORE_GLYPH
        )));
        assert!(plain
            .iter()
            .flatten()
            .any(|(c, rgb)| *c == '█' && *rgb == INK));
        for swatch in SPECTRUM {
            assert!(plain.iter().flatten().any(|(_, rgb)| rgb == swatch));
        }
    }

    #[test]
    fn flare_intensities_follow_the_beam() {
        let [tl, br] = flare_intensities(0);
        assert_eq!((tl, br), (0.0, 0.0));
        let [tl, br] = flare_intensities(60);
        assert!(tl > 0.9 && br == 0.0, "tl {tl} br {br}");
        let [tl, br] = flare_intensities(SETTLED_MS + 2000);
        assert!(tl > 0.1 && br > 0.1 && tl < 0.7 && br < 0.7);
    }

    #[test]
    fn reflection_passes_left_to_right_then_back() {
        let geo = geometry();
        let mid_pass = INTRO_MS + HOLD_MS + REFLECT_MS / 2;
        let early = reflection_center(&geo, INTRO_MS + HOLD_MS + REFLECT_MS / 10).unwrap();
        let late = reflection_center(&geo, mid_pass).unwrap();
        assert!(late > early, "first pass moves right");
        let back_start = INTRO_MS + 2 * HOLD_MS + REFLECT_MS;
        let b_early = reflection_center(&geo, back_start + REFLECT_MS / 10).unwrap();
        let b_late = reflection_center(&geo, back_start + REFLECT_MS / 2).unwrap();
        assert!(b_late < b_early, "second pass moves left");
        assert!(reflection_center(&geo, INTRO_MS + HOLD_MS / 2).is_none());
        assert!(reflection_center(&geo, back_start - HOLD_MS / 2).is_none());
        assert!(!in_hold(mid_pass));
        // The loop repeats.
        assert_eq!(
            reflection_center(&geo, mid_pass),
            reflection_center(&geo, mid_pass + LOOP_MS)
        );
        // The band whitens the cells it crosses.
        let frame = art_frame(mid_pass);
        assert!(frame
            .iter()
            .flatten()
            .any(|(c, rgb)| *c == '█' && rgb.iter().all(|v| *v > 0xe8)));
    }

    #[test]
    fn flares_surge_when_reflection_arrives() {
        let geo = geometry();
        let lo = geo.x_min - BAND_HALF;
        let hi = geo.x_max + BAND_HALF;
        for flare in &geo.flares {
            let rest = flare_intensity(&geo, flare, SETTLED_MS + FLASH_MS as u64 + 50);
            // Instant the left→right band centre reaches this flare.
            let q = (flare.x - lo) / (hi - lo);
            let arrive = INTRO_MS + HOLD_MS + (q * REFLECT_MS as f32) as u64;
            let center = reflection_center(&geo, arrive).unwrap();
            assert!(
                (center - flare.x).abs() < 1.0,
                "center {center} vs {}",
                flare.x
            );
            let surge = flare_intensity(&geo, flare, arrive);
            assert!(surge > 0.9 && surge > rest, "surge {surge} vs rest {rest}");
        }
    }
}
