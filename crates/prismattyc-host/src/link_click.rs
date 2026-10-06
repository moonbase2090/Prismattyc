//! Link click policy and the press/move/release state used by the host.

use std::time::{Duration, Instant};

use prismattyc_core::HyperlinkId;
use serde::Deserialize;

pub const MULTI_CLICK_MS: u128 = 500;
const OPEN_DELAY: Duration = Duration::from_millis(MULTI_CLICK_MS as u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Plain,
    Modifier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    pub identity: Identity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Osc8 {
        id: HyperlinkId,
        start: usize,
        end: usize,
    },
    Detected {
        start: (usize, usize),
        end: (usize, usize),
    },
}

pub fn open_allowed(
    mode: Mode,
    plain: bool,
    primary_modifier: bool,
    shift: bool,
    mouse_reporting: bool,
) -> bool {
    if shift {
        return false;
    }
    primary_modifier || (mode == Mode::Plain && plain && !mouse_reporting)
}

pub fn drag_threshold(cell_width: usize, cell_height: usize) -> f64 {
    (cell_width.min(cell_height) as f64 / 2.0).max(3.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor<P> {
    pub pane: P,
    pub row: usize,
    pub col: usize,
    pub x: f64,
    pub y: f64,
    pub threshold: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct Press<P> {
    target: TargetKey,
    anchor: Anchor<P>,
    started_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetKey {
    url: String,
    identity: Identity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending<P> {
    target: String,
    pane: P,
    row: usize,
    col: usize,
    deadline: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Release<P> {
    Deferred,
    Select(Anchor<P>),
    Ignore,
}

#[derive(Debug)]
pub struct Gesture<P> {
    press: Option<Press<P>>,
    pending: Vec<Pending<P>>,
    selecting_drag: bool,
}

impl<P> Default for Gesture<P> {
    fn default() -> Self {
        Self {
            press: None,
            pending: Vec::new(),
            selecting_drag: false,
        }
    }
}

impl<P: Copy + PartialEq> Gesture<P> {
    pub fn start(&mut self, target: &Target, anchor: Anchor<P>, now: Instant) {
        self.press = Some(Press {
            target: TargetKey {
                url: target.url.clone(),
                identity: target.identity.clone(),
            },
            anchor,
            started_at: now,
        });
        self.selecting_drag = false;
    }

    pub fn moved(&mut self, x: f64, y: f64) -> Option<Anchor<P>> {
        let press = self.press.as_ref()?;
        if within_threshold(press.anchor, x, y) {
            return None;
        }
        let anchor = self.press.take()?.anchor;
        self.selecting_drag = true;
        Some(anchor)
    }

    pub fn release(
        &mut self,
        pane: P,
        target: Option<&Target>,
        x: f64,
        y: f64,
        now: Instant,
    ) -> Release<P> {
        let Some(press) = self.press.take() else {
            return Release::Ignore;
        };
        let same_target = target.is_some_and(|target| {
            target.url == press.target.url && target.identity == press.target.identity
        });
        if pane == press.anchor.pane && same_target && within_threshold(press.anchor, x, y) {
            self.pending.push(Pending {
                target: press.target.url,
                pane,
                row: press.anchor.row,
                col: press.anchor.col,
                deadline: (press.started_at + OPEN_DELAY).max(now),
            });
            Release::Deferred
        } else {
            self.selecting_drag = true;
            Release::Select(press.anchor)
        }
    }

    pub fn cancel_pending_multi_click(&mut self, pane: P, row: usize, col: usize) {
        self.pending
            .retain(|pending| !(pending.pane == pane && pending.row == row && pending.col == col));
    }

    pub fn on_multi_click(&mut self, pane: P, row: usize, col: usize, clicks: u8) -> bool {
        if !matches!(clicks, 2 | 3) {
            return false;
        }
        self.cancel_pending_multi_click(pane, row, col);
        self.press = None;
        self.selecting_drag = true;
        true
    }

    pub fn cancel_press_for_drag(&mut self) -> Option<Anchor<P>> {
        let anchor = self.press.take()?.anchor;
        self.selecting_drag = true;
        Some(anchor)
    }

    pub fn selecting_drag(&self) -> bool {
        self.selecting_drag
    }

    pub fn owns_pointer(&self) -> bool {
        self.press.is_some() || self.selecting_drag
    }

    pub fn finish_drag(&mut self) -> bool {
        std::mem::take(&mut self.selecting_drag)
    }

    pub fn cancel_press(&mut self) {
        self.press = None;
        self.selecting_drag = false;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|pending| pending.deadline).min()
    }

    pub fn take_due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        let mut later = Vec::with_capacity(self.pending.len());
        for pending in self.pending.drain(..) {
            if pending.deadline <= now {
                due.push(pending.target);
            } else {
                later.push(pending);
            }
        }
        self.pending = later;
        due
    }
}

fn within_threshold<P>(anchor: Anchor<P>, x: f64, y: f64) -> bool {
    let dx = x - anchor.x;
    let dy = y - anchor.y;
    let threshold = anchor.threshold.max(0.0);
    dx.is_finite()
        && dy.is_finite()
        && threshold.is_finite()
        && dx * dx + dy * dy <= threshold * threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(start: usize) -> Target {
        Target {
            url: "https://example.com".into(),
            identity: Identity::Detected {
                start: (0, start),
                end: (0, start + 18),
            },
        }
    }

    fn anchor(x: f64, y: f64) -> Anchor<u8> {
        Anchor {
            pane: 1,
            row: 0,
            col: 2,
            x,
            y,
            threshold: 5.0,
        }
    }

    #[test]
    fn plain_mode_opens_without_modifiers_and_modifier_mode_keeps_the_old_gate() {
        assert!(open_allowed(Mode::Plain, true, false, false, false));
        assert!(open_allowed(Mode::Plain, false, true, false, true));
        assert!(!open_allowed(Mode::Plain, true, false, false, true));
        assert!(!open_allowed(Mode::Modifier, true, false, false, false));
        assert!(open_allowed(Mode::Modifier, false, true, false, true));
        assert!(!open_allowed(Mode::Plain, true, false, true, false));
    }

    #[test]
    fn click_threshold_is_half_a_cell_with_a_small_pixel_floor() {
        assert_eq!(drag_threshold(10, 16), 5.0);
        assert_eq!(drag_threshold(2, 16), 3.0);
    }

    #[test]
    fn release_defers_a_same_link_click_but_a_drag_or_changed_link_selects() {
        let now = Instant::now();
        let first = target(2);
        let mut gesture = Gesture::default();
        gesture.start(&first, anchor(20.0, 10.0), now);
        assert_eq!(gesture.moved(24.0, 10.0), None);
        assert_eq!(
            gesture.release(1, Some(&first), 24.0, 10.0, now),
            Release::Deferred
        );
        assert_eq!(gesture.take_due(now + OPEN_DELAY), vec![first.url.clone()]);

        gesture.start(&first, anchor(20.0, 10.0), now);
        assert_eq!(gesture.moved(26.0, 10.0), Some(anchor(20.0, 10.0)));
        assert!(gesture.selecting_drag());
        assert!(gesture.finish_drag());

        gesture.start(&first, anchor(20.0, 10.0), now);
        assert_eq!(
            gesture.release(1, Some(&target(30)), 20.0, 10.0, now),
            Release::Select(anchor(20.0, 10.0))
        );
    }

    #[test]
    fn second_or_third_click_cancels_the_first_open() {
        for clicks in [2, 3] {
            let now = Instant::now();
            let first = target(2);
            let mut gesture = Gesture::default();
            gesture.start(&first, anchor(20.0, 10.0), now);
            assert_eq!(
                gesture.release(1, Some(&first), 20.0, 10.0, now),
                Release::Deferred
            );
            assert!(gesture.on_multi_click(1, 0, 2, clicks));
            assert!(gesture.selecting_drag());
            assert!(gesture.take_due(now + OPEN_DELAY).is_empty());
            assert!(gesture.finish_drag());
        }
    }
}
