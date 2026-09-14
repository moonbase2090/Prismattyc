//! Walkthrough session, caption band, and host detectors (PT-192–196).
//!
//! Catalog, progress, and `detect_step` live in `prismattyc_mux::walkthrough`.
//! This module keeps `WalkthroughLive`, caption geometry, and hit testing.
//! No `HostState`.

use prismattyc_mux::walkthrough as wt;

pub(crate) use wt::{
    boss_matches, bundled_boss, bundled_catalog, detect_step, load_progress, mux_detected,
    progress_path, reset_progress, save_progress, space_from_pane_counts, BossVerdict, Catalog,
    DetectOutcome, Detected, Expect, Progress, ShowMe, Step,
};

/// Host walkthrough session: shared mux Cursor plus caption state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalkthroughLive {
    cursor: wt::Cursor,
    caption_visible: bool,
    fail_hint: Option<String>,
}

/// Line two after a failed action. The step stays armed.
pub(crate) const DETECT_FAIL_HINT: &str = "That did not complete. Try again.";

/// Caption lines and which controls to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptionView {
    pub caption: String,
    pub line2: String,
    pub show_me: bool,
    pub skip: bool,
}

/// Pixel band and exclusive hit targets (PT-193).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptionBand {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub dismiss: CaptionRect,
    pub show_me: Option<CaptionRect>,
    pub skip: Option<CaptionRect>,
}

/// Inclusive origin, exclusive-size rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptionRect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl CaptionRect {
    fn contains(self, px: usize, py: usize) -> bool {
        px >= self.x
            && px < self.x.saturating_add(self.w)
            && py >= self.y
            && py < self.y.saturating_add(self.h)
    }
}

/// Which caption control was hit. The rest of the band falls through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptionHit {
    Dismiss,
    ShowMe,
    Skip,
}

pub(crate) const SHOW_ME_LABEL: &str = "[show me]";
pub(crate) const SKIP_LABEL: &str = "[skip]";
pub(crate) const DISMISS_LABEL: &str = "×";
/// ~75 % opaque band (owner spike).
pub(crate) const CAPTION_ALPHA: u8 = 191;

impl WalkthroughLive {
    /// Start at the first catalog step with the caption shown.
    pub(crate) fn start(catalog: Catalog) -> Option<Self> {
        Self::start_with(catalog, None)
    }

    /// Resume at the first step neither completed nor skipped.
    /// All-done progress replays from step 0 and keeps the records.
    pub(crate) fn start_with(catalog: Catalog, progress: Option<&Progress>) -> Option<Self> {
        Some(Self {
            cursor: wt::Cursor::resume(catalog, progress, None)?,
            caption_visible: true,
            fail_hint: None,
        })
    }

    /// Snapshot for the progress file. `now` is injected for tests.
    pub(crate) fn progress(&self, now: std::time::SystemTime) -> Option<Progress> {
        self.cursor.snapshot(now)
    }

    pub(crate) fn current_step(&self) -> Option<&Step> {
        self.cursor.current_step()
    }

    /// Catalog position: steps in earlier levels plus `cursor.step`.
    pub(crate) fn caption_index(&self) -> usize {
        self.cursor
            .catalog
            .level
            .iter()
            .take(self.cursor.level)
            .map(|level| level.step.len())
            .sum::<usize>()
            .saturating_add(self.cursor.step)
    }

    pub(crate) fn completed_ids(&self) -> &[String] {
        &self.cursor.completed
    }

    pub(crate) fn skipped_ids(&self) -> &[String] {
        &self.cursor.skipped
    }

    pub(crate) fn boss_step_armed(&self) -> bool {
        matches!(
            self.current_step().map(|step| &step.expect),
            Some(Expect::SpaceEvent { event }) if event == "boss_snapshot_match"
        )
    }

    /// Caption line 2 for a boss mismatch. Does not mark Fail.
    pub(crate) fn set_mismatch_hint(&mut self, text: String) {
        self.fail_hint = Some(text);
        self.caption_visible = true;
    }

    /// Hide the caption. The step stays armed.
    pub(crate) fn dismiss_caption(&mut self) {
        self.caption_visible = false;
    }

    /// Show the current caption again (palette / splash resume).
    pub(crate) fn reveal_caption(&mut self) {
        self.caption_visible = true;
    }

    /// Skip the current step. False means the catalog is finished.
    pub(crate) fn skip(&mut self) -> bool {
        let more = self.cursor.skip();
        if more {
            self.caption_visible = true;
            self.fail_hint = None;
        }
        more
    }

    /// Feed a real action or mux event. Show me completes only through this.
    pub(crate) fn note(&mut self, fact: &Detected) -> DetectOutcome {
        let Some(step) = self.current_step() else {
            return DetectOutcome::Ignore;
        };
        let outcome = detect_step(&step.expect, fact);
        match outcome {
            DetectOutcome::Advance => {
                if self.cursor.complete() {
                    self.caption_visible = true;
                    self.fail_hint = None;
                    DetectOutcome::Advance
                } else {
                    self.fail_hint = None;
                    self.caption_visible = false;
                    DetectOutcome::Finished
                }
            }
            DetectOutcome::Fail => {
                self.fail_hint = Some(DETECT_FAIL_HINT.to_string());
                self.caption_visible = true;
                DetectOutcome::Fail
            }
            DetectOutcome::Ignore | DetectOutcome::Finished => DetectOutcome::Ignore,
        }
    }

    /// Host action name for Show me, if this step can dispatch one.
    pub(crate) fn show_me_action(&self) -> Option<&str> {
        match self.current_step()?.show_me.as_ref()? {
            ShowMe::HostAction { action } => Some(action.as_str()),
            _ => None,
        }
    }

    /// Stub fact for Show me when the step has no host action to dispatch.
    /// Command and space adapters are PT-195..198; Show me still advances.
    pub(crate) fn show_me_stub(&self) -> Option<Detected> {
        let step = self.current_step()?;
        if matches!(step.show_me, Some(ShowMe::HostAction { .. })) {
            return None;
        }
        match &step.expect {
            Expect::CommandEvent { command, result } => Some(Detected::CommandEvent {
                command: command.clone(),
                result: result.clone().unwrap_or_else(|| "ok".into()),
            }),
            Expect::SpaceEvent { event } => Some(Detected::SpaceEvent {
                event: event.clone(),
            }),
            Expect::MuxEvent {
                event,
                to_window,
                session,
            } => Some(Detected::MuxEvent {
                event: event.clone(),
                to_window: to_window.clone(),
                session: session.clone(),
            }),
            Expect::HostAction { .. } => None,
        }
    }

    /// Caption view when the overlay is visible.
    pub(crate) fn view(&self, chord: &str) -> Option<CaptionView> {
        if !self.caption_visible {
            return None;
        }
        let step = self.current_step()?;
        let mut view = caption_view(step, chord);
        if let Some(fail) = self.fail_hint.as_deref() {
            view.line2 = fail.to_string();
        }
        Some(view)
    }
}

/// Line two prefers the live keymap chord, then `command`, then `hint`.
pub(crate) fn caption_view(step: &Step, chord: &str) -> CaptionView {
    let line2 = if !chord.is_empty() {
        chord.to_string()
    } else if let Some(command) = step.command.as_deref().filter(|c| !c.is_empty()) {
        command.to_string()
    } else {
        step.hint.clone().unwrap_or_default()
    };
    CaptionView {
        caption: step.caption.clone(),
        line2,
        show_me: step.show_me.is_some(),
        skip: true,
    }
}

/// Center a two-line subtitle over the pane area. Does not change pane geometry.
pub(crate) fn caption_band(
    (pane_x, pane_y, pane_w, pane_h): (usize, usize, usize, usize),
    cell_w: usize,
    cell_h: usize,
    window_pad: usize,
    view: &CaptionView,
) -> Option<CaptionBand> {
    if cell_w == 0 || cell_h == 0 || pane_w == 0 || pane_h == 0 {
        return None;
    }
    let pad_x = cell_w;
    let pad_y = cell_h / 4 + 1;
    let dismiss_cols = DISMISS_LABEL.chars().count().max(1);
    let line1_cols = view
        .caption
        .chars()
        .count()
        .saturating_add(2 + dismiss_cols);
    let mut line2_cols = view.line2.chars().count();
    if view.show_me {
        line2_cols = line2_cols.saturating_add(1 + SHOW_ME_LABEL.chars().count());
    }
    if view.skip {
        line2_cols = line2_cols.saturating_add(1 + SKIP_LABEL.chars().count());
    }
    let inner_cols = line1_cols.max(line2_cols).max(8);
    let w = inner_cols
        .saturating_mul(cell_w)
        .saturating_add(pad_x.saturating_mul(2))
        .min(pane_w);
    let h = cell_h
        .saturating_mul(2)
        .saturating_add(pad_y.saturating_mul(2));
    let x = pane_x.saturating_add(pane_w.saturating_sub(w) / 2);
    let margin = cell_h.saturating_add(window_pad);
    let y = pane_y
        .saturating_add(pane_h)
        .saturating_sub(margin)
        .saturating_sub(h)
        .max(pane_y);
    let dismiss = CaptionRect {
        x: x.saturating_add(w.saturating_sub(pad_x + dismiss_cols * cell_w)),
        y: y.saturating_add(pad_y),
        w: dismiss_cols.saturating_mul(cell_w),
        h: cell_h,
    };
    // Pin [show me] [skip] to the right edge. Left-aligning them after the
    // chord slides [skip] under a double-click when the next chord is shorter.
    let line2_y = y.saturating_add(pad_y).saturating_add(cell_h);
    let mut right = x.saturating_add(w).saturating_sub(pad_x);
    let skip = view.skip.then(|| {
        let rect_w = SKIP_LABEL.chars().count().saturating_mul(cell_w);
        let rect = CaptionRect {
            x: right.saturating_sub(rect_w).max(x),
            y: line2_y,
            w: rect_w,
            h: cell_h,
        };
        right = rect.x.saturating_sub(cell_w);
        rect
    });
    let show_me = view.show_me.then(|| {
        let rect_w = SHOW_ME_LABEL.chars().count().saturating_mul(cell_w);
        CaptionRect {
            x: right.saturating_sub(rect_w).max(x),
            y: line2_y,
            w: rect_w,
            h: cell_h,
        }
    });
    Some(CaptionBand {
        x,
        y,
        w,
        h,
        dismiss,
        show_me,
        skip,
    })
}

pub(crate) fn caption_hit(band: &CaptionBand, px: usize, py: usize) -> Option<CaptionHit> {
    if band.dismiss.contains(px, py) {
        return Some(CaptionHit::Dismiss);
    }
    if band.show_me.is_some_and(|r| r.contains(px, py)) {
        return Some(CaptionHit::ShowMe);
    }
    if band.skip.is_some_and(|r| r.contains(px, py)) {
        return Some(CaptionHit::Skip);
    }
    None
}

/// Double-click / bounce window. Matches host `MULTI_CLICK_MS`.
pub(crate) const CAPTION_REPEAT_MS: u128 = 500;
/// Pointer jitter still counts as the same press.
pub(crate) const CAPTION_REPEAT_SLOP_PX: usize = 8;

/// Convert a pointer into a caption-press pixel.
///
/// Finite, non-negative coordinates truncate toward zero. `None`, NaN,
/// infinities, and negative axes miss.
pub(crate) fn caption_press_px(pointer: Option<(f64, f64)>) -> Option<(usize, usize)> {
    let (x, y) = pointer?;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    Some((x as usize, y as usize))
}

/// Whether a left caption press is a miss, a consumed repeat, or a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptionClickResult {
    Miss,
    ConsumeRepeat,
    Dispatch(CaptionHit),
}

impl CaptionClickResult {
    pub(crate) fn consumed(self) -> bool {
        !matches!(self, Self::Miss)
    }
}

/// Pure caption-press dispatch. Hit-testing stays in the caller.
pub(crate) fn caption_click_result(
    pointer: Option<(f64, f64)>,
    prev: Option<(usize, usize)>,
    elapsed: Option<std::time::Duration>,
    hit: Option<CaptionHit>,
) -> CaptionClickResult {
    let Some((px, py)) = caption_press_px(pointer) else {
        return CaptionClickResult::Miss;
    };
    if caption_repeat_click(prev, elapsed, px, py) {
        return CaptionClickResult::ConsumeRepeat;
    }
    match hit {
        Some(hit) => CaptionClickResult::Dispatch(hit),
        None => CaptionClickResult::Miss,
    }
}

/// JSON object for one caption control rect, or `null`.
pub(crate) fn caption_rect_json(rect: Option<CaptionRect>) -> String {
    match rect {
        Some(r) => format!(
            "{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            r.x, r.y, r.w, r.h
        ),
        None => "null".into(),
    }
}

/// True when this press is the second half of a caption control click.
/// The caller must consume it and must not dispatch Show me or Skip.
pub(crate) fn caption_repeat_click(
    prev: Option<(usize, usize)>,
    elapsed: Option<std::time::Duration>,
    x: usize,
    y: usize,
) -> bool {
    let Some((px, py)) = prev else {
        return false;
    };
    let Some(elapsed) = elapsed else {
        return false;
    };
    elapsed.as_millis() <= CAPTION_REPEAT_MS
        && x.abs_diff(px) <= CAPTION_REPEAT_SLOP_PX
        && y.abs_diff(py) <= CAPTION_REPEAT_SLOP_PX
}

/// Escape while a caption is showing hides it. No hover required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptionEscape {
    Dismiss,
    PassThrough,
}

pub(crate) fn caption_escape_decision(caption_visible: bool) -> CaptionEscape {
    if caption_visible {
        CaptionEscape::Dismiss
    } else {
        CaptionEscape::PassThrough
    }
}

/// Palette, theme picker, find, and space picker hide the caption
/// for their duration. The walkthrough step stays armed.
pub(crate) fn caption_paint_decision(caption_visible: bool, overlay_open: bool) -> bool {
    caption_visible && !overlay_open
}

/// True when the pointer is over the band (pointer cursor / Esc hover is
/// not required to dismiss).
pub(crate) fn caption_hover(band: &CaptionBand, px: usize, py: usize) -> bool {
    px >= band.x
        && px < band.x.saturating_add(band.w)
        && py >= band.y
        && py < band.y.saturating_add(band.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wt::resume_index;
    use wt::scripted_fact;

    #[test]
    fn start_dismiss_and_skip_keep_the_session_non_modal() {
        let mut live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        let first = live.current_step().unwrap().id.clone();
        assert_eq!(first, "window.split-right");
        assert_eq!(live.show_me_action(), Some("split_right"));
        live.dismiss_caption();
        assert!(live.view("Ctrl+Shift+\\").is_none());
        live.reveal_caption();
        assert_eq!(live.view("Ctrl+Shift+\\").unwrap().line2, "Ctrl+Shift+\\");
        assert!(live.skip());
        assert_eq!(live.current_step().unwrap().id, "window.focus-right");
    }

    #[test]
    fn caption_band_sits_inside_the_pane_and_only_controls_hit() {
        let view = CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "C-S-\\".into(),
            show_me: true,
            skip: true,
        };
        let band = caption_band((10, 20, 400, 200), 8, 16, 8, &view).unwrap();
        assert!(band.x >= 10);
        assert!(band.x + band.w <= 410);
        assert!(band.y >= 20);
        assert!(band.y + band.h + 8 + 16 <= 220);
        assert_eq!(
            caption_hit(&band, band.dismiss.x, band.dismiss.y),
            Some(CaptionHit::Dismiss)
        );
        let show = band.show_me.unwrap();
        let skip = band.skip.unwrap();
        assert_eq!(caption_hit(&band, show.x, show.y), Some(CaptionHit::ShowMe));
        assert_eq!(caption_hit(&band, skip.x, skip.y), Some(CaptionHit::Skip));
        assert!(show.x >= band.x);
        assert!(show.x + show.w <= skip.x);
        assert!(skip.x + skip.w <= band.x + band.w);
        assert!(caption_hit(&band, band.x + 2, band.y + 2).is_none());
        assert!(caption_hover(&band, band.x + 2, band.y + 2));
    }

    #[test]
    fn show_me_and_skip_stay_put_when_the_chord_changes() {
        let pane = (0, 0, 400, 200);
        let short = CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "C-S-W".into(),
            show_me: true,
            skip: true,
        };
        let long = CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "C-S-A-PgDn".into(),
            show_me: true,
            skip: true,
        };
        let a = caption_band(pane, 8, 16, 8, &short).unwrap();
        let b = caption_band(pane, 8, 16, 8, &long).unwrap();
        assert_eq!(a.show_me, b.show_me, "chord length must not move Show me");
        assert_eq!(a.skip, b.skip, "chord length must not move Skip");
    }

    #[test]
    fn long_chord_keeps_show_me_inside_a_narrow_pane() {
        let view = CaptionView {
            caption: "Split.".into(),
            line2: "C-S-A-PgDn/C-S-A-PgUp".into(),
            show_me: true,
            skip: true,
        };
        let band = caption_band((0, 0, 160, 120), 8, 16, 4, &view).unwrap();
        let show = band.show_me.unwrap();
        let skip = band.skip.unwrap();
        assert!(show.x >= band.x);
        assert!(show.x + show.w <= band.x + band.w);
        assert!(skip.x + skip.w <= band.x + band.w);
        assert_eq!(
            caption_hit(&band, show.x + 1, show.y + 1),
            Some(CaptionHit::ShowMe)
        );
    }

    fn caption_band_left_aligned(
        (pane_x, pane_y, pane_w, pane_h): (usize, usize, usize, usize),
        cell_w: usize,
        cell_h: usize,
        window_pad: usize,
        view: &CaptionView,
    ) -> CaptionBand {
        // Pre-PT-295 layout: [show me] [skip] follow the chord. A shorter
        // next chord slides [skip] under the recorded Show me pixel.
        let pad_x = cell_w;
        let pad_y = cell_h / 4 + 1;
        let dismiss_cols = DISMISS_LABEL.chars().count().max(1);
        let line1_cols = view
            .caption
            .chars()
            .count()
            .saturating_add(2 + dismiss_cols);
        let mut line2_cols = view.line2.chars().count();
        if view.show_me {
            line2_cols = line2_cols.saturating_add(1 + SHOW_ME_LABEL.chars().count());
        }
        if view.skip {
            line2_cols = line2_cols.saturating_add(1 + SKIP_LABEL.chars().count());
        }
        let inner_cols = line1_cols.max(line2_cols).max(8);
        let w = inner_cols
            .saturating_mul(cell_w)
            .saturating_add(pad_x.saturating_mul(2))
            .min(pane_w);
        let h = cell_h
            .saturating_mul(2)
            .saturating_add(pad_y.saturating_mul(2));
        let x = pane_x.saturating_add(pane_w.saturating_sub(w) / 2);
        let margin = cell_h.saturating_add(window_pad);
        let y = pane_y
            .saturating_add(pane_h)
            .saturating_sub(margin)
            .saturating_sub(h)
            .max(pane_y);
        let dismiss = CaptionRect {
            x: x.saturating_add(w.saturating_sub(pad_x + dismiss_cols * cell_w)),
            y: y.saturating_add(pad_y),
            w: dismiss_cols.saturating_mul(cell_w),
            h: cell_h,
        };
        let mut cursor_x = x
            .saturating_add(pad_x)
            .saturating_add(view.line2.chars().count().saturating_mul(cell_w));
        if !view.line2.is_empty() {
            cursor_x = cursor_x.saturating_add(cell_w);
        }
        let line2_y = y.saturating_add(pad_y).saturating_add(cell_h);
        let show_me = view.show_me.then(|| {
            let rect = CaptionRect {
                x: cursor_x,
                y: line2_y,
                w: SHOW_ME_LABEL.chars().count().saturating_mul(cell_w),
                h: cell_h,
            };
            cursor_x = cursor_x.saturating_add(rect.w).saturating_add(cell_w);
            rect
        });
        let skip = view.skip.then(|| CaptionRect {
            x: cursor_x,
            y: line2_y,
            w: SKIP_LABEL.chars().count().saturating_mul(cell_w),
            h: cell_h,
        });
        CaptionBand {
            x,
            y,
            w,
            h,
            dismiss,
            show_me,
            skip,
        }
    }

    #[test]
    fn left_aligned_shorter_chord_puts_skip_under_show_me_pixel() {
        let pane = (0, 0, 400, 200);
        let long = CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "Ctrl+Shift+\\".into(),
            show_me: true,
            skip: true,
        };
        let short = CaptionView {
            caption: "Focus the pane on the right.".into(),
            line2: "C-S-W".into(),
            show_me: true,
            skip: true,
        };
        let first = caption_band_left_aligned(pane, 8, 16, 8, &long);
        let show = first.show_me.unwrap();
        let px = show.x + show.w / 2;
        let py = show.y + show.h / 2;
        assert_eq!(caption_hit(&first, px, py), Some(CaptionHit::ShowMe));
        let next = caption_band_left_aligned(pane, 8, 16, 8, &short);
        assert_eq!(
            caption_hit(&next, px, py),
            Some(CaptionHit::Skip),
            "reverting the right-pin puts [skip] under the recorded pixel"
        );
        let pinned = caption_band(pane, 8, 16, 8, &short).unwrap();
        assert_eq!(
            caption_hit(&pinned, px, py),
            Some(CaptionHit::ShowMe),
            "right-pin keeps [show me] under the recorded pixel"
        );
    }

    #[test]
    fn without_repeat_click_the_second_press_is_the_new_hit() {
        use std::time::Duration;
        let pane = (0, 0, 400, 200);
        let long = CaptionView {
            caption: "Split the pane to the right.".into(),
            line2: "Ctrl+Shift+\\".into(),
            show_me: true,
            skip: true,
        };
        let short = CaptionView {
            caption: "Focus the pane on the right.".into(),
            line2: "C-S-W".into(),
            show_me: true,
            skip: true,
        };
        let first = caption_band_left_aligned(pane, 8, 16, 8, &long);
        let show = first.show_me.unwrap();
        let px = show.x + show.w / 2;
        let py = show.y + show.h / 2;
        let elapsed = Duration::from_millis(80);
        assert!(
            caption_repeat_click(Some((px, py)), Some(elapsed), px, py),
            "debounce consumes the second press"
        );
        let next = caption_band_left_aligned(pane, 8, 16, 8, &short);
        assert_eq!(
            caption_hit(&next, px, py),
            Some(CaptionHit::Skip),
            "caption_repeat_click returning false observes a skip"
        );
    }

    #[test]
    fn caption_index_starts_at_zero() {
        let live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        assert_eq!(live.caption_index(), 0);
        assert!(live.skipped_ids().is_empty());
        assert!(live.completed_ids().is_empty());
    }

    #[test]
    fn getters_after_one_advance_and_one_skip() {
        let mut skipped = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        let first = skipped.current_step().unwrap().id.clone();
        assert!(skipped.skip());
        assert_eq!(skipped.caption_index(), 1);
        assert_eq!(skipped.skipped_ids(), std::slice::from_ref(&first));
        assert!(skipped.completed_ids().is_empty());

        let mut advanced = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        assert_eq!(
            advanced.note(&Detected::HostAction {
                action: "split_right".into(),
                result: "ok".into(),
            }),
            DetectOutcome::Advance
        );
        assert_eq!(advanced.caption_index(), 1);
        assert_eq!(advanced.completed_ids(), &[first]);
        assert!(advanced.skipped_ids().is_empty());
    }

    #[test]
    fn caption_press_px_table() {
        let cases = [
            ("none", None, None),
            ("nan x", Some((f64::NAN, 1.0)), None),
            ("nan y", Some((1.0, f64::NAN)), None),
            ("neg x", Some((-0.1, 4.0)), None),
            ("neg y", Some((4.0, -0.1)), None),
            ("zero", Some((0.0, 0.0)), Some((0, 0))),
            ("width", Some((1600.0, 900.0)), Some((1600, 900))),
            ("just below zero", Some((-f64::EPSILON, 1.0)), None),
            ("fractional", Some((3.9, 2.1)), Some((3, 2))),
            ("large", Some((1_000_000.7, 0.0)), Some((1_000_000, 0))),
        ];
        for (name, pointer, expected) in cases {
            assert_eq!(caption_press_px(pointer), expected, "{name}");
        }
    }

    #[test]
    fn caption_click_result_table() {
        use std::time::Duration;
        let pointer = Some((10.2, 20.8));
        let prev = Some((10, 20));
        let hit = Some(CaptionHit::Skip);
        let cases = [
            (
                "repeat consumed",
                pointer,
                prev,
                Some(Duration::from_millis(80)),
                hit,
                CaptionClickResult::ConsumeRepeat,
                true,
            ),
            (
                "hit dispatched",
                pointer,
                prev,
                Some(Duration::from_millis(501)),
                hit,
                CaptionClickResult::Dispatch(CaptionHit::Skip),
                true,
            ),
            (
                "miss",
                pointer,
                None,
                None,
                None,
                CaptionClickResult::Miss,
                false,
            ),
        ];
        for (name, pointer, prev, elapsed, hit, expected, consumed) in cases {
            let result = caption_click_result(pointer, prev, elapsed, hit);
            assert_eq!(result, expected, "{name}");
            assert_eq!(result.consumed(), consumed, "{name} consumed");
        }
    }

    #[test]
    fn caption_rect_json_is_exact() {
        assert_eq!(caption_rect_json(None), "null");
        assert_eq!(
            caption_rect_json(Some(CaptionRect {
                x: 8,
                y: 16,
                w: 24,
                h: 12
            })),
            "{\"x\":8,\"y\":16,\"w\":24,\"h\":12}"
        );
    }

    #[test]
    fn caption_band_geometry_for_a_known_pane() {
        let view = CaptionView {
            caption: "Split.".into(),
            line2: "C-S-\\".into(),
            show_me: true,
            skip: true,
        };
        let band = caption_band((10, 20, 400, 200), 8, 16, 8, &view).unwrap();
        assert_eq!(band.w % 8, 0);
        assert!(band.x >= 10 && band.x + band.w <= 410);
        let show = band.show_me.unwrap();
        let skip = band.skip.unwrap();
        assert!(show.x + show.w <= skip.x);
    }

    #[test]
    fn caption_repeat_click_swallows_a_double_press() {
        use std::time::Duration;
        assert!(caption_repeat_click(
            Some((100, 50)),
            Some(Duration::from_millis(120)),
            102,
            51
        ));
        assert!(
            !caption_repeat_click(Some((100, 50)), Some(Duration::from_millis(501)), 100, 50),
            "a later click at the same pixel is a new gesture"
        );
        assert!(!caption_repeat_click(
            Some((100, 50)),
            Some(Duration::from_millis(50)),
            100 + CAPTION_REPEAT_SLOP_PX + 1,
            50
        ));
        assert!(!caption_repeat_click(
            None,
            Some(Duration::from_millis(10)),
            0,
            0
        ));
    }

    #[test]
    fn escape_dismisses_without_hover() {
        assert_eq!(caption_escape_decision(true), CaptionEscape::Dismiss);
        assert_eq!(caption_escape_decision(false), CaptionEscape::PassThrough);
    }

    #[test]
    fn palette_open_yields_no_caption_view() {
        let live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        assert!(live.view("Ctrl+Shift+\\").is_some());
        assert!(caption_paint_decision(true, false));
        assert!(
            !caption_paint_decision(true, true),
            "palette/theme/find open hides the caption"
        );
        assert!(!caption_paint_decision(false, false));
    }

    #[test]
    fn host_action_ok_advances_and_error_stays() {
        let mut live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        assert_eq!(
            detect_step(
                &live.current_step().unwrap().expect,
                &Detected::HostAction {
                    action: "split_right".into(),
                    result: "ok".into(),
                }
            ),
            DetectOutcome::Advance
        );
        assert_eq!(
            live.note(&Detected::HostAction {
                action: "focus_right".into(),
                result: "ok".into(),
            }),
            DetectOutcome::Ignore
        );
        assert_eq!(live.current_step().unwrap().id, "window.split-right");
        assert_eq!(
            live.note(&Detected::HostAction {
                action: "split_right".into(),
                result: "err".into(),
            }),
            DetectOutcome::Fail
        );
        assert_eq!(live.current_step().unwrap().id, "window.split-right");
        assert_eq!(live.view("").unwrap().line2, DETECT_FAIL_HINT);
        assert_eq!(
            live.note(&Detected::HostAction {
                action: "split_right".into(),
                result: "ok".into(),
            }),
            DetectOutcome::Advance
        );
        assert_eq!(live.current_step().unwrap().id, "window.focus-right");
        assert_eq!(live.view("C-S-\\").unwrap().line2, "C-S-\\");
    }

    #[test]
    fn command_and_space_expect_match_and_show_me_stubs() {
        let command = Expect::CommandEvent {
            command: "pmux_new".into(),
            result: Some("ok".into()),
        };
        assert_eq!(
            detect_step(
                &command,
                &Detected::CommandEvent {
                    command: "pmux_new".into(),
                    result: "ok".into(),
                }
            ),
            DetectOutcome::Advance
        );
        assert_eq!(
            detect_step(
                &command,
                &Detected::CommandEvent {
                    command: "pmux_new".into(),
                    result: "err".into(),
                }
            ),
            DetectOutcome::Fail
        );
        assert_eq!(
            detect_step(
                &Expect::SpaceEvent {
                    event: "saved".into(),
                },
                &Detected::SpaceEvent {
                    event: "saved".into(),
                }
            ),
            DetectOutcome::Advance
        );
        let mut live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        while live.current_step().map(|s| s.id.as_str()) != Some("seats.new") {
            assert!(live.skip(), "skip to seats.new");
        }
        let stub = live.show_me_stub().expect("seats.new has a command stub");
        assert_eq!(live.note(&stub), DetectOutcome::Advance);
        assert_eq!(live.current_step().unwrap().id, "seats.attach-all");
    }

    fn progress_fixture_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pt-195-progress-{}-{}-{}",
            std::process::id(),
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("walkthrough.json")
    }

    #[test]
    fn progress_round_trip_and_atomic_write_leaves_no_temp() {
        let path = progress_fixture_path("round");
        let catalog = bundled_catalog().unwrap();
        let mut live = WalkthroughLive::start(catalog.clone()).unwrap();
        assert_eq!(
            live.note(&Detected::HostAction {
                action: "split_right".into(),
                result: "ok".into(),
            }),
            DetectOutcome::Advance
        );
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let saved = live.progress(now).unwrap();
        save_progress(&path, &saved).unwrap();
        assert!(path.is_file());
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(&tmp).exists(), "temp must not remain");
        let loaded = load_progress(&path).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.completed, ["window.split-right"]);
        assert!(loaded.skipped.is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resume_index_skips_completed_and_skipped_ids() {
        let catalog = bundled_catalog().unwrap();
        let progress = Progress {
            schema_version: 1,
            current_level: "window".into(),
            current_step: "window.split-right".into(),
            completed: vec!["window.split-right".into()],
            skipped: vec!["window.focus-right".into()],
            updated_at: "1970-01-01T00:00:00Z".into(),
        };
        let (level, step) = resume_index(&catalog, &progress).unwrap();
        assert_ne!(catalog.level[level].step[step].id, "window.split-right");
        assert_ne!(catalog.level[level].step[step].id, "window.focus-right");
        let live = WalkthroughLive::start_with(catalog.clone(), Some(&progress)).unwrap();
        assert_eq!(
            live.current_step().unwrap().id,
            catalog.level[level].step[step].id
        );
        assert!(live.cursor.completed.contains(&"window.split-right".into()));
    }

    #[test]
    fn skip_and_reset_progress() {
        let path = progress_fixture_path("skip");
        let catalog = bundled_catalog().unwrap();
        let mut live = WalkthroughLive::start(catalog.clone()).unwrap();
        let first = live.current_step().unwrap().id.clone();
        assert!(live.skip());
        let saved = live.progress(UNIX_EPOCH).unwrap();
        assert_eq!(saved.skipped, [first.as_str()]);
        save_progress(&path, &saved).unwrap();
        reset_progress(&path).unwrap();
        assert!(!path.exists());
        reset_progress(&path).unwrap();
        let live = WalkthroughLive::start_with(catalog, load_progress(&path).as_ref()).unwrap();
        assert_eq!(live.current_step().unwrap().id, "window.split-right");
        assert!(live.cursor.skipped.is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn note_through_catalog_including_boss_finishes() {
        let mut live = WalkthroughLive::start(bundled_catalog().unwrap()).unwrap();
        loop {
            let fact = scripted_fact(&live.current_step().unwrap().expect);
            match live.note(&fact) {
                DetectOutcome::Advance => {}
                DetectOutcome::Finished => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(live.current_step().is_none() || live.view("").is_none());
    }
}
