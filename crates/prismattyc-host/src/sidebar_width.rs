//! Sidebar width, collapse, and icon-strip geometry (issue #174).
//!
//! Dragging, snapping, and hit-testing live here so the host can reflow and
//! paint without owning the rules. Nothing in this module reads the tree
//! labels: a long name never changes the width.

use crate::space_rail::RailSide;

/// Expanded width when the user has not dragged and has not saved a width.
pub const DEFAULT_DESIGN_PX: f32 = 256.0;
/// Short names stay readable. Narrower than this snaps the strip closed.
pub const MIN_DESIGN_PX: f32 = 200.0;
/// Fixed icon-strip width. It does not grow with the number of icons.
pub const COLLAPSED_DESIGN_PX: f32 = 52.0;
/// Pane area kept free of the sidebar, in physical pixels.
pub const MIN_PANE_PX: f32 = 320.0;
/// Pointer slop around the grip, matching the spaces-rail grip.
pub const GRIP_SLOP_PX: f32 = 5.0;
/// Second press on the grip inside this window resets the width.
pub const DOUBLE_CLICK_MS: u128 = 400;
/// Minimum gap between pane reflows while the grip is held.
pub const REFLOW_MS: u128 = 100;
/// Largest expanded width stored in the config, before the window clamp.
pub const MAX_STORED_PX: i64 = 2000;

/// Which edge the sidebar occupies. Only `space_rail = "right"` docks it right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dock {
    Left,
    Right,
}

/// Saved sidebar geometry. `expanded_px` is the width to restore, never the
/// collapsed strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    pub expanded_px: f32,
    pub collapsed: bool,
    pub dock: Dock,
}

impl Chrome {
    /// Design width currently on screen.
    pub fn design_px(self) -> f32 {
        if self.collapsed {
            COLLAPSED_DESIGN_PX
        } else {
            self.expanded_px
        }
    }

    /// Physical column width. `window_px == 0` skips the pane-room clamp
    /// (tests and the first frame, before the window size is known).
    pub fn physical_px(self, scale_milli: u32, window_px: f32) -> usize {
        let design = if self.collapsed {
            self.design_px()
        } else {
            clamp_expanded(self.expanded_px, window_px, scale_milli)
        };
        physical(design, scale_milli)
    }

    pub fn toggle(self) -> Self {
        Self {
            collapsed: !self.collapsed,
            ..self
        }
    }

    /// Double-click: the default expanded width, open.
    pub fn reset_default(self) -> Self {
        Self {
            expanded_px: DEFAULT_DESIGN_PX,
            collapsed: false,
            dock: self.dock,
        }
    }
}

/// Right only when the spaces rail is docked on the right. Bottom, top, left,
/// and off keep the sidebar on the left.
pub fn dock_for_rail(side: RailSide) -> Dock {
    if side == RailSide::Right {
        Dock::Right
    } else {
        Dock::Left
    }
}

pub fn stored_width_ok(px: u32) -> bool {
    (MIN_DESIGN_PX as u32..=MAX_STORED_PX as u32).contains(&px)
}

/// Clamp an expanded design width. A tiny window cannot go below
/// [`MIN_DESIGN_PX`]; the panes then keep whatever room is left.
pub fn clamp_expanded(design_px: f32, window_px: f32, scale_milli: u32) -> f32 {
    let scale = scale_of(scale_milli);
    let min = MIN_DESIGN_PX;
    let mut max = MAX_STORED_PX as f32;
    if window_px.is_finite() && window_px > 0.0 {
        let max_phys = (window_px - MIN_PANE_PX).max(min * scale);
        max = max.min(max_phys / scale);
    }
    if max < min {
        max = min;
    }
    if !design_px.is_finite() {
        return DEFAULT_DESIGN_PX.clamp(min, max);
    }
    design_px.clamp(min, max)
}

/// Absent width is the default. A stored width outside the config range is
/// pulled back in so a hand-edited file cannot zero the column.
pub fn from_persisted(width: Option<i64>, collapsed: bool, dock: Dock) -> Chrome {
    let expanded = match width {
        Some(px) if px > 0 => (px as f32).clamp(MIN_DESIGN_PX, MAX_STORED_PX as f32),
        _ => DEFAULT_DESIGN_PX,
    };
    Chrome {
        expanded_px: expanded,
        collapsed,
        dock,
    }
}

pub fn persisted(chrome: Chrome) -> (i64, bool) {
    (chrome.expanded_px.round() as i64, chrome.collapsed)
}

/// The width does not read the labels. Callers pass them so a test can prove it.
#[cfg(test)]
pub fn design_px_for(chrome: Chrome, _labels: &[&str]) -> f32 {
    chrome.design_px()
}

fn scale_of(scale_milli: u32) -> f32 {
    (scale_milli.max(1) as f32) / 1000.0
}

fn physical(design: f32, scale_milli: u32) -> usize {
    (design * scale_of(scale_milli)).round().max(1.0) as usize
}

fn pointer_design(dock: Dock, pointer_x: f32, window_px: f32, scale_milli: u32) -> f32 {
    let phys = match dock {
        Dock::Left => pointer_x,
        Dock::Right => window_px - pointer_x,
    };
    phys / scale_of(scale_milli)
}

/// Grip along the inner edge: the right edge of a left sidebar, the left
/// edge of a right sidebar.
pub fn hits_grip(
    dock: Dock,
    column_x: f32,
    column_w: f32,
    column_y: f32,
    column_h: f32,
    pointer_x: f32,
    pointer_y: f32,
) -> bool {
    if !pointer_x.is_finite() || !pointer_y.is_finite() || column_w <= 0.0 || column_h <= 0.0 {
        return false;
    }
    if pointer_y < column_y || pointer_y >= column_y + column_h {
        return false;
    }
    let edge = match dock {
        Dock::Left => column_x + column_w,
        Dock::Right => column_x,
    };
    (pointer_x - edge).abs() <= GRIP_SLOP_PX
}

#[cfg(test)]
pub fn double_click(previous_ms: Option<u128>, now_ms: u128) -> bool {
    previous_ms.is_some_and(|previous| now_ms.saturating_sub(previous) <= DOUBLE_CLICK_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragEvent {
    /// Pointer moved, but the physical width is unchanged.
    Hold,
    /// Width changed inside the reflow gap. The host keeps the last applied width.
    Preview,
    /// Apply this width and reflow the panes once.
    Reflow,
}

/// One grip drag. The expanded width remembered at press is what a snap
/// restores; the undersized pointer position is never stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drag {
    origin_expanded: f32,
    chrome: Chrome,
    scale_milli: u32,
    window_px: f32,
    last_reflow_ms: u128,
    last_applied_phys: usize,
}

impl Drag {
    pub fn start(chrome: Chrome, scale_milli: u32, window_px: f32, now_ms: u128) -> Self {
        Self {
            origin_expanded: chrome.expanded_px,
            chrome,
            scale_milli,
            window_px,
            last_reflow_ms: now_ms,
            last_applied_phys: chrome.physical_px(scale_milli, window_px),
        }
    }

    pub fn chrome(self) -> Chrome {
        self.chrome
    }

    pub fn pointer(&mut self, pointer_x: f32, now_ms: u128) -> DragEvent {
        if !pointer_x.is_finite() {
            return DragEvent::Hold;
        }
        let design = pointer_design(
            self.chrome.dock,
            pointer_x,
            self.window_px,
            self.scale_milli,
        );
        let was_collapsed = self.chrome.collapsed;
        if design < MIN_DESIGN_PX {
            self.chrome.collapsed = true;
            self.chrome.expanded_px = self.origin_expanded;
        } else {
            self.chrome.collapsed = false;
            self.chrome.expanded_px = clamp_expanded(design, self.window_px, self.scale_milli);
        }
        let phys = self.chrome.physical_px(self.scale_milli, self.window_px);
        if phys == self.last_applied_phys {
            return DragEvent::Hold;
        }
        let snapped = self.chrome.collapsed && !was_collapsed;
        let due = snapped || now_ms.saturating_sub(self.last_reflow_ms) >= REFLOW_MS;
        if due {
            self.last_reflow_ms = now_ms;
            self.last_applied_phys = phys;
            DragEvent::Reflow
        } else {
            DragEvent::Preview
        }
    }

    /// Commit the pending width. `true` when the host still owes a reflow.
    pub fn release(self) -> (Chrome, bool) {
        let phys = self.chrome.physical_px(self.scale_milli, self.window_px);
        (self.chrome, phys != self.last_applied_phys)
    }
}

/// Icon family for the collapsed strip. Space rows are always [`Seat::Space`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seat {
    Space,
    Shell,
    Claude,
    Codex,
    Grok,
    Muse,
    Composer,
}

pub fn seat_for(space: bool, label: &str) -> Seat {
    if space {
        return Seat::Space;
    }
    let lower = label.to_ascii_lowercase();
    if lower.contains("claude") {
        Seat::Claude
    } else if lower.contains("codex") {
        Seat::Codex
    } else if lower.contains("grok") {
        Seat::Grok
    } else if lower.contains("muse") {
        Seat::Muse
    } else if lower.contains("composer") {
        Seat::Composer
    } else {
        Seat::Shell
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxBox {
    pub fn contains(self, x: i32, y: i32) -> bool {
        self.w > 0
            && self.h > 0
            && x >= self.x
            && y >= self.y
            && x < self.x + self.w
            && y < self.y + self.h
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconStrip {
    pub toggle: PxBox,
    /// Visible icons: absolute row index, then the hit box.
    pub icons: Vec<(usize, PxBox)>,
    pub actions: [PxBox; 3],
    pub list: PxBox,
    pub first: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconTarget {
    Toggle,
    /// Index into [`IconStrip::icons`], not the absolute row.
    Row(usize),
    Action(usize),
}

#[allow(clippy::too_many_arguments)] // column box plus the three row metrics
pub fn icon_strip(
    column_x: i32,
    column_w: i32,
    column_h: i32,
    head_h: i32,
    row_h: i32,
    action_h: i32,
    row_count: usize,
    scroll: usize,
) -> IconStrip {
    let column_w = column_w.max(0);
    let column_h = column_h.max(0);
    let head_h = head_h.clamp(0, column_h);
    let action_h = action_h.max(0);
    let row_h = row_h.max(1);
    let foot_pad = (action_h / 4).max(0);
    let body = column_h.saturating_sub(head_h);
    let mut foot_h = action_h
        .saturating_mul(3)
        .saturating_add(foot_pad)
        .min(body);
    // A short window still shows one icon. The footer gives up room first.
    if row_count > 0 {
        let min_list = row_h.min(body);
        if body.saturating_sub(foot_h) < min_list {
            foot_h = body.saturating_sub(min_list);
        }
    }
    let foot_y = column_h - foot_h;
    let list_h = foot_y.saturating_sub(head_h).max(0);
    let visible = (list_h / row_h).max(1) as usize;
    let max_scroll = row_count.saturating_sub(visible.min(row_count.max(1)));
    let first = scroll.min(max_scroll).min(row_count);
    // The empty strip stored before the first layout, and a very short row,
    // can put the preferred maximum under the preferred minimum. `clamp`
    // panics on that, which took down every real window at startup (#174).
    let icon_hi = (row_h - 2).max(1);
    let icon = (column_w - 12).clamp(8.min(icon_hi), icon_hi);
    let toggle_hi = (column_w - 12).max(1);
    let toggle_size = (head_h - 8).clamp(8.min(toggle_hi), toggle_hi);
    let toggle = PxBox {
        x: column_x + (column_w - toggle_size) / 2,
        y: (head_h - toggle_size) / 2,
        w: toggle_size.min(column_w),
        h: toggle_size.min(head_h),
    };
    let mut icons = Vec::new();
    for index in 0..row_count.saturating_sub(first) {
        let absolute = first + index;
        let y = head_h + (index as i32) * row_h;
        if y + row_h > head_h + list_h {
            break;
        }
        icons.push((
            absolute,
            PxBox {
                x: column_x + (column_w - icon) / 2,
                y: y + (row_h - icon) / 2,
                w: icon,
                h: icon,
            },
        ));
    }
    let mut actions = [PxBox {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    }; 3];
    for (index, slot) in actions.iter_mut().enumerate() {
        let y = foot_y + foot_pad + (index as i32) * action_h;
        if y >= column_h || foot_h == 0 {
            continue;
        }
        *slot = PxBox {
            x: column_x + (column_w - icon) / 2,
            y,
            w: icon,
            h: action_h.min(column_h - y),
        };
    }
    IconStrip {
        toggle,
        icons,
        actions,
        list: PxBox {
            x: column_x,
            y: head_h,
            w: column_w,
            h: list_h,
        },
        first,
    }
}

pub fn icon_hit(strip: &IconStrip, x: i32, y: i32) -> Option<IconTarget> {
    if strip.toggle.contains(x, y) {
        return Some(IconTarget::Toggle);
    }
    if let Some(index) = strip.actions.iter().position(|slot| slot.contains(x, y)) {
        return Some(IconTarget::Action(index));
    }
    strip
        .icons
        .iter()
        .position(|(_, slot)| slot.contains(x, y))
        .map(IconTarget::Row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(width: f32) -> Chrome {
        Chrome {
            expanded_px: width,
            collapsed: false,
            dock: Dock::Left,
        }
    }

    #[test]
    fn clamp_keeps_names_readable_and_leaves_pane_room() {
        assert_eq!(clamp_expanded(80.0, 1600.0, 1000), MIN_DESIGN_PX);
        assert_eq!(clamp_expanded(400.0, 1600.0, 1000), 400.0);
        // 1600 - 320 = 1280 physical, which is also the design max here.
        assert_eq!(clamp_expanded(1500.0, 1600.0, 1000), 1280.0);
        // 2x: 1000 physical leaves 680, which is 340 design px.
        assert_eq!(clamp_expanded(500.0, 1000.0, 2000), 340.0);
        // A tiny window cannot shrink past the minimum.
        assert_eq!(clamp_expanded(400.0, 200.0, 1000), MIN_DESIGN_PX);
        assert_eq!(clamp_expanded(f32::NAN, 1600.0, 1000), DEFAULT_DESIGN_PX);
    }

    #[test]
    fn width_is_independent_of_labels() {
        let chrome = open(280.0);
        assert_eq!(design_px_for(chrome, &["a"]), 280.0);
        assert_eq!(
            design_px_for(chrome, &["a very long session name that would ellipsize"]),
            design_px_for(chrome, &["a"])
        );
    }

    #[test]
    fn absent_width_is_the_default_and_collapsed_is_remembered() {
        let fresh = from_persisted(None, false, Dock::Left);
        assert_eq!(fresh.expanded_px, DEFAULT_DESIGN_PX);
        assert!(!fresh.collapsed);
        let saved = from_persisted(Some(10), true, Dock::Right);
        assert_eq!(saved.expanded_px, MIN_DESIGN_PX);
        assert!(saved.collapsed);
        assert_eq!(saved.dock, Dock::Right);
        assert_eq!(persisted(saved), (MIN_DESIGN_PX as i64, true));
    }

    #[test]
    fn toggle_restores_the_dragged_width_and_double_click_resets() {
        let dragged = open(340.0);
        let collapsed = dragged.toggle();
        assert!(collapsed.collapsed);
        assert_eq!(collapsed.expanded_px, 340.0);
        assert_eq!(collapsed.design_px(), COLLAPSED_DESIGN_PX);
        let restored = collapsed.toggle();
        assert_eq!(restored, dragged);
        let reset = collapsed.reset_default();
        assert!(!reset.collapsed);
        assert_eq!(reset.expanded_px, DEFAULT_DESIGN_PX);
    }

    #[test]
    fn snap_remembers_the_width_at_press_and_does_not_reflow_every_pixel() {
        let mut drag = Drag::start(open(320.0), 1000, 1600.0, 0);
        let mut previews = 0;
        let mut reflows = 0;
        for x in 300..360 {
            match drag.pointer(x as f32, 20) {
                DragEvent::Preview => previews += 1,
                DragEvent::Reflow => reflows += 1,
                DragEvent::Hold => {}
            }
        }
        assert_eq!(reflows, 0, "inside the reflow gap the panes stay put");
        assert!(previews > 0);
        assert_eq!(drag.pointer(340.0, 20 + REFLOW_MS), DragEvent::Reflow);
        assert_eq!(drag.chrome().expanded_px, 340.0);

        let mut snap = Drag::start(open(320.0), 1000, 1600.0, 0);
        assert_eq!(snap.pointer(120.0, 5), DragEvent::Reflow);
        assert!(snap.chrome().collapsed);
        assert_eq!(
            snap.chrome().expanded_px,
            320.0,
            "the undersized pointer is not the restored width"
        );
        // Dragging back above the minimum follows the pointer.
        assert_eq!(snap.pointer(280.0, 5 + REFLOW_MS), DragEvent::Reflow);
        assert!(!snap.chrome().collapsed);
        assert_eq!(snap.chrome().expanded_px, 280.0);
        let (released, owe) = snap.release();
        assert_eq!(released.expanded_px, 280.0);
        assert!(!owe);

        let mut pending = Drag::start(open(320.0), 1000, 1600.0, 0);
        assert_eq!(pending.pointer(300.0, 10), DragEvent::Preview);
        let (chrome, owe) = pending.release();
        assert!(owe, "release applies the width the throttle skipped");
        assert_eq!(chrome.expanded_px, 300.0);
    }

    #[test]
    fn right_dock_grip_is_the_left_edge_and_drag_measures_from_the_right() {
        let column_x = 1600.0 - 256.0;
        assert!(hits_grip(
            Dock::Right,
            column_x,
            256.0,
            0.0,
            800.0,
            column_x,
            40.0
        ));
        assert!(hits_grip(
            Dock::Right,
            column_x,
            256.0,
            0.0,
            800.0,
            column_x + GRIP_SLOP_PX,
            40.0
        ));
        assert!(!hits_grip(
            Dock::Right,
            column_x,
            256.0,
            0.0,
            800.0,
            column_x + 40.0,
            40.0
        ));
        assert!(hits_grip(Dock::Left, 0.0, 256.0, 0.0, 800.0, 256.0, 10.0));
        assert!(!hits_grip(Dock::Left, 0.0, 256.0, 0.0, 800.0, 0.0, 10.0));

        let mut drag = Drag::start(
            Chrome {
                expanded_px: 256.0,
                collapsed: false,
                dock: Dock::Right,
            },
            1000,
            1600.0,
            0,
        );
        assert_eq!(drag.pointer(1600.0 - 300.0, REFLOW_MS), DragEvent::Reflow);
        assert_eq!(drag.chrome().expanded_px, 300.0);
        assert_eq!(drag.pointer(1600.0 - 40.0, REFLOW_MS), DragEvent::Reflow);
        assert!(drag.chrome().collapsed);
        assert_eq!(drag.chrome().expanded_px, 256.0);
    }

    #[test]
    fn double_click_window_is_four_hundred_milliseconds() {
        assert!(!double_click(None, 1_000));
        assert!(double_click(Some(1_000), 1_400));
        assert!(!double_click(Some(1_000), 1_401));
    }

    #[test]
    fn seat_icons_follow_the_label_and_spaces_stay_spaces() {
        assert_eq!(seat_for(true, "Claude"), Seat::Space);
        assert_eq!(seat_for(false, "Claude"), Seat::Claude);
        assert_eq!(seat_for(false, "codex-review"), Seat::Codex);
        assert_eq!(seat_for(false, "Grok"), Seat::Grok);
        assert_eq!(seat_for(false, "muse"), Seat::Muse);
        assert_eq!(seat_for(false, "Composer 1"), Seat::Composer);
        assert_eq!(seat_for(false, "zsh"), Seat::Shell);
    }

    #[test]
    fn empty_icon_strip_survives_the_metrics_stored_before_layout() {
        let strip = icon_strip(0, 0, 0, 0, 1, 0, 0, 0);
        assert!(strip.icons.is_empty());
        assert_eq!(strip.toggle.w, 0);
        assert_eq!(strip.toggle.h, 0);
        assert_eq!(icon_hit(&strip, 0, 0), None);
        // A short row used to invert the icon clamp (row height minus 2 < 8).
        let short = icon_strip(0, 52, 80, 20, 8, 16, 1, 0);
        assert_eq!(short.icons.len(), 1);
        let slot = short.icons[0].1;
        assert!(slot.w > 0 && slot.h > 0);
        assert!(slot.y >= 0 && slot.y + slot.h <= 80);
    }

    #[test]
    fn icon_hit_testing_scrolls_inside_a_fixed_strip() {
        let strip = icon_strip(0, 52, 400, 44, 36, 32, 3, 0);
        assert_eq!(strip.icons.len(), 3);
        assert!(strip
            .icons
            .iter()
            .all(|(_, slot)| slot.x >= 0 && slot.x + slot.w <= 52));
        let (_, first) = strip.icons[0];
        assert_eq!(
            icon_hit(&strip, first.x + 1, first.y + 1),
            Some(IconTarget::Row(0))
        );
        assert_eq!(
            icon_hit(&strip, strip.toggle.x + 1, strip.toggle.y + 1),
            Some(IconTarget::Toggle)
        );
        let action = strip.actions[2];
        assert_eq!(
            icon_hit(&strip, action.x + 1, action.y + 1),
            Some(IconTarget::Action(2))
        );
        assert_eq!(icon_hit(&strip, 200, 10), None);

        let scrolled = icon_strip(10, 52, 160, 44, 36, 32, 20, 100);
        assert_eq!(scrolled.first, scrolled.icons[0].0);
        assert!(scrolled.first > 0);
        assert_eq!(scrolled.list.w, 52);
        let wide = icon_strip(10, 52, 160, 44, 36, 32, 2, 0);
        assert_eq!(wide.list.w, scrolled.list.w, "the strip never grows");
        assert!(scrolled.icons.iter().all(|(_, slot)| {
            slot.x >= scrolled.list.x && slot.x + slot.w <= scrolled.list.x + scrolled.list.w
        }));
    }

    #[test]
    fn persistence_round_trips_through_the_config_file() {
        let dir =
            std::env::temp_dir().join(format!("prismattyc-sidebar-width-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        crate::config::save_preference(&path, "sidebar_width_px", toml_edit::value(320_i64))
            .unwrap();
        crate::config::save_preference(&path, "sidebar_collapsed", toml_edit::value(true)).unwrap();
        let loaded = crate::config::load(&path).unwrap();
        let chrome = from_persisted(
            loaded.sidebar_width_px.map(|px| px as i64),
            loaded.sidebar_collapsed.unwrap_or(false),
            Dock::Left,
        );
        assert_eq!((chrome.expanded_px, chrome.collapsed), (320.0, true));
        let missing = dir.join("missing.toml");
        let absent = crate::config::load(&missing).unwrap();
        let chrome = from_persisted(
            absent.sidebar_width_px.map(|px| px as i64),
            false,
            Dock::Left,
        );
        assert_eq!(chrome.expanded_px, DEFAULT_DESIGN_PX);
        assert!(!chrome.collapsed);
        std::fs::write(&path, "sidebar_width_px = 10\n").unwrap();
        assert!(crate::config::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
