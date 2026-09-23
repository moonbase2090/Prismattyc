//! Pixel-space damage retained by presentation backends.
//!
//! The compositor passes the returned [`FrameDamage`] to the PT-244 aged
//! buffer repair layer. The layer unions this frame with every newer frame
//! that the selected wl_shm slot missed (including age 2 and age 3 slots)
//! before copying pixels. A pixel that is outside that union keeps the value
//! already present in the aged slot.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl PixelRect {
    pub(crate) fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) fn clipped(self, width: usize, height: usize) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let right = self.x.saturating_add(self.width).min(width);
        let bottom = self.y.saturating_add(self.height).min(height);
        (x < right && y < bottom).then_some(Self::new(x, y, right - x, bottom - y))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameDamage {
    Full,
    Rects(Vec<PixelRect>),
}

impl FrameDamage {
    pub(crate) fn rects() -> Self {
        Self::Rects(Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn rects_slice(&self) -> &[PixelRect] {
        match self {
            Self::Full => &[],
            Self::Rects(rects) => rects,
        }
    }

    /// Add a rectangle and merge vertically adjacent terminal row bands.
    pub(crate) fn push_rect(&mut self, rect: PixelRect) {
        let Self::Rects(rects) = self else {
            return;
        };
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        if rects.contains(&rect) {
            return;
        }
        if let Some(last) = rects.last_mut() {
            if last.x == rect.x
                && last.width == rect.width
                && last.y.saturating_add(last.height) == rect.y
            {
                last.height = last.height.saturating_add(rect.height);
                return;
            }
        }
        rects.push(rect);
    }
}

/// A pane's stable id and its two pixel boxes in one layout snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneLayoutSnapshot {
    pub(crate) id: u64,
    pub(crate) slot: PixelRect,
    pub(crate) content: PixelRect,
}

/// Layout geometry used by the pure frame-damage composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutSnapshot {
    pub(crate) panes: Vec<PaneLayoutSnapshot>,
}

/// Host chrome that can move without changing pane geometry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChromeSnapshot {
    pub(crate) focused_pane: Option<u64>,
    pub(crate) focused_slot: Option<PixelRect>,
    /// Known title, badge, divider, and rail boxes. Unknown chrome belongs in
    /// the caller's full-frame fallback.
    pub(crate) boxes: Vec<PixelRect>,
    /// State tokens for known boxes (mail, unseen, and active chips).
    pub(crate) markers: Vec<(u64, u64)>,
    /// Quantized active-dot pulse step. A changed step repaints the dot boxes.
    pub(crate) pulse_step: Option<u8>,
    /// Quantized focus-border sweep step. A live step repaints the edge strips.
    pub(crate) light_cycle_step: Option<u8>,
    /// Signature for chrome that the bounded composer does not map to boxes.
    /// A changed signature requires a conservative full repaint.
    pub(crate) unbounded_state: String,
}

/// Damage for one pane in cell-row coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneDamageSnapshot {
    pub(crate) pane_id: u64,
    pub(crate) content: PixelRect,
    pub(crate) row_height: usize,
    pub(crate) dirty_rows: Vec<usize>,
    /// A scroll blit copies this box before the dirty rows are rasterized.
    pub(crate) blit: Option<PixelRect>,
}

/// Fixed chrome damage budget for the PT-289 four-pane box.
///
/// The value covers the measured active chip, pane handle, and divider area
/// in the four-pane reference layout. A mapper that falls back to the whole
/// 1600x900 frame exceeds this bound and is therefore rejected.
pub(crate) const CHROME_DAMAGE_BUDGET_PX: usize = 32 * 1024;

pub(crate) fn layout_transition(prior: Option<&LayoutSnapshot>, current: &LayoutSnapshot) -> bool {
    let Some(prior) = prior else {
        return true;
    };
    if prior.panes.len() != current.panes.len() {
        return true;
    }
    let mut old_slots: Vec<_> = prior
        .panes
        .iter()
        .map(|pane| (pane.id, pane.slot, pane.content))
        .collect();
    let mut new_slots: Vec<_> = current
        .panes
        .iter()
        .map(|pane| (pane.id, pane.slot, pane.content))
        .collect();
    old_slots.sort_unstable_by_key(|(id, slot, content)| {
        (
            *id,
            slot.y,
            slot.x,
            slot.width,
            slot.height,
            content.y,
            content.x,
            content.width,
            content.height,
        )
    });
    new_slots.sort_unstable_by_key(|(id, slot, content)| {
        (
            *id,
            slot.y,
            slot.x,
            slot.width,
            slot.height,
            content.y,
            content.x,
            content.width,
            content.height,
        )
    });
    old_slots != new_slots
}

pub(crate) fn chrome_geometry_changed(
    prior: Option<&ChromeSnapshot>,
    current: &ChromeSnapshot,
) -> bool {
    prior.is_some_and(|prior| {
        prior.boxes != current.boxes
            || prior.markers != current.markers
            || prior.pulse_step != current.pulse_step
            || prior.light_cycle_step != current.light_cycle_step
            || prior.unbounded_state != current.unbounded_state
    })
}

fn pane_slot(prior: Option<&LayoutSnapshot>, id: u64) -> Option<PixelRect> {
    prior?
        .panes
        .iter()
        .find_map(|pane| (pane.id == id).then_some(pane.slot))
}

fn push_chrome_box_damage(
    damage: &mut FrameDamage,
    prior: Option<&ChromeSnapshot>,
    current: &ChromeSnapshot,
) -> bool {
    let changed = prior.is_some_and(|prior| {
        prior.boxes != current.boxes
            || prior.markers != current.markers
            || prior.pulse_step != current.pulse_step
    });
    if !changed {
        return false;
    }
    if let Some(prior) = prior {
        for rect in &prior.boxes {
            damage.push_rect(*rect);
        }
    }
    for rect in &current.boxes {
        damage.push_rect(*rect);
    }
    true
}

fn chrome_boxes_area(prior: Option<&ChromeSnapshot>, current: &ChromeSnapshot) -> usize {
    prior
        .map(|chrome| chrome.boxes.as_slice())
        .unwrap_or_default()
        .iter()
        .chain(current.boxes.iter())
        .fold(0, |area, rect| {
            area.saturating_add(rect.width.saturating_mul(rect.height))
        })
}

/// Compose pane and known chrome damage without reading host state.
///
/// Layout transitions are full because old and new pane surfaces can expose
/// different gaps, dividers, and overlays. Focus moves and known chrome box
/// changes remain bounded to the old and new boxes. Unknown or over-budget
/// chrome returns a full frame.
pub(crate) fn compose_frame_damage(
    prior_layout: Option<&LayoutSnapshot>,
    current_layout: &LayoutSnapshot,
    prior_chrome: Option<&ChromeSnapshot>,
    current_chrome: &ChromeSnapshot,
    panes: &[PaneDamageSnapshot],
    full: bool,
) -> FrameDamage {
    if full || layout_transition(prior_layout, current_layout) {
        return FrameDamage::Full;
    }

    if prior_chrome.is_some_and(|prior| prior.unbounded_state != current_chrome.unbounded_state) {
        return FrameDamage::Full;
    }

    let mut damage = FrameDamage::rects();
    let mut by_id = HashMap::new();
    for pane in &current_layout.panes {
        by_id.insert(pane.id, pane.content);
    }
    for pane in panes {
        let Some(content) = by_id.get(&pane.pane_id).copied() else {
            return FrameDamage::Full;
        };
        if let Some(blit) = pane.blit {
            damage.push_rect(blit);
        }
        for row in &pane.dirty_rows {
            let y = content
                .y
                .saturating_add(row.saturating_mul(pane.row_height));
            let row_height = pane
                .row_height
                .min(content.y.saturating_add(content.height).saturating_sub(y));
            damage.push_rect(PixelRect::new(content.x, y, content.width, row_height));
        }
    }

    if let Some(current_focus) = current_chrome.focused_pane {
        let prior_focus = prior_chrome.and_then(|chrome| chrome.focused_pane);
        if prior_focus != Some(current_focus) {
            if let Some(prior_focus) = prior_focus.and_then(|id| pane_slot(prior_layout, id)) {
                damage.push_rect(prior_focus);
            }
            if let Some(current_focus) = current_chrome.focused_slot {
                damage.push_rect(current_focus);
            }
        }
    }

    let light_cycle_changed =
        prior_chrome.and_then(|chrome| chrome.light_cycle_step) != current_chrome.light_cycle_step;
    if current_chrome.light_cycle_step.is_some() || light_cycle_changed {
        if let Some(prior_slot) = prior_chrome.and_then(|chrome| chrome.focused_slot) {
            for strip in border_strips(prior_slot) {
                damage.push_rect(strip);
            }
        }
        if let Some(current_slot) = current_chrome.focused_slot {
            for strip in border_strips(current_slot) {
                damage.push_rect(strip);
            }
        }
    }

    let chrome_changed = push_chrome_box_damage(&mut damage, prior_chrome, current_chrome);
    if chrome_changed && chrome_boxes_area(prior_chrome, current_chrome) > CHROME_DAMAGE_BUDGET_PX {
        return FrameDamage::Full;
    }
    damage
}

/// The animated head can extend seven pixels inward from the slot edge.
/// Keep this bound shared with the painter, including at clipped corners.
pub(crate) const BORDER_HEAD_SIZE: usize = 7;

/// Disjoint edge strips, including the full footprint of the animated head.
/// Tiny slots collapse to a full slot without overlap or underflow.
pub(crate) fn border_strips(slot: PixelRect) -> [PixelRect; 4] {
    let top = BORDER_HEAD_SIZE.min(slot.height);
    let bottom = BORDER_HEAD_SIZE.min(slot.height - top);
    let middle = slot.height - top - bottom;
    let left = BORDER_HEAD_SIZE.min(slot.width);
    let right = BORDER_HEAD_SIZE.min(slot.width - left);
    [
        PixelRect::new(slot.x, slot.y, slot.width, top),
        PixelRect::new(
            slot.x,
            slot.y.saturating_add(slot.height - bottom),
            slot.width,
            bottom,
        ),
        PixelRect::new(slot.x, slot.y.saturating_add(top), left, middle),
        PixelRect::new(
            slot.x.saturating_add(slot.width - right),
            slot.y.saturating_add(top),
            right,
            middle,
        ),
    ]
}

/// Quantized pulse steps used by chrome snapshots and paint.
pub(crate) const PULSE_STEPS: u8 = 16;
pub(crate) const STRIP_TAB_MARKER_PREFIX: u64 = 1 << 63;
pub(crate) const STRIP_HANDLE_MARKER_PREFIX: u64 = 1 << 62;
const MAIL_BADGE_PX: usize = 20;
const UNSEEN_BADGE_W: usize = 20;
const UNSEEN_BADGE_H: usize = 16;
const ACTIVE_DOT_W: usize = 12;
const ACTIVE_DOT_H: usize = 16;

/// Map a quantized pulse step onto the 0..1 paint phase.
pub(crate) fn pulse_phase_for_step(step: u8) -> f32 {
    f32::from(step % PULSE_STEPS) / f32::from(PULSE_STEPS)
}

/// True when the window may show a live pulse.
pub(crate) fn pulse_live(window_focused: bool, window_occluded: bool) -> bool {
    window_focused && !window_occluded
}

/// Snapshot the pulse step only while an active pane is live and visible.
pub(crate) fn pulse_step_for_snapshot(active_count: usize, live: bool, step: u8) -> Option<u8> {
    (active_count > 0 && live).then_some(step)
}

/// Snapshot the light-cycle step only while the sweep is armed.
pub(crate) fn light_cycle_step_for_snapshot(border_anim_live: bool, step: u8) -> Option<u8> {
    border_anim_live.then_some(step)
}

/// Pulse phase used by strip and pane paint when the animation is live.
pub(crate) fn pulse_phase_if(live: bool, step: u8) -> Option<f32> {
    live.then(|| pulse_phase_for_step(step))
}

/// True when `rect` overlaps the composed frame damage.
pub(crate) fn frame_damage_intersects(damage: &FrameDamage, rect: PixelRect) -> bool {
    let FrameDamage::Rects(rects) = damage else {
        return true;
    };
    rects.iter().any(|other| {
        other.x < rect.x.saturating_add(rect.width)
            && rect.x < other.x.saturating_add(other.width)
            && other.y < rect.y.saturating_add(rect.height)
            && rect.y < other.y.saturating_add(other.height)
    })
}

/// True when one damage rectangle covers the complete pane surface.
pub(crate) fn frame_damage_covers(damage: &FrameDamage, slot: PixelRect) -> bool {
    let FrameDamage::Rects(rects) = damage else {
        return true;
    };
    rects.iter().any(|rect| {
        rect.x <= slot.x
            && rect.y <= slot.y
            && rect.x.saturating_add(rect.width) >= slot.x.saturating_add(slot.width)
            && rect.y.saturating_add(rect.height) >= slot.y.saturating_add(slot.height)
    })
}

/// True when a pane must run its paint path for this frame.
pub(crate) fn pane_paint_required(
    paint_rows_empty: bool,
    damage: &FrameDamage,
    slot: PixelRect,
) -> bool {
    !paint_rows_empty || frame_damage_intersects(damage, slot)
}

/// True when the composer returned Full after a partial attempt.
pub(crate) fn composer_promotes_to_full(already_full: bool, damage: &FrameDamage) -> bool {
    !already_full && matches!(damage, FrameDamage::Full)
}

/// True when a partial frame has no rects and no leftover pane damage.
pub(crate) fn empty_partial_skips_paint(
    already_full: bool,
    damage: &FrameDamage,
    pane_damage_empty: bool,
) -> bool {
    !already_full
        && matches!(damage, FrameDamage::Rects(rects) if rects.is_empty())
        && pane_damage_empty
}

/// True when the tab strip must raster on this frame.
pub(crate) fn should_paint_tab_strip(
    shown: bool,
    top_chrome_px: usize,
    full: bool,
    strip_changed: bool,
) -> bool {
    shown && top_chrome_px > 0 && (full || strip_changed)
}

/// True when the strip contributes boxes and markers.
pub(crate) fn tab_strip_visible_for_damage(shown: bool, top_chrome_px: usize) -> bool {
    shown && top_chrome_px > 0
}

/// Inner strip stride after an optional reserved end cell.
pub(crate) fn tab_strip_inner_stride(
    window_width: usize,
    cell_w: usize,
    reserve_end: bool,
) -> usize {
    if reserve_end {
        window_width.saturating_sub(cell_w.max(1))
    } else {
        window_width
    }
}

/// True when a chrome marker key belongs to the tab strip.
pub(crate) fn strip_marker_key(key: u64) -> bool {
    key & (STRIP_TAB_MARKER_PREFIX | STRIP_HANDLE_MARKER_PREFIX) != 0
}

fn strip_markers_of(snapshot: &ChromeSnapshot) -> Vec<(u64, u64)> {
    snapshot
        .markers
        .iter()
        .copied()
        .filter(|(key, _)| strip_marker_key(*key))
        .collect()
}

/// True when pulse step or strip markers changed.
pub(crate) fn strip_chrome_changed(prior: &ChromeSnapshot, current: &ChromeSnapshot) -> bool {
    prior.pulse_step != current.pulse_step || strip_markers_of(prior) != strip_markers_of(current)
}

/// Tab-active marker for one strip slot.
pub(crate) fn strip_tab_marker(tab_index: usize, active: bool) -> (u64, u64) {
    (
        STRIP_TAB_MARKER_PREFIX | tab_index as u64,
        u64::from(active),
    )
}

/// Handle-active marker for one strip handle.
pub(crate) fn strip_handle_marker(tab_index: usize, handle: usize, active: bool) -> (u64, u64) {
    (
        STRIP_HANDLE_MARKER_PREFIX | ((tab_index as u64) << 32) | handle as u64,
        u64::from(active),
    )
}

/// Append strip markers for one tab and its handles.
pub(crate) fn append_tab_strip_markers(
    tab_index: usize,
    active: bool,
    handle_active: &[bool],
    markers: &mut Vec<(u64, u64)>,
) {
    markers.push(strip_tab_marker(tab_index, active));
    for (handle, handle_live) in handle_active.iter().copied().enumerate() {
        markers.push(strip_handle_marker(tab_index, handle, handle_live));
    }
}

/// Handle y and height for a tall title row versus a compact bar.
pub(crate) fn tab_strip_handle_metrics(bar_h: usize, title_h: usize) -> (usize, usize) {
    if bar_h > title_h {
        (title_h.saturating_add(2), title_h.saturating_sub(4).max(1))
    } else {
        (2, bar_h.saturating_sub(4).max(1))
    }
}

/// Left edge of the first handle inside a tab slot.
pub(crate) fn tab_strip_handle_origin(x0: usize, inner_pad: usize) -> usize {
    x0.saturating_add(inner_pad)
}

/// True when a handle width still sits inside the tab slot.
pub(crate) fn tab_strip_handle_fits(
    hx: usize,
    handle_w: usize,
    x0: usize,
    slot_width: usize,
) -> bool {
    hx.saturating_add(handle_w) <= x0.saturating_add(slot_width)
}

/// Pixel box for one strip handle.
pub(crate) fn tab_strip_handle_box(
    hx: usize,
    handle_y: usize,
    handle_w: usize,
    handle_h: usize,
    bar_h: usize,
) -> PixelRect {
    PixelRect::new(
        hx,
        handle_y,
        handle_w.saturating_sub(1),
        handle_h.min(bar_h.saturating_sub(handle_y)),
    )
}

/// Inputs for one tab's handle-chip boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabStripHandlePlan {
    pub(crate) x0: usize,
    pub(crate) slot_width: usize,
    pub(crate) inner_pad: usize,
    pub(crate) bar_h: usize,
    pub(crate) title_h: usize,
    pub(crate) handle_w: usize,
    pub(crate) handles: usize,
}

/// Push every handle that fits inside the tab slot.
pub(crate) fn push_tab_strip_handle_boxes(plan: TabStripHandlePlan, boxes: &mut Vec<PixelRect>) {
    let (handle_y, handle_h) = tab_strip_handle_metrics(plan.bar_h, plan.title_h);
    let handle_origin = tab_strip_handle_origin(plan.x0, plan.inner_pad);
    for handle in 0..plan.handles {
        let hx = handle_origin.saturating_add(handle.saturating_mul(plan.handle_w));
        if !tab_strip_handle_fits(hx, plan.handle_w, plan.x0, plan.slot_width) {
            break;
        }
        boxes.push(tab_strip_handle_box(
            hx,
            handle_y,
            plan.handle_w,
            handle_h,
            plan.bar_h,
        ));
    }
}

/// Badge cluster box to the left of the close target.
pub(crate) fn tab_strip_badge_box(
    x0: usize,
    slot_width: usize,
    close_left: Option<usize>,
    badge_span: usize,
    badge_y: usize,
    bar_h: usize,
    badge_size: usize,
) -> PixelRect {
    let badge_x = close_left
        .unwrap_or(x0.saturating_add(slot_width))
        .saturating_sub(2);
    PixelRect::new(
        badge_x.saturating_sub(badge_span),
        badge_y,
        badge_span.min(badge_x),
        badge_size.min(bar_h.saturating_sub(badge_y)),
    )
}

/// Mail chip box: slot on multi-pane, content on a single pane.
pub(crate) fn mail_chrome_box(slot: PixelRect, content: PixelRect, multi_pane: bool) -> PixelRect {
    let (x, y, width, height) = if multi_pane {
        (slot.x, slot.y, slot.width, slot.height)
    } else {
        (content.x, content.y, content.width, content.height)
    };
    PixelRect::new(x, y, MAIL_BADGE_PX.min(width), MAIL_BADGE_PX.min(height))
}

/// Unseen-output badge at the slot's top-right.
pub(crate) fn unseen_chrome_box(slot: PixelRect) -> PixelRect {
    let badge_width = UNSEEN_BADGE_W.min(slot.width);
    PixelRect::new(
        slot.x
            .saturating_add(slot.width.saturating_sub(badge_width)),
        slot.y,
        badge_width,
        UNSEEN_BADGE_H.min(slot.height),
    )
}

/// Active-dot box at the slot's top-right.
pub(crate) fn active_dot_box(slot: PixelRect) -> PixelRect {
    let dot_width = ACTIVE_DOT_W.min(slot.width);
    PixelRect::new(
        slot.x.saturating_add(slot.width.saturating_sub(dot_width)),
        slot.y,
        dot_width,
        ACTIVE_DOT_H.min(slot.height),
    )
}

/// Bounded mail / unseen / active bits in the pane marker word.
pub(crate) fn pane_chrome_bits(mail: bool, unseen: bool, active: bool) -> u64 {
    u64::from(mail) | (u64::from(unseen) << 1) | (u64::from(active) << 2)
}

/// Scrollback length and thumb packed into the high marker bits.
pub(crate) fn scrollbar_marker(max_scroll: usize, scroll: usize) -> u64 {
    (max_scroll.min(u32::MAX as usize) as u64) << 32 | scroll.min(u32::MAX as usize) as u64
}

/// Combine pane bits and scrollbar state into one marker word.
pub(crate) fn pane_marker_word(id: u64, bits: u64, scrollbar: u64) -> (u64, u64) {
    (id, bits | (scrollbar << 8))
}

/// True when the scrollbar track belongs in the chrome boxes.
pub(crate) fn should_record_scrollbar_box(max_scroll: usize) -> bool {
    max_scroll > 0
}

/// Push known pane chrome boxes for mail, unseen, and active chips.
pub(crate) fn push_pane_chrome_boxes(
    slot: PixelRect,
    content: PixelRect,
    multi_pane: bool,
    mail: bool,
    unseen: bool,
    active: bool,
    boxes: &mut Vec<PixelRect>,
) {
    if mail {
        boxes.push(mail_chrome_box(slot, content, multi_pane));
    }
    if unseen {
        boxes.push(unseen_chrome_box(slot));
    }
    if active {
        boxes.push(active_dot_box(slot));
    }
}

/// True when prior content differs from the current pane box.
pub(crate) fn assignment_changed_content(
    prior_content: Option<PixelRect>,
    current: PixelRect,
) -> bool {
    prior_content.is_some_and(|old| old != current)
}

/// True when a focus move must dirty this pane's cursor rows.
pub(crate) fn focus_affects_pane(
    focus_changed: bool,
    prior_focus: Option<u64>,
    pane_id: u64,
    focused: u64,
) -> bool {
    focus_changed && (prior_focus == Some(pane_id) || pane_id == focused)
}

fn insert_snapshot_row(rows: &mut Vec<usize>, row: usize, screen_rows: usize) {
    if row < screen_rows {
        rows.push(row);
        rows.sort_unstable();
        rows.dedup();
    }
}

/// Dirty rows for one pane snapshot, including assignment and focus cursors.
pub(crate) fn snapshot_dirty_rows(
    assignment_changed: bool,
    screen_rows: usize,
    existing: &[usize],
    include_focus_cursors: bool,
    cursor_row: usize,
    last_painted_cursor: Option<usize>,
) -> Vec<usize> {
    let mut dirty_rows = if assignment_changed {
        (0..screen_rows).collect()
    } else {
        existing.to_vec()
    };
    if include_focus_cursors {
        insert_snapshot_row(&mut dirty_rows, cursor_row, screen_rows);
        if let Some(row) = last_painted_cursor {
            insert_snapshot_row(&mut dirty_rows, row, screen_rows);
        }
    }
    dirty_rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_slot_damage_requires_surface_repaint() {
        let slot = PixelRect::new(10, 20, 30, 40);
        assert!(frame_damage_covers(&FrameDamage::Full, slot));
        assert!(!frame_damage_covers(&FrameDamage::rects(), slot));
        for (rect, expected) in [
            (slot, true),
            (PixelRect::new(0, 0, 50, 70), true),
            (PixelRect::new(11, 20, 30, 40), false),
            (PixelRect::new(10, 21, 30, 40), false),
            (PixelRect::new(10, 20, 29, 40), false),
            (PixelRect::new(10, 20, 30, 39), false),
        ] {
            assert_eq!(
                frame_damage_covers(&FrameDamage::Rects(vec![rect]), slot),
                expected
            );
        }
    }

    #[test]
    fn clipping_rejects_each_empty_axis_and_preserves_nonzero_origin() {
        let cases = [
            ("right edge", PixelRect::new(5, 1, 2, 2), 5, 4, None),
            ("bottom edge", PixelRect::new(1, 4, 2, 2), 5, 4, None),
            (
                "nonzero origin",
                PixelRect::new(2, 1, 2, 3),
                5,
                5,
                Some(PixelRect::new(2, 1, 2, 3)),
            ),
            (
                "saturating bounds",
                PixelRect::new(3, 2, usize::MAX, usize::MAX),
                5,
                4,
                Some(PixelRect::new(3, 2, 2, 2)),
            ),
        ];

        for (name, rect, width, height, expected) in cases {
            assert_eq!(rect.clipped(width, height), expected, "{name}");
        }
    }

    #[test]
    fn push_rect_rejects_each_zero_dimension() {
        for rect in [PixelRect::new(2, 3, 0, 4), PixelRect::new(2, 3, 4, 0)] {
            let mut damage = FrameDamage::rects();
            damage.push_rect(rect);
            assert_eq!(damage, FrameDamage::Rects(Vec::new()), "{rect:?}");
        }
    }

    #[test]
    fn adjacent_row_bands_merge_but_disjoint_bands_do_not() {
        let mut damage = FrameDamage::rects();
        damage.push_rect(PixelRect::new(4, 10, 20, 5));
        damage.push_rect(PixelRect::new(4, 15, 20, 5));
        damage.push_rect(PixelRect::new(4, 25, 20, 5));
        assert_eq!(
            damage,
            FrameDamage::Rects(vec![
                PixelRect::new(4, 10, 20, 10),
                PixelRect::new(4, 25, 20, 5),
            ])
        );
    }

    fn layout() -> LayoutSnapshot {
        LayoutSnapshot {
            panes: vec![PaneLayoutSnapshot {
                id: 7,
                slot: PixelRect::new(0, 0, 100, 100),
                content: PixelRect::new(4, 4, 92, 92),
            }],
        }
    }

    #[test]
    fn steady_four_pane_rows_are_partial_and_layout_changes_are_full() {
        let current = LayoutSnapshot {
            panes: (0..4)
                .map(|id| PaneLayoutSnapshot {
                    id,
                    slot: PixelRect::new(id as usize * 100, 0, 96, 96),
                    content: PixelRect::new(id as usize * 100 + 4, 4, 88, 88),
                })
                .collect(),
        };
        let damage = compose_frame_damage(
            Some(&current),
            &current,
            Some(&ChromeSnapshot::default()),
            &ChromeSnapshot::default(),
            &[PaneDamageSnapshot {
                pane_id: 2,
                content: PixelRect::new(204, 4, 88, 88),
                row_height: 8,
                dirty_rows: vec![3],
                blit: None,
            }],
            false,
        );
        assert!(
            matches!(damage, FrameDamage::Rects(ref rects) if rects == &vec![PixelRect::new(204, 28, 88, 8)])
        );
        assert!(matches!(
            compose_frame_damage(
                Some(&current),
                &layout(),
                None,
                &ChromeSnapshot::default(),
                &[],
                false
            ),
            FrameDamage::Full
        ));
    }

    #[test]
    fn focus_damage_covers_old_and_new_slots() {
        let current = LayoutSnapshot {
            panes: vec![
                PaneLayoutSnapshot {
                    id: 7,
                    slot: PixelRect::new(0, 0, 100, 100),
                    content: PixelRect::new(4, 4, 92, 92),
                },
                PaneLayoutSnapshot {
                    id: 8,
                    slot: PixelRect::new(100, 0, 100, 100),
                    content: PixelRect::new(104, 4, 92, 92),
                },
            ],
        };
        let prior_chrome = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            boxes: Vec::new(),
            markers: Vec::new(),
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            focused_pane: Some(8),
            focused_slot: Some(PixelRect::new(100, 0, 100, 100)),
            boxes: Vec::new(),
            markers: Vec::new(),
            ..ChromeSnapshot::default()
        };
        let damage = compose_frame_damage(
            Some(&current),
            &current,
            Some(&prior_chrome),
            &current_chrome,
            &[],
            false,
        );
        assert!(damage
            .rects_slice()
            .contains(&PixelRect::new(0, 0, 100, 100)));
        assert!(damage
            .rects_slice()
            .contains(&PixelRect::new(100, 0, 100, 100)));
    }

    #[test]
    fn chrome_geometry_changed_uses_boxes_and_markers_table() {
        let unchanged = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            boxes: vec![PixelRect::new(2, 2, 10, 10)],
            markers: vec![(7, 1)],
            ..ChromeSnapshot::default()
        };
        let cases = [
            (None, false),
            (Some(unchanged.clone()), false),
            (
                Some(ChromeSnapshot {
                    boxes: vec![PixelRect::new(3, 2, 10, 10)],
                    ..unchanged.clone()
                }),
                true,
            ),
            (
                Some(ChromeSnapshot {
                    markers: vec![(7, 2)],
                    ..unchanged.clone()
                }),
                true,
            ),
            (
                Some(ChromeSnapshot {
                    pulse_step: Some(3),
                    ..unchanged.clone()
                }),
                true,
            ),
            (
                Some(ChromeSnapshot {
                    light_cycle_step: Some(2),
                    ..unchanged.clone()
                }),
                true,
            ),
            (
                Some(ChromeSnapshot {
                    unbounded_state: "other".to_owned(),
                    ..unchanged.clone()
                }),
                true,
            ),
        ];
        for (prior, expected) in cases {
            assert_eq!(
                chrome_geometry_changed(prior.as_ref(), &unchanged),
                expected
            );
        }
    }

    #[test]
    fn pane_assignment_change_is_a_layout_transition() {
        let prior = LayoutSnapshot {
            panes: vec![
                PaneLayoutSnapshot {
                    id: 1,
                    slot: PixelRect::new(0, 0, 100, 100),
                    content: PixelRect::new(4, 4, 92, 92),
                },
                PaneLayoutSnapshot {
                    id: 2,
                    slot: PixelRect::new(100, 0, 100, 100),
                    content: PixelRect::new(104, 4, 92, 92),
                },
            ],
        };
        let current = LayoutSnapshot {
            panes: vec![
                PaneLayoutSnapshot {
                    id: 2,
                    slot: PixelRect::new(0, 0, 100, 100),
                    content: PixelRect::new(4, 4, 92, 92),
                },
                PaneLayoutSnapshot {
                    id: 1,
                    slot: PixelRect::new(100, 0, 100, 100),
                    content: PixelRect::new(104, 4, 92, 92),
                },
            ],
        };
        assert!(layout_transition(Some(&prior), &current));
        assert_eq!(
            compose_frame_damage(
                Some(&prior),
                &current,
                Some(&ChromeSnapshot::default()),
                &ChromeSnapshot::default(),
                &[],
                false,
            ),
            FrameDamage::Full
        );
    }

    #[test]
    fn full_frame_chrome_mapper_exceeds_budget_and_returns_full() {
        let current = layout();
        let full_frame = PixelRect::new(0, 0, 1600, 900);
        assert!(1600usize.saturating_mul(900) > CHROME_DAMAGE_BUDGET_PX);
        let prior_chrome = ChromeSnapshot {
            boxes: vec![full_frame],
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            boxes: vec![full_frame],
            markers: vec![(7, 1)],
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Full
        );
    }

    #[test]
    fn over_budget_known_chrome_falls_back_to_full() {
        let current = layout();
        let prior_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(0, 0, 256, 256)],
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(0, 0, 256, 256)],
            markers: vec![(7, 1)],
            ..ChromeSnapshot::default()
        };
        assert!(matches!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Full
        ));
    }

    #[test]
    fn bounded_chrome_change_stays_partial() {
        let current = layout();
        let prior_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(0, 0, 16, 16)],
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(0, 0, 16, 16)],
            markers: vec![(7, 1)],
            ..ChromeSnapshot::default()
        };
        let damage = compose_frame_damage(
            Some(&current),
            &current,
            Some(&prior_chrome),
            &current_chrome,
            &[],
            false,
        );
        assert_eq!(
            damage,
            FrameDamage::Rects(vec![PixelRect::new(0, 0, 16, 16)])
        );
    }

    #[test]
    fn pulse_step_repaints_known_dot_box() {
        let current = layout();
        let prior_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(88, 0, 12, 16)],
            pulse_step: Some(1),
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            boxes: vec![PixelRect::new(88, 0, 12, 16)],
            pulse_step: Some(2),
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Rects(vec![PixelRect::new(88, 0, 12, 16)])
        );
    }

    #[test]
    fn two_tab_strip_pulse_stays_on_exact_dot_boxes() {
        let current = layout();
        let first_dot = PixelRect::new(120, 4, 6, 6);
        let second_dot = PixelRect::new(620, 4, 6, 6);
        let prior_chrome = ChromeSnapshot {
            boxes: vec![first_dot, second_dot],
            pulse_step: Some(3),
            unbounded_state: "two-tabs".to_owned(),
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            boxes: vec![first_dot, second_dot],
            pulse_step: Some(4),
            unbounded_state: "two-tabs".to_owned(),
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Rects(vec![first_dot, second_dot])
        );
    }

    #[test]
    fn live_light_cycle_repaints_edge_strips_each_step_and_settles() {
        let current = layout();
        let prior_chrome = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            light_cycle_step: Some(1),
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            light_cycle_step: Some(2),
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Rects(border_strips(PixelRect::new(0, 0, 100, 100)).to_vec())
        );
        let settled = ChromeSnapshot {
            light_cycle_step: None,
            ..current_chrome.clone()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&current_chrome),
                &settled,
                &[],
                false,
            ),
            FrameDamage::Rects(border_strips(PixelRect::new(0, 0, 100, 100)).to_vec())
        );
    }

    #[test]
    fn unbounded_chrome_change_promotes_to_full() {
        let current = layout();
        let prior_chrome = ChromeSnapshot {
            unbounded_state: "prior".to_owned(),
            ..ChromeSnapshot::default()
        };
        let current_chrome = ChromeSnapshot {
            unbounded_state: "current".to_owned(),
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(
                Some(&current),
                &current,
                Some(&prior_chrome),
                &current_chrome,
                &[],
                false,
            ),
            FrameDamage::Full
        );
    }

    #[test]
    fn layout_transition_none_and_identical_table() {
        let current = layout();
        let cases = [
            ("no prior", None, true),
            ("identical", Some(current.clone()), false),
        ];
        for (name, prior, expected) in cases {
            assert_eq!(
                layout_transition(prior.as_ref(), &current),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn compose_full_flag_missing_id_and_blit_table() {
        let current = layout();
        let chrome = ChromeSnapshot::default();
        let cases = [
            (
                "full flag",
                true,
                &[PaneDamageSnapshot {
                    pane_id: 7,
                    content: PixelRect::new(4, 4, 92, 92),
                    row_height: 8,
                    dirty_rows: vec![1],
                    blit: None,
                }][..],
                FrameDamage::Full,
            ),
            (
                "missing pane id",
                false,
                &[PaneDamageSnapshot {
                    pane_id: 99,
                    content: PixelRect::new(4, 4, 92, 92),
                    row_height: 8,
                    dirty_rows: vec![1],
                    blit: None,
                }][..],
                FrameDamage::Full,
            ),
            (
                "blit only",
                false,
                &[PaneDamageSnapshot {
                    pane_id: 7,
                    content: PixelRect::new(4, 4, 92, 92),
                    row_height: 8,
                    dirty_rows: Vec::new(),
                    blit: Some(PixelRect::new(4, 12, 92, 24)),
                }][..],
                FrameDamage::Rects(vec![PixelRect::new(4, 12, 92, 24)]),
            ),
        ];
        for (name, full, panes, expected) in cases {
            assert_eq!(
                compose_frame_damage(
                    Some(&current),
                    &current,
                    Some(&chrome),
                    &chrome,
                    panes,
                    full,
                ),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn push_rect_full_is_noop_and_duplicates_are_ignored() {
        let mut full = FrameDamage::Full;
        full.push_rect(PixelRect::new(1, 1, 2, 2));
        assert_eq!(full, FrameDamage::Full);

        let mut damage = FrameDamage::rects();
        let rect = PixelRect::new(3, 4, 5, 6);
        damage.push_rect(rect);
        damage.push_rect(rect);
        assert_eq!(damage, FrameDamage::Rects(vec![rect]));
    }

    #[test]
    fn live_light_cycle_same_step_still_repaints_edge_strips() {
        let current = layout();
        let chrome = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            light_cycle_step: Some(1),
            ..ChromeSnapshot::default()
        };
        assert_eq!(
            compose_frame_damage(Some(&current), &current, Some(&chrome), &chrome, &[], false,),
            FrameDamage::Rects(border_strips(PixelRect::new(0, 0, 100, 100)).to_vec())
        );
        let idle = ChromeSnapshot {
            light_cycle_step: None,
            ..chrome.clone()
        };
        assert_eq!(
            compose_frame_damage(Some(&current), &current, Some(&idle), &idle, &[], false),
            FrameDamage::rects()
        );
    }

    #[test]
    fn sweep_does_not_invalidate_unrelated_chrome_boxes() {
        let layout = layout();
        let prior = ChromeSnapshot {
            focused_pane: Some(7),
            focused_slot: Some(PixelRect::new(0, 0, 100, 100)),
            boxes: vec![PixelRect::new(110, 10, 9, 9)],
            light_cycle_step: Some(1),
            ..Default::default()
        };
        let current = ChromeSnapshot {
            light_cycle_step: Some(2),
            ..prior.clone()
        };
        let damage =
            compose_frame_damage(Some(&layout), &layout, Some(&prior), &current, &[], false);
        assert!(!frame_damage_intersects(&damage, prior.boxes[0]));
        assert!(!frame_damage_intersects(
            &damage,
            PixelRect::new(7, 7, 86, 86)
        ));
    }

    #[test]
    fn border_strips_are_disjoint_and_cover_only_edges_including_tiny_slots() {
        for width in [0, 1, 7, 13, 14, 15, 100] {
            for height in [0, 1, 7, 13, 14, 15, 100] {
                let slot = PixelRect::new(5, 8, width, height);
                let strips = border_strips(slot);
                for y in 0..height {
                    for x in 0..width {
                        let covered = strips
                            .iter()
                            .filter(|r| {
                                x + 5 >= r.x
                                    && x + 5 < r.x + r.width
                                    && y + 8 >= r.y
                                    && y + 8 < r.y + r.height
                            })
                            .count();
                        let edge = x < 7 || y < 7 || width - x <= 7 || height - y <= 7;
                        assert_eq!(covered, usize::from(edge));
                    }
                }
            }
        }
    }

    #[test]
    fn dirty_row_clips_to_content_bottom() {
        let current = LayoutSnapshot {
            panes: vec![PaneLayoutSnapshot {
                id: 7,
                slot: PixelRect::new(0, 0, 100, 20),
                content: PixelRect::new(4, 4, 92, 10),
            }],
        };
        let damage = compose_frame_damage(
            Some(&current),
            &current,
            Some(&ChromeSnapshot::default()),
            &ChromeSnapshot::default(),
            &[PaneDamageSnapshot {
                pane_id: 7,
                content: PixelRect::new(4, 4, 92, 10),
                row_height: 8,
                dirty_rows: vec![1],
                blit: None,
            }],
            false,
        );
        assert_eq!(
            damage,
            FrameDamage::Rects(vec![PixelRect::new(4, 12, 92, 2)])
        );
    }

    #[test]
    fn pulse_phase_and_visibility_table() {
        let cases = [(0u8, 0.0f32), (8, 0.5), (16, 0.0), (24, 0.5)];
        for (step, expected) in cases {
            assert_eq!(pulse_phase_for_step(step), expected, "step {step}");
        }
        assert!(pulse_live(true, false));
        assert!(!pulse_live(true, true));
        assert!(!pulse_live(false, false));
        assert_eq!(pulse_step_for_snapshot(1, true, 7), Some(7));
        assert_eq!(pulse_step_for_snapshot(0, true, 7), None);
        assert_eq!(pulse_step_for_snapshot(1, false, 7), None);
        assert_eq!(light_cycle_step_for_snapshot(true, 3), Some(3));
        assert_eq!(light_cycle_step_for_snapshot(false, 3), None);
        assert_eq!(pulse_phase_if(true, 8), Some(0.5));
        assert_eq!(pulse_phase_if(false, 8), None);
    }

    #[test]
    fn frame_damage_intersects_and_paint_required_table() {
        let slot = PixelRect::new(10, 20, 30, 40);
        let overlap = PixelRect::new(20, 30, 10, 10);
        let adjacent = PixelRect::new(40, 20, 5, 40);
        let above = PixelRect::new(10, 0, 30, 20);
        let empty = FrameDamage::rects();
        let partial = FrameDamage::Rects(vec![overlap]);
        let cases = [
            ("full", FrameDamage::Full, slot, true),
            ("empty", empty.clone(), slot, false),
            ("overlap", partial.clone(), slot, true),
            ("adjacent right", partial.clone(), adjacent, false),
            ("adjacent top", FrameDamage::Rects(vec![above]), slot, false),
            (
                "zero size",
                partial.clone(),
                PixelRect::new(20, 30, 0, 10),
                false,
            ),
        ];
        for (name, damage, rect, expected) in cases {
            assert_eq!(frame_damage_intersects(&damage, rect), expected, "{name}");
        }
        assert!(pane_paint_required(false, &empty, slot));
        assert!(!pane_paint_required(true, &empty, slot));
        assert!(pane_paint_required(true, &partial, slot));
    }

    #[test]
    fn raster_skip_and_strip_paint_predicates_table() {
        let empty = FrameDamage::rects();
        let full = FrameDamage::Full;
        let rects = FrameDamage::Rects(vec![PixelRect::new(1, 1, 2, 2)]);
        assert!(!composer_promotes_to_full(true, &full));
        assert!(composer_promotes_to_full(false, &full));
        assert!(!composer_promotes_to_full(false, &rects));
        assert!(empty_partial_skips_paint(false, &empty, true));
        assert!(!empty_partial_skips_paint(true, &empty, true));
        assert!(!empty_partial_skips_paint(false, &rects, true));
        assert!(!empty_partial_skips_paint(false, &empty, false));
        let strip_cases = [
            (true, 16usize, false, true, true),
            (true, 16, true, false, true),
            (true, 0, true, true, false),
            (false, 16, true, true, false),
            (true, 16, false, false, false),
        ];
        for (shown, top, full, changed, expected) in strip_cases {
            assert_eq!(
                should_paint_tab_strip(shown, top, full, changed),
                expected,
                "shown={shown} top={top} full={full} changed={changed}"
            );
        }
        assert!(tab_strip_visible_for_damage(true, 8));
        assert!(!tab_strip_visible_for_damage(true, 0));
        assert!(!tab_strip_visible_for_damage(false, 8));
        assert_eq!(tab_strip_inner_stride(100, 8, true), 92);
        assert_eq!(tab_strip_inner_stride(100, 0, true), 99);
        assert_eq!(tab_strip_inner_stride(100, 8, false), 100);
    }

    #[test]
    fn strip_markers_and_chrome_changed_table() {
        assert!(strip_marker_key(STRIP_TAB_MARKER_PREFIX | 3));
        assert!(strip_marker_key(STRIP_HANDLE_MARKER_PREFIX | 1));
        assert!(!strip_marker_key(7));
        assert_eq!(strip_tab_marker(2, true), (STRIP_TAB_MARKER_PREFIX | 2, 1));
        assert_eq!(strip_tab_marker(2, false), (STRIP_TAB_MARKER_PREFIX | 2, 0));
        assert_eq!(
            strip_handle_marker(1, 3, true),
            (STRIP_HANDLE_MARKER_PREFIX | (1u64 << 32) | 3, 1)
        );
        let mut markers = Vec::new();
        append_tab_strip_markers(0, true, &[false, true], &mut markers);
        assert_eq!(
            markers,
            vec![
                strip_tab_marker(0, true),
                strip_handle_marker(0, 0, false),
                strip_handle_marker(0, 1, true),
            ]
        );
        let prior = ChromeSnapshot {
            pulse_step: Some(1),
            markers: vec![(7, 4), strip_tab_marker(0, true)],
            ..ChromeSnapshot::default()
        };
        let same_strip = ChromeSnapshot {
            pulse_step: Some(1),
            markers: vec![(8, 1), strip_tab_marker(0, true)],
            ..ChromeSnapshot::default()
        };
        let pulse = ChromeSnapshot {
            pulse_step: Some(2),
            markers: prior.markers.clone(),
            ..ChromeSnapshot::default()
        };
        let strip = ChromeSnapshot {
            pulse_step: Some(1),
            markers: vec![(7, 4), strip_tab_marker(0, false)],
            ..ChromeSnapshot::default()
        };
        assert!(!strip_chrome_changed(&prior, &same_strip));
        assert!(strip_chrome_changed(&prior, &pulse));
        assert!(strip_chrome_changed(&prior, &strip));
    }

    #[test]
    fn tab_strip_handle_and_badge_tables() {
        assert_eq!(tab_strip_handle_metrics(20, 10), (12, 6));
        assert_eq!(tab_strip_handle_metrics(10, 10), (2, 6));
        assert_eq!(tab_strip_handle_metrics(3, 10), (2, 1));
        assert_eq!(tab_strip_handle_origin(40, 8), 48);
        assert!(tab_strip_handle_fits(40, 12, 40, 12));
        assert!(!tab_strip_handle_fits(41, 12, 40, 12));
        assert_eq!(
            tab_strip_handle_box(48, 12, 12, 6, 20),
            PixelRect::new(48, 12, 11, 6)
        );
        assert_eq!(
            tab_strip_handle_box(48, 18, 12, 8, 20),
            PixelRect::new(48, 18, 11, 2)
        );
        let mut boxes = Vec::new();
        push_tab_strip_handle_boxes(
            TabStripHandlePlan {
                x0: 40,
                slot_width: 30,
                inner_pad: 0,
                bar_h: 20,
                title_h: 10,
                handle_w: 12,
                handles: 3,
            },
            &mut boxes,
        );
        assert_eq!(
            boxes,
            vec![PixelRect::new(40, 12, 11, 6), PixelRect::new(52, 12, 11, 6)]
        );
        assert_eq!(
            tab_strip_badge_box(40, 80, Some(100), 22, 4, 20, 6),
            PixelRect::new(76, 4, 22, 6)
        );
        assert_eq!(
            tab_strip_badge_box(40, 80, None, 22, 4, 20, 6),
            PixelRect::new(96, 4, 22, 6)
        );
        assert_eq!(
            tab_strip_badge_box(0, 8, Some(4), 22, 4, 8, 6),
            PixelRect::new(0, 4, 2, 4)
        );
    }

    #[test]
    fn pane_chrome_boxes_and_markers_table() {
        let slot = PixelRect::new(10, 20, 40, 30);
        let content = PixelRect::new(12, 22, 30, 20);
        assert_eq!(
            mail_chrome_box(slot, content, true),
            PixelRect::new(10, 20, 20, 20)
        );
        assert_eq!(
            mail_chrome_box(slot, content, false),
            PixelRect::new(12, 22, 20, 20)
        );
        let tiny = PixelRect::new(0, 0, 8, 8);
        assert_eq!(
            mail_chrome_box(tiny, tiny, true),
            PixelRect::new(0, 0, 8, 8)
        );
        assert_eq!(unseen_chrome_box(slot), PixelRect::new(30, 20, 20, 16));
        assert_eq!(unseen_chrome_box(tiny), PixelRect::new(0, 0, 8, 8));
        assert_eq!(active_dot_box(slot), PixelRect::new(38, 20, 12, 16));
        assert_eq!(active_dot_box(tiny), PixelRect::new(0, 0, 8, 8));
        assert_eq!(pane_chrome_bits(false, false, false), 0);
        assert_eq!(pane_chrome_bits(true, false, false), 1);
        assert_eq!(pane_chrome_bits(false, true, false), 2);
        assert_eq!(pane_chrome_bits(false, false, true), 4);
        assert_eq!(pane_chrome_bits(true, true, true), 7);
        assert_eq!(scrollbar_marker(0, 0), 0);
        assert_eq!(scrollbar_marker(3, 1), (3u64 << 32) | 1);
        assert_eq!(
            scrollbar_marker(u32::MAX as usize + 8, u32::MAX as usize + 2),
            (u32::MAX as u64) << 32 | u32::MAX as u64
        );
        assert_eq!(pane_marker_word(9, 5, 3), (9, 5 | (3 << 8)));
        assert!(should_record_scrollbar_box(1));
        assert!(!should_record_scrollbar_box(0));
        let mut boxes = Vec::new();
        push_pane_chrome_boxes(slot, content, true, true, true, true, &mut boxes);
        assert_eq!(
            boxes,
            vec![
                mail_chrome_box(slot, content, true),
                unseen_chrome_box(slot),
                active_dot_box(slot),
            ]
        );
        boxes.clear();
        push_pane_chrome_boxes(slot, content, true, false, false, false, &mut boxes);
        assert!(boxes.is_empty());
    }

    #[test]
    fn snapshot_dirty_rows_and_focus_table() {
        let content = PixelRect::new(4, 4, 92, 92);
        assert!(!assignment_changed_content(None, content));
        assert!(!assignment_changed_content(Some(content), content));
        assert!(assignment_changed_content(
            Some(PixelRect::new(0, 0, 10, 10)),
            content
        ));
        assert!(!focus_affects_pane(false, Some(1), 1, 2));
        assert!(focus_affects_pane(true, Some(1), 1, 2));
        assert!(focus_affects_pane(true, Some(1), 2, 2));
        assert!(!focus_affects_pane(true, Some(1), 3, 2));
        assert_eq!(
            snapshot_dirty_rows(true, 3, &[9], false, 1, Some(2)),
            vec![0, 1, 2]
        );
        assert_eq!(
            snapshot_dirty_rows(false, 4, &[1], true, 2, Some(0)),
            vec![0, 1, 2]
        );
        assert_eq!(
            snapshot_dirty_rows(false, 2, &[1], true, 9, Some(8)),
            vec![1]
        );
        assert_eq!(
            snapshot_dirty_rows(false, 2, &[], false, 0, Some(1)),
            Vec::<usize>::new()
        );
    }
}
