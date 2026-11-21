//! The scrim every host modal sits on: it dims the frame behind the modal and
//! blurs it slightly, so the dialog reads as the one thing in focus.
//!
//! The frame paints the scrim once, after the panes and chrome and before
//! the first modal, so every dialog, picker and modal menu gets the same one.
//! It fades in when a modal opens; with reduced motion, from the app's
//! `reduced_motion` setting or the system's, it appears at once. When the
//! system asks for reduced transparency the scrim dims without blurring.
//! The blur runs on a copy of the frame four logical pixels to the pixel.
//! Where that would cost too much the scrim dims only: on a very large
//! window, and for the next modal after a blurred frame took longer than the
//! budget. A modal keeps the look it opened with until it closes.

use std::time::{Duration, Instant};

use crate::raster::{alpha_of, pack_argb, unpack_rgb};
use crate::system_accessibility::Preferences;
use crate::theme::ThemeVariant;

/// Fade-in when a modal opens.
pub(crate) const FADE: Duration = Duration::from_millis(140);
/// Scrim opacity over a dark theme.
const DIM_DARK: f32 = 0.42;
/// Light themes need less to read as dimmed.
const DIM_LIGHT: f32 = 0.30;
/// The blur works on a copy this many logical pixels smaller in each
/// direction, blurred by one pixel there: about a 5-pixel blur.
const STEP_LOGICAL: f64 = 4.0;
/// Radius of each box pass on the small copy.
const BLUR_RADIUS: usize = 1;
/// Above this many pixels the scrim dims only.
const BLUR_PIXEL_LIMIT: usize = 6144 * 3456;
/// After a blur slower than this, the next modal's scrim dims only.
const BLUR_BUDGET: Duration = Duration::from_millis(12);

/// What the scrim does to the frame behind a modal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Look {
    /// Opacity of the dark layer, 0 to 1.
    pub dim: f32,
    /// Device pixels per pixel of the blurred copy; 0 dims only.
    pub step: usize,
}

impl Look {
    pub(crate) fn new(variant: ThemeVariant, scale: f64) -> Self {
        let dim = match variant {
            ThemeVariant::Dark => DIM_DARK,
            ThemeVariant::Light => DIM_LIGHT,
        };
        let step = (STEP_LOGICAL * scale.max(1.0)).round() as usize;
        Self { dim, step }
    }

    fn dim_only(self) -> Self {
        Self { step: 0, ..self }
    }
}

/// Fade and cost state, kept across frames while a modal is open.
#[derive(Debug, Default)]
pub(crate) struct Scrim {
    /// When the current modal opened; `None` while no modal is open.
    pub(crate) opened_at: Option<Instant>,
    /// The last blurred frame went over budget.
    slow: bool,
    /// The open modal's scrim blurs. Chosen when it opens, from `slow`, and
    /// kept until it closes, so a dialog never changes look while it is up.
    blur: bool,
    /// What the last frame painted.
    painted: Option<Painted>,
    /// The system's reduce-motion and reduce-transparency choices, read when
    /// the open modal opened.
    system: Preferences,
    /// Which modal the choices above belong to.
    modal: Option<&'static str>,
}

/// The scrim a frame painted, for native drivers' change lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Painted {
    /// The fade-in had finished.
    pub settled: bool,
    /// It blurred as well as dimmed.
    pub blur: bool,
}

impl Scrim {
    /// Whether `modal` is a dialog the system choices have not been read for:
    /// one opening, or one handed over to by another without a frame between
    /// (the palette's Transparency command).
    pub(crate) fn opens_with(&self, modal: Option<&'static str>) -> bool {
        modal.is_some() && modal != self.modal
    }

    /// The system choices `modal` follows. A dialog taking over from another
    /// keeps the fade already under way but chooses its own look.
    pub(crate) fn follow_system(&mut self, modal: Option<&'static str>, system: Preferences) {
        self.modal = modal;
        self.system = system;
        if self.opened_at.is_some() {
            self.blur = !self.slow && !self.system.reduce_transparency;
        }
    }

    /// How far the scrim has faded in, from 0 to 1, or `None` with no modal.
    pub(crate) fn level(&mut self, open: bool, now: Instant, reduced_motion: bool) -> Option<f32> {
        if !open {
            self.opened_at = None;
            self.painted = None;
            self.modal = None;
            return None;
        }
        let opened = *self.opened_at.get_or_insert_with(|| {
            self.blur = !self.slow && !self.system.reduce_transparency;
            now
        });
        if reduced_motion || self.system.reduce_motion {
            return Some(1.0);
        }
        let t = (now.saturating_duration_since(opened).as_secs_f32() / FADE.as_secs_f32()).min(1.0);
        // Ease out: quick to start, gentle to settle.
        Some(1.0 - (1.0 - t) * (1.0 - t))
    }

    /// Paint the scrim at `level` and report whether it is still fading in.
    pub(crate) fn paint(
        &mut self,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        look: Look,
        level: f32,
    ) -> bool {
        let look = if !self.blur || width.saturating_mul(height) > BLUR_PIXEL_LIMIT {
            look.dim_only()
        } else {
            look
        };
        let started = Instant::now();
        paint(buffer, width, height, look, level);
        if look.step > 0 {
            self.slow = started.elapsed() > BLUR_BUDGET;
        }
        self.painted = Some(Painted {
            settled: level >= 1.0,
            blur: look.step > 0,
        });
        level < 1.0
    }

    /// The scrim the last frame painted; `None` when it painted none.
    pub(crate) fn painted(&self) -> Option<Painted> {
        self.painted
    }
}

/// Dim and blur `buffer` (row stride `width`) at `level` from 0 to 1.
pub(crate) fn paint(buffer: &mut [u32], width: usize, height: usize, look: Look, level: f32) {
    let level = level.clamp(0.0, 1.0);
    if level <= 0.0 || width == 0 || height == 0 || buffer.len() < width * height {
        return;
    }
    let dim = look.dim.clamp(0.0, 1.0) * level;
    // Opaque pixels (nearly all of them) take the integer path.
    let keep = ((1.0 - dim) * 256.0).round() as u32;
    let blurred = (look.step > 0).then(|| Small::blurred(buffer, width, height, look.step));
    let blur_weight = (level * 256.0).round() as u32;
    let columns: Vec<(usize, usize, u32)> = match blurred.as_ref() {
        Some(small) => (0..width).map(|x| axis(x, small.w, small.step)).collect(),
        None => Vec::new(),
    };
    let shade = |first_row: usize, band: &mut [u32]| {
        let mut soft_row = vec![[0u32; 3]; blurred.as_ref().map_or(0, |small| small.w)];
        for (i, row) in band.chunks_exact_mut(width).enumerate() {
            if let Some(small) = blurred.as_ref() {
                small.row(first_row + i, &mut soft_row);
            }
            for (x, px) in row.iter_mut().enumerate() {
                let mut rgb = *px & 0x00ff_ffff;
                if let Some(&(lo, hi, wx)) = columns.get(x) {
                    let soft = mix_soft(soft_row[lo], soft_row[hi], wx);
                    rgb = lerp_rgb(rgb, soft, blur_weight);
                }
                *px = if *px >> 24 == 0xff {
                    0xff00_0000 | scale_rgb(rgb, keep)
                } else {
                    over_black(unpack_rgb(rgb), alpha_of(*px), dim)
                };
            }
        }
    };
    in_bands(
        &mut buffer[..width * height],
        width,
        height,
        width * height,
        shade,
    );
}

/// Run `work(first_row, band)` over horizontal bands of `items`, which holds
/// `rows` rows of `row_len` items each. A job touching `pixels` frame pixels
/// in all runs on several threads when that is large.
fn in_bands<T: Send>(
    items: &mut [T],
    row_len: usize,
    rows: usize,
    pixels: usize,
    work: impl Fn(usize, &mut [T]) + Sync,
) {
    let threads = threads_for(pixels);
    if threads <= 1 || rows < threads * 2 {
        work(0, items);
        return;
    }
    let band_rows = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, band) in items.chunks_mut(band_rows * row_len).enumerate() {
            let work = &work;
            scope.spawn(move || work(i * band_rows, band));
        }
    });
}

/// Threads worth using for `pixels` of work: one below a small frame, up
/// to eight on a large one.
fn threads_for(pixels: usize) -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cores = *CORES.get_or_init(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });
    (pixels / (256 * 1024)).clamp(1, cores.min(8))
}

/// Each channel of `rgb` times `weight / 256`.
fn scale_rgb(rgb: u32, weight: u32) -> u32 {
    let rb = ((rgb & 0x00ff_00ff) * weight + 0x0080_0080) >> 8 & 0x00ff_00ff;
    let g = ((rgb & 0x0000_ff00) * weight + 0x0000_8000) >> 8 & 0x0000_ff00;
    rb | g
}

/// `from` moved toward `to` by `weight / 256`, per channel.
fn lerp_rgb(from: u32, to: u32, weight: u32) -> u32 {
    if weight >= 256 {
        return to;
    }
    let rb = (from & 0x00ff_00ff) * (256 - weight) + (to & 0x00ff_00ff) * weight + 0x0080_0080;
    let g = (from & 0x0000_ff00) * (256 - weight) + (to & 0x0000_ff00) * weight + 0x0000_8000;
    (rb >> 8 & 0x00ff_00ff) | (g >> 8 & 0x0000_ff00)
}

/// Horizontal mix of two vertically mixed samples (each channel × 256).
fn mix_soft(left: [u32; 3], right: [u32; 3], wx: u32) -> u32 {
    let channel = |c: usize| (left[c] * (256 - wx) + right[c] * wx + (1 << 15)) >> 16;
    channel(0) << 16 | channel(1) << 8 | channel(2)
}

/// A black layer of opacity `dim` over a straight-alpha pixel. A translucent
/// window darkens too: the layer raises its alpha.
fn over_black(rgb: [u8; 3], alpha: u8, dim: f32) -> u32 {
    let below = f32::from(alpha) / 255.0;
    let out = dim + below * (1.0 - dim);
    if out <= 0.0 {
        return pack_argb(0, [0, 0, 0]);
    }
    let keep = below * (1.0 - dim) / out;
    let scaled = rgb.map(|c| (f32::from(c) * keep).round() as u8);
    pack_argb((out * 255.0).round() as u8, scaled)
}

/// The frame shrunk by `step` and blurred, sampled back bilinearly.
struct Small {
    w: usize,
    h: usize,
    step: usize,
    rgb: Vec<[u32; 3]>,
}

/// Box passes over the small copy; two read as a soft Gaussian.
const BLUR_PASSES: usize = 2;

impl Small {
    fn blurred(buffer: &[u32], width: usize, height: usize, step: usize) -> Self {
        let w = width.div_ceil(step);
        let h = height.div_ceil(step);
        let mut rgb = vec![[0u32; 3]; w * h];
        let shrink = |first_row: usize, band: &mut [[u32; 3]]| {
            for (i, out) in band.chunks_exact_mut(w).enumerate() {
                let y0 = (first_row + i) * step;
                let y1 = (y0 + step).min(height);
                for y in y0..y1 {
                    let row = &buffer[y * width..(y + 1) * width];
                    for (block, sum) in row.chunks(step).zip(out.iter_mut()) {
                        for px in block {
                            sum[0] += px >> 16 & 0xff;
                            sum[1] += px >> 8 & 0xff;
                            sum[2] += px & 0xff;
                        }
                    }
                }
                for (qx, sum) in out.iter_mut().enumerate() {
                    let cols = (width - qx * step).min(step);
                    let n = ((y1 - y0) * cols) as u32;
                    *sum = sum.map(|total| (total + n / 2) / n);
                }
            }
        };
        in_bands(&mut rgb, w, h, width * height, shrink);
        let mut tmp = vec![[0u32; 3]; w * h];
        for _ in 0..BLUR_PASSES {
            for (src, dst) in rgb.chunks_exact(w).zip(tmp.chunks_exact_mut(w)) {
                box_row(src, dst, BLUR_RADIUS);
            }
            box_columns(&tmp, &mut rgb, w, h, BLUR_RADIUS);
        }
        Self { w, h, step, rgb }
    }

    /// The blurred row under full-size row `y`, mixed vertically, each
    /// channel scaled by 256.
    fn row(&self, y: usize, out: &mut [[u32; 3]]) {
        let (y0, y1, wy) = axis(y, self.h, self.step);
        let top = &self.rgb[y0 * self.w..(y0 + 1) * self.w];
        let bottom = &self.rgb[y1 * self.w..(y1 + 1) * self.w];
        for ((slot, t), b) in out.iter_mut().zip(top).zip(bottom) {
            for c in 0..3 {
                slot[c] = t[c] * (256 - wy) + b[c] * wy;
            }
        }
    }
}

/// Box mean of one row, clamping at the ends.
fn box_row(src: &[[u32; 3]], dst: &mut [[u32; 3]], radius: usize) {
    let n = src.len();
    let k = (radius * 2 + 1) as u32;
    let at = |i: isize| src[i.clamp(0, n as isize - 1) as usize];
    let mut acc = [0u32; 3];
    for i in -(radius as isize)..=(radius as isize) {
        add(&mut acc, at(i));
    }
    for (x, out) in dst.iter_mut().enumerate() {
        *out = acc.map(|total| (total + k / 2) / k);
        let x = x as isize;
        add(&mut acc, at(x + radius as isize + 1));
        sub(&mut acc, at(x - radius as isize));
    }
}

/// Box mean down each column, a whole row at a time so memory runs in order.
fn box_columns(src: &[[u32; 3]], dst: &mut [[u32; 3]], w: usize, h: usize, radius: usize) {
    let k = (radius * 2 + 1) as u32;
    let row = |y: isize| {
        let y = y.clamp(0, h as isize - 1) as usize;
        &src[y * w..(y + 1) * w]
    };
    let mut acc = vec![[0u32; 3]; w];
    for y in -(radius as isize)..=(radius as isize) {
        for (sum, px) in acc.iter_mut().zip(row(y)) {
            add(sum, *px);
        }
    }
    for (y, out) in dst.chunks_exact_mut(w).enumerate() {
        for (slot, sum) in out.iter_mut().zip(&acc) {
            *slot = sum.map(|total| (total + k / 2) / k);
        }
        let y = y as isize;
        let (enter, leave) = (row(y + radius as isize + 1), row(y - radius as isize));
        for ((sum, a), b) in acc.iter_mut().zip(enter).zip(leave) {
            add(sum, *a);
            sub(sum, *b);
        }
    }
}

fn add(acc: &mut [u32; 3], px: [u32; 3]) {
    for c in 0..3 {
        acc[c] += px[c];
    }
}

fn sub(acc: &mut [u32; 3], px: [u32; 3]) {
    for c in 0..3 {
        acc[c] -= px[c];
    }
}

/// Neighbours and weight (0 to 256) for full-size coordinate `i` on an axis
/// `n` small pixels of `step` long, sampling at small-pixel centres.
fn axis(i: usize, n: usize, step: usize) -> (usize, usize, u32) {
    let pos = (i * 2 + 1) as f32 / (2 * step) as f32 - 0.5;
    if pos <= 0.0 {
        return (0, 0, 0);
    }
    let lo = (pos.floor() as usize).min(n - 1);
    let hi = (lo + 1).min(n - 1);
    let weight = ((pos - lo as f32) * 256.0).round() as u32;
    (lo, hi, weight.min(256))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: Look = Look {
        dim: DIM_DARK,
        step: 4,
    };

    fn channel(px: u32) -> u8 {
        unpack_rgb(px)[0]
    }

    #[test]
    fn a_settled_scrim_dims_every_pixel() {
        let (w, h) = (64, 40);
        let mut frame = vec![pack_argb(255, [200, 200, 200]); w * h];
        paint(&mut frame, w, h, DARK, 1.0);
        // 200 under a 42% black layer is 116.
        assert!(frame
            .iter()
            .all(|px| *px == pack_argb(255, [116, 116, 116])));
    }

    #[test]
    fn the_blur_softens_edges_and_dim_only_keeps_them() {
        let (w, h) = (96, 32);
        let split = |x: usize| if x < w / 2 { 0 } else { 240 };
        let edge = |look: Look| {
            let mut frame: Vec<u32> = (0..w * h)
                .map(|i| {
                    let v = split(i % w);
                    pack_argb(255, [v, v, v])
                })
                .collect();
            paint(&mut frame, w, h, look, 1.0);
            let row = &frame[(h / 2) * w..(h / 2 + 1) * w];
            row.iter().map(|px| channel(*px)).collect::<Vec<u8>>()
        };
        let sharp = edge(DARK.dim_only());
        assert_eq!(sharp[w / 2 - 1], 0);
        assert_eq!(sharp[w / 2], 139);
        let soft = edge(DARK);
        assert!(
            soft[w / 2 - 1] > 10 && soft[w / 2] < 130,
            "the edge blends: {:?}",
            &soft[w / 2 - 4..w / 2 + 4]
        );
        assert_eq!(soft[0], 0, "far from the edge the dark side stays dark");
        assert_eq!(soft[w - 1], 139, "and the light side only dims");
    }

    #[test]
    fn a_translucent_window_darkens_too() {
        let mut frame = vec![pack_argb(0, [0, 0, 0]), pack_argb(128, [255, 255, 255])];
        paint(&mut frame, 2, 1, DARK.dim_only(), 1.0);
        assert_eq!(frame[0], pack_argb(107, [0, 0, 0]));
        // Alpha 0.42 + 0.502 * 0.58 = 0.711; white keeps 0.291 / 0.711 of itself.
        assert_eq!(frame[1], pack_argb(181, [104, 104, 104]));
    }

    #[test]
    fn the_scrim_fades_in_then_holds_and_resets_on_close() {
        let mut scrim = Scrim::default();
        let t0 = Instant::now();
        assert_eq!(scrim.level(false, t0, false), None);
        assert_eq!(scrim.level(true, t0, false), Some(0.0));
        let half = scrim.level(true, t0 + FADE / 2, false).unwrap();
        assert!(half > 0.5 && half < 1.0, "eased past halfway: {half}");
        assert_eq!(scrim.level(true, t0 + FADE, false), Some(1.0));
        assert_eq!(scrim.level(true, t0 + FADE * 10, false), Some(1.0));
        assert_eq!(scrim.level(false, t0 + FADE * 11, false), None);
        let reopened = t0 + FADE * 12;
        assert_eq!(
            scrim.level(true, reopened, false),
            Some(0.0),
            "a new modal fades again"
        );
    }

    #[test]
    fn reduced_motion_shows_the_scrim_at_once() {
        let mut scrim = Scrim::default();
        assert_eq!(scrim.level(true, Instant::now(), true), Some(1.0));
    }

    #[test]
    fn a_partly_faded_scrim_dims_less() {
        let (w, h) = (8, 8);
        let mut early = vec![pack_argb(255, [200, 200, 200]); w * h];
        paint(&mut early, w, h, DARK, 0.5);
        // Half of 42% is 21%: 200 becomes 158.
        assert_eq!(early[0], pack_argb(255, [158, 158, 158]));
        let mut closed = vec![pack_argb(255, [200, 200, 200]); w * h];
        paint(&mut closed, w, h, DARK, 0.0);
        assert_eq!(closed[0], pack_argb(255, [200, 200, 200]));
    }

    #[test]
    fn a_slow_blur_dims_only_from_the_next_modal() {
        let look = Look::new(ThemeVariant::Dark, 1.0);
        assert_eq!(look.step, 4);
        assert_eq!(Look::new(ThemeVariant::Dark, 2.0).step, 8);
        assert_eq!(Look::new(ThemeVariant::Light, 1.0).dim, DIM_LIGHT);
        let (w, h) = (32, 8);
        let edge = |scrim: &mut Scrim| {
            let mut frame: Vec<u32> = (0..w * h)
                .map(|i| pack_argb(255, if i % w < w / 2 { [0; 3] } else { [240; 3] }))
                .collect();
            assert!(!scrim.paint(&mut frame, w, h, look, 1.0));
            channel(frame[w / 2 - 1])
        };
        let t0 = Instant::now();
        let mut scrim = Scrim::default();
        scrim.level(true, t0, true);
        // A blur over budget while this modal is up does not change its look.
        scrim.slow = true;
        assert!(edge(&mut scrim) > 0, "the open modal keeps its blur");
        assert_eq!(scrim.painted().map(|p| p.blur), Some(true));
        scrim.slow = true;
        scrim.level(false, t0, true);
        scrim.level(true, t0, true);
        assert_eq!(edge(&mut scrim), 0, "the next modal dims only");
        assert_eq!(scrim.painted().map(|p| p.blur), Some(false));
    }

    #[test]
    fn the_system_reduce_motion_setting_shows_the_scrim_at_once() {
        let mut scrim = Scrim::default();
        scrim.follow_system(
            Some("palette"),
            Preferences {
                reduce_motion: true,
                reduce_transparency: false,
            },
        );
        assert_eq!(scrim.level(true, Instant::now(), false), Some(1.0));
        let mut fading = Scrim::default();
        let t0 = Instant::now();
        fading.level(true, t0, false);
        assert!(fading.level(true, t0, false).unwrap() < 1.0);
    }

    #[test]
    fn the_system_reduce_transparency_setting_dims_without_blur() {
        let look = Look::new(ThemeVariant::Dark, 1.0);
        let painted = |system: Preferences| {
            let mut scrim = Scrim::default();
            scrim.follow_system(Some("palette"), system);
            scrim.level(true, Instant::now(), true);
            let mut frame = vec![pack_argb(255, [100; 3]); 32 * 8];
            scrim.paint(&mut frame, 32, 8, look, 1.0);
            scrim.painted().unwrap()
        };
        let reduced = painted(Preferences {
            reduce_motion: false,
            reduce_transparency: true,
        });
        assert!(reduced.settled && !reduced.blur);
        assert!(painted(Preferences::default()).blur);
    }

    #[test]
    fn a_modal_opens_once_until_it_closes() {
        let mut scrim = Scrim::default();
        assert!(!scrim.opens_with(None));
        assert!(scrim.opens_with(Some("palette")));
        scrim.follow_system(Some("palette"), Preferences::default());
        scrim.level(true, Instant::now(), false);
        assert!(!scrim.opens_with(Some("palette")));
        scrim.level(false, Instant::now(), false);
        assert!(scrim.opens_with(Some("palette")));
    }

    #[test]
    fn a_dialog_handed_over_to_reads_the_system_choices_again() {
        let look = Look::new(ThemeVariant::Dark, 1.0);
        let mut scrim = Scrim::default();
        let t0 = Instant::now();
        // The palette opens with the system asking for nothing.
        scrim.follow_system(Some("palette"), Preferences::default());
        scrim.level(true, t0, false);
        let mut frame = vec![pack_argb(255, [100; 3]); 32 * 8];
        scrim.paint(&mut frame, 32, 8, look, 0.5);
        assert_eq!(scrim.painted().map(|p| p.blur), Some(true));
        // The person turns Reduce transparency and Reduce motion on, then
        // picks Transparency from the palette: no frame has no modal.
        assert!(scrim.opens_with(Some("transparency")));
        scrim.follow_system(
            Some("transparency"),
            Preferences {
                reduce_motion: true,
                reduce_transparency: true,
            },
        );
        assert!(!scrim.opens_with(Some("transparency")));
        assert_eq!(
            scrim.level(true, t0, false),
            Some(1.0),
            "the new dialog follows the new reduce-motion choice"
        );
        scrim.paint(&mut frame, 32, 8, look, 1.0);
        assert_eq!(
            scrim.painted().map(|p| p.blur),
            Some(false),
            "and the new reduce-transparency choice"
        );
    }
}
