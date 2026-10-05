//! Graphite transparency settings (#112).
//!
//! A 760×460 dialog with a scrolling list and a live preview. It writes the
//! existing opacity keys and is opened only when `chrome_style = "graphite"`.
//! Text, the cursor, status dots, badges, the active tab chip, the command
//! field, and the focus ring stay opaque in the preview. The ground, pane
//! surfaces, and bar backgrounds take the opacities being edited.

use crate::config::{self, ConfigFile};
use crate::graphite::{self, Face, Rect};
use crate::mux::ChromeGeom;
use crate::raster::{alpha_of, opacity_to_alpha, pack_argb, unpack_rgb};
use crate::theme::ThemeVariant;

/// Design size of the dialog. It does not grow or shrink with its contents.
/// A shorter window clamps the frame; the list scrolls either way.
pub(crate) const DIALOG_W: f32 = 760.0;
pub(crate) const DIALOG_H: f32 = 460.0;

const HEADER_H: f32 = 44.0;
const FOOTER_H: f32 = 36.0;
const PREVIEW_W: f32 = 248.0;
const PAD: f32 = 16.0;
const GAP: f32 = 16.0;
const RADIUS: f32 = 10.0;
const ROW_H: f32 = 48.0;
const SCROLLBAR_W: f32 = 8.0;
const LIMITS_H: f32 = 132.0;

const LIMIT_LINES: &[&str] = &[
    "Platform limits",
    "X11 without a compositor and --gpu stay opaque.",
    "Lowering window opacity from 1.0 on X11 or Wayland needs a restart.",
    "Text, the cursor, status dots, badges, and the focus ring stay opaque.",
];

/// Whether this window can show a translucent ground right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionLimits {
    pub alpha: bool,
    pub gpu: bool,
}

/// The dialog opens only for Graphite chrome.
#[must_use]
pub(crate) fn opens_for(style: config::ChromeStyle) -> bool {
    style == config::ChromeStyle::Graphite
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowId {
    WindowOpacity,
    ChromeOpacity,
    WindowBlur,
    PaneActive,
    PaneInactive,
    BackgroundImage,
    ImageOpacity,
    ImageBlur,
}

const ROWS: [RowId; 8] = [
    RowId::WindowOpacity,
    RowId::ChromeOpacity,
    RowId::WindowBlur,
    RowId::PaneActive,
    RowId::PaneInactive,
    RowId::BackgroundImage,
    RowId::ImageOpacity,
    RowId::ImageBlur,
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Values {
    pub window_opacity: f32,
    pub chrome_opacity: f32,
    pub chrome_follows: bool,
    pub window_blur: bool,
    pub pane_opacity_active: f32,
    pub pane_opacity_inactive: f32,
    pub background_image: Option<String>,
    pub background_opacity: f32,
    pub background_blur_px: u32,
}

/// A config write. `ChromeOpacity(None)` removes the key so chrome follows
/// the window. `BackgroundImage(None)` removes the image.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Write {
    WindowOpacity(f32),
    ChromeOpacity(Option<f32>),
    WindowBlur(bool),
    PaneActive(f32),
    PaneInactive(f32),
    BackgroundImage(Option<String>),
    ImageOpacity(f32),
    ImageBlur(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    Slider(RowId),
    Scroll { grab: i32 },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dialog {
    pub values: Values,
    pub selected: usize,
    pub scroll: usize,
    pub path_edit: Option<String>,
    pub limits: SessionLimits,
    pub notice: Option<String>,
    drag: Option<Drag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Close,
    Row(RowId),
    Slider(RowId),
    Toggle(RowId),
    ClearImage,
    ScrollThumb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorKind {
    Pointer,
    Grab,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Input {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Backspace,
    Char(char),
    Home,
    End,
    PageUp,
    PageDown,
    PointerDown(usize, usize),
    PointerMove(usize, usize),
    PointerUp,
    /// Positive scrolls the list downward.
    Wheel(i32),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Edit {
    pub close: bool,
    pub writes: Vec<Write>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Samples {
    pub ground: (usize, usize),
    pub bar: (usize, usize),
    pub pane: (usize, usize),
    pub text: (usize, usize),
    pub cursor: (usize, usize),
    pub dot: (usize, usize),
    pub badge: (usize, usize),
    pub ring: (usize, usize),
    pub chip: (usize, usize),
    pub field: (usize, usize),
    /// Stripe color under the ground sample, before the ground is composited.
    pub ground_under: [u8; 3],
    /// Stripe color under the bar sample, before the ground and bar.
    pub bar_under: [u8; 3],
}

#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub dialog: Rect,
    pub list: Rect,
    pub scroll: usize,
    pub max_scroll: usize,
    pub row_h: usize,
    rows: Vec<RowBox>,
    close: Rect,
    thumb: Option<Rect>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub samples: Samples,
    preview: Rect,
    content_w: usize,
    scale_milli: u32,
}

#[derive(Debug, Clone, Copy)]
struct RowBox {
    id: RowId,
    bounds: Rect,
    slider: Option<Rect>,
    toggle: Option<Rect>,
    clear: Option<Rect>,
}

impl Dialog {
    pub(crate) fn from_config(file: &ConfigFile, limits: SessionLimits) -> Self {
        let follows = file.chrome_opacity.is_none();
        let window = snap_opacity(file.window_opacity());
        Self {
            values: Values {
                window_opacity: window,
                chrome_opacity: snap_opacity(if follows {
                    window
                } else {
                    file.chrome_opacity()
                }),
                chrome_follows: follows,
                window_blur: file.window_blur(),
                pane_opacity_active: snap_opacity(file.pane_opacity_active()),
                pane_opacity_inactive: snap_opacity(file.pane_opacity_inactive()),
                background_image: file
                    .background_image
                    .as_ref()
                    .map(|path| path.display().to_string()),
                background_opacity: snap_opacity(file.background_opacity()),
                background_blur_px: file.background_blur_px().min(64),
            },
            selected: 0,
            scroll: 0,
            path_edit: None,
            limits,
            notice: None,
            drag: None,
        }
    }

    pub(crate) fn row_labels(&self) -> Vec<String> {
        ROWS.iter().map(|id| self.row_label(*id)).collect()
    }

    pub(crate) fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub(crate) fn edit(
        &mut self,
        input: Input,
        chrome: ChromeGeom,
        window_w: usize,
        window_h: usize,
    ) -> Edit {
        let before = self.values.clone();
        let mut close = false;
        let mut path_error = false;
        match input {
            Input::Escape => {
                if self.path_edit.take().is_some() {
                    self.notice = None;
                } else {
                    close = true;
                }
            }
            Input::Up => self.move_selection(-1),
            Input::Down => self.move_selection(1),
            Input::PageUp => self.move_selection(-4),
            Input::PageDown => self.move_selection(4),
            Input::Left => self.nudge(-1),
            Input::Right => self.nudge(1),
            Input::Home => self.jump(0.0),
            Input::End => self.jump(1.0),
            Input::Enter => path_error = self.activate(),
            Input::Backspace => {
                if let Some(edit) = self.path_edit.as_mut() {
                    edit.pop();
                }
            }
            Input::Char(ch) if !ch.is_control() => {
                if self.current() == Some(RowId::BackgroundImage) {
                    self.path_edit
                        .get_or_insert_with(|| {
                            self.values.background_image.clone().unwrap_or_default()
                        })
                        .push(ch);
                }
            }
            Input::Char(_) => {}
            Input::Wheel(rows) => {
                let metrics = metrics(chrome, window_w, window_h);
                let max = max_scroll(metrics.list_h, metrics.content_h);
                self.scroll = scroll_by(self.scroll, rows * metrics.row_h as i32, max);
            }
            Input::PointerDown(x, y) => {
                let layout = layout(self, chrome, window_w, window_h);
                match hit(&layout, x, y) {
                    Some(Hit::Close) => close = true,
                    Some(Hit::Toggle(id)) => {
                        self.select(id);
                        path_error = self.activate();
                    }
                    Some(Hit::ClearImage) => {
                        self.select(RowId::BackgroundImage);
                        self.path_edit = None;
                        self.values.background_image = None;
                    }
                    Some(Hit::Slider(id)) => {
                        self.select(id);
                        self.drag = Some(Drag::Slider(id));
                        if let Some(track) = row_box(&layout, id).and_then(|row| row.slider) {
                            self.set_slider(id, slider_t(track, x));
                        }
                    }
                    Some(Hit::ScrollThumb) => {
                        let thumb_y = layout.thumb.map(|thumb| thumb.y).unwrap_or(y);
                        self.drag = Some(Drag::Scroll {
                            grab: y as i32 - thumb_y as i32,
                        });
                    }
                    Some(Hit::Row(id)) => self.select(id),
                    None => {}
                }
            }
            Input::PointerMove(x, y) => match self.drag {
                Some(Drag::Slider(id)) => {
                    let layout = layout(self, chrome, window_w, window_h);
                    if let Some(track) = row_box(&layout, id).and_then(|row| row.slider) {
                        self.set_slider(id, slider_t(track, x));
                    }
                }
                Some(Drag::Scroll { grab }) => {
                    let layout = layout(self, chrome, window_w, window_h);
                    if let Some(thumb) = layout.thumb {
                        let travel = layout.list.h.saturating_sub(thumb.h).max(1);
                        let top = (y as i32 - grab).max(layout.list.y as i32) as usize;
                        let along = top.saturating_sub(layout.list.y).min(travel);
                        self.scroll = (along * layout.max_scroll / travel).min(layout.max_scroll);
                    }
                }
                None => {}
            },
            Input::PointerUp => self.drag = None,
        }
        if !path_error {
            self.refresh_notice();
        }
        // A wheel moves the list under a fixed selection. Keyboard and
        // pointer selection still bring the current row into the viewport.
        if !matches!(input, Input::Wheel(_)) {
            self.reveal(chrome, window_w, window_h);
        }
        Edit {
            close,
            writes: diff(&before, &self.values),
        }
    }

    fn current(&self) -> Option<RowId> {
        ROWS.get(self.selected).copied()
    }

    fn select(&mut self, id: RowId) {
        if let Some(index) = ROWS.iter().position(|row| *row == id) {
            self.selected = index;
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let last = ROWS.len().saturating_sub(1) as i32;
        let next = (self.selected as i32 + delta).clamp(0, last);
        self.selected = next as usize;
    }

    fn nudge(&mut self, dir: i32) {
        let Some(id) = self.current() else {
            return;
        };
        match id {
            RowId::WindowOpacity => {
                self.values.window_opacity = step_opacity(self.values.window_opacity, dir);
                if self.values.chrome_follows {
                    self.values.chrome_opacity = self.values.window_opacity;
                }
            }
            RowId::ChromeOpacity => {
                self.values.chrome_follows = false;
                self.values.chrome_opacity = step_opacity(self.values.chrome_opacity, dir);
            }
            RowId::WindowBlur => {
                if dir < 0 {
                    self.values.window_blur = false;
                } else if dir > 0 {
                    self.values.window_blur = true;
                }
            }
            RowId::PaneActive => {
                self.values.pane_opacity_active =
                    step_opacity(self.values.pane_opacity_active, dir);
            }
            RowId::PaneInactive => {
                self.values.pane_opacity_inactive =
                    step_opacity(self.values.pane_opacity_inactive, dir);
            }
            RowId::ImageOpacity => {
                self.values.background_opacity = step_opacity(self.values.background_opacity, dir);
            }
            RowId::ImageBlur => {
                self.values.background_blur_px = step_blur(self.values.background_blur_px, dir);
            }
            RowId::BackgroundImage => {}
        }
    }

    fn jump(&mut self, t: f32) {
        let Some(id) = self.current() else {
            return;
        };
        if matches!(id, RowId::WindowBlur | RowId::BackgroundImage) {
            return;
        }
        self.set_slider(id, t);
    }

    fn set_slider(&mut self, id: RowId, t: f32) {
        let t = t.clamp(0.0, 1.0);
        match id {
            RowId::WindowOpacity => {
                self.values.window_opacity = snap_opacity(t);
                if self.values.chrome_follows {
                    self.values.chrome_opacity = self.values.window_opacity;
                }
            }
            RowId::ChromeOpacity => {
                self.values.chrome_follows = false;
                self.values.chrome_opacity = snap_opacity(t);
            }
            RowId::PaneActive => self.values.pane_opacity_active = snap_opacity(t),
            RowId::PaneInactive => self.values.pane_opacity_inactive = snap_opacity(t),
            RowId::ImageOpacity => self.values.background_opacity = snap_opacity(t),
            RowId::ImageBlur => {
                self.values.background_blur_px = (t * 64.0).round().clamp(0.0, 64.0) as u32;
            }
            RowId::WindowBlur | RowId::BackgroundImage => {}
        }
    }

    /// Returns whether a path error should keep the current notice.
    fn activate(&mut self) -> bool {
        match self.current() {
            Some(RowId::WindowBlur) => {
                self.values.window_blur = !self.values.window_blur;
                false
            }
            Some(RowId::ChromeOpacity) => {
                self.values.chrome_follows = !self.values.chrome_follows;
                if self.values.chrome_follows {
                    self.values.chrome_opacity = self.values.window_opacity;
                }
                false
            }
            Some(RowId::BackgroundImage) => self.commit_path(),
            _ => false,
        }
    }

    fn commit_path(&mut self) -> bool {
        let Some(edit) = self.path_edit.clone() else {
            self.path_edit = Some(self.values.background_image.clone().unwrap_or_default());
            return false;
        };
        let text = edit.trim();
        if text.is_empty() {
            self.values.background_image = None;
            self.path_edit = None;
            return false;
        }
        if !std::path::Path::new(text).is_absolute() {
            self.notice = Some("Background image must be an absolute path.".into());
            return true;
        }
        self.values.background_image = Some(text.to_string());
        self.path_edit = None;
        false
    }

    fn refresh_notice(&mut self) {
        if self
            .notice
            .as_deref()
            .is_some_and(|text| text.contains("absolute"))
        {
            return;
        }
        self.notice = if self.values.window_opacity < 1.0 && !self.limits.alpha {
            Some(
                if self.limits.gpu {
                    "This window uses --gpu and stays opaque."
                } else {
                    "This window stays opaque until a restart."
                }
                .into(),
            )
        } else {
            None
        };
    }

    fn reveal(&mut self, chrome: ChromeGeom, window_w: usize, window_h: usize) {
        let metrics = metrics(chrome, window_w, window_h);
        let top = self.selected * metrics.row_h;
        let bottom = top + metrics.row_h;
        if top < self.scroll {
            self.scroll = top;
        } else if bottom > self.scroll + metrics.list_h {
            self.scroll = bottom.saturating_sub(metrics.list_h);
        }
        self.scroll = self
            .scroll
            .min(max_scroll(metrics.list_h, metrics.content_h));
    }

    fn row_label(&self, id: RowId) -> String {
        match id {
            RowId::WindowOpacity => {
                format!("Window opacity {}", fmt_opacity(self.values.window_opacity))
            }
            RowId::ChromeOpacity => {
                if self.values.chrome_follows {
                    "Chrome opacity, follows window".into()
                } else {
                    format!("Chrome opacity {}", fmt_opacity(self.values.chrome_opacity))
                }
            }
            RowId::WindowBlur => format!(
                "Window blur {}",
                if self.values.window_blur { "on" } else { "off" }
            ),
            RowId::PaneActive => {
                format!(
                    "Active pane opacity {}",
                    fmt_opacity(self.values.pane_opacity_active)
                )
            }
            RowId::PaneInactive => format!(
                "Inactive pane opacity {}",
                fmt_opacity(self.values.pane_opacity_inactive)
            ),
            RowId::BackgroundImage => format!(
                "Background image {}",
                self.path_edit
                    .as_deref()
                    .or(self.values.background_image.as_deref())
                    .unwrap_or("none")
            ),
            RowId::ImageOpacity => {
                format!(
                    "Image opacity {}",
                    fmt_opacity(self.values.background_opacity)
                )
            }
            RowId::ImageBlur => format!("Image blur {} px", self.values.background_blur_px),
        }
    }
}

#[must_use]
pub(crate) fn cursor(hit: Option<Hit>, dragging: bool) -> CursorKind {
    if dragging {
        CursorKind::Grab
    } else if hit.is_some() {
        CursorKind::Pointer
    } else {
        CursorKind::Default
    }
}

struct Metrics {
    row_h: usize,
    list_h: usize,
    content_h: usize,
}

fn layout_chrome(scale_milli: u32) -> ChromeGeom {
    ChromeGeom {
        graphite: true,
        scale_milli,
    }
}

/// Design type size in device pixels. Geometry already follows
/// `scale_milli`; leaving type at a raw 13 px makes the dialog unreadably
/// small on a 2× display.
fn type_px(chrome: ChromeGeom, design: f32) -> f32 {
    chrome.px(design).max(1) as f32
}

/// Fixed 760×460 design frame, clamped so a short window still shows it.
/// The size does not depend on which rows or values are visible.
fn dialog_size(chrome: ChromeGeom, window_w: usize, window_h: usize) -> (usize, usize) {
    let margin = chrome.px(PAD);
    let fit = |design: f32, window: usize| {
        let max = window.saturating_sub(margin.saturating_mul(2)).max(1);
        chrome.px(design).min(max).max(1)
    };
    (fit(DIALOG_W, window_w), fit(DIALOG_H, window_h))
}

fn metrics(chrome: ChromeGeom, window_w: usize, window_h: usize) -> Metrics {
    let row_h = chrome.px(ROW_H).max(1);
    let (_, dialog_h) = dialog_size(chrome, window_w, window_h);
    let list_h = dialog_h
        .saturating_sub(chrome.px(HEADER_H))
        .saturating_sub(chrome.px(FOOTER_H));
    let content_h = row_h * ROWS.len() + chrome.px(LIMITS_H);
    Metrics {
        row_h,
        list_h,
        content_h,
    }
}

fn max_scroll(list_h: usize, content_h: usize) -> usize {
    content_h.saturating_sub(list_h)
}

fn scroll_by(scroll: usize, delta: i32, max: usize) -> usize {
    let next = if delta < 0 {
        scroll.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        scroll.saturating_add(delta as usize)
    };
    next.min(max)
}

fn snap_opacity(value: f32) -> f32 {
    (value.clamp(0.0, 1.0) * 100.0).round() / 100.0
}

fn hundredths(value: f32) -> i32 {
    (snap_opacity(value) * 100.0).round() as i32
}

fn opacity_changed(left: f32, right: f32) -> bool {
    hundredths(left) != hundredths(right)
}

fn step_opacity(value: f32, dir: i32) -> f32 {
    snap_opacity(value + 0.05 * dir as f32)
}

fn step_blur(value: u32, dir: i32) -> u32 {
    if dir < 0 {
        value.saturating_sub(1)
    } else {
        value.saturating_add(1).min(64)
    }
}

fn fmt_opacity(value: f32) -> String {
    format!("{:.2}", snap_opacity(value))
}

fn diff(before: &Values, after: &Values) -> Vec<Write> {
    let mut writes = Vec::new();
    if opacity_changed(before.window_opacity, after.window_opacity) {
        writes.push(Write::WindowOpacity(after.window_opacity));
    }
    if before.chrome_follows != after.chrome_follows
        || (!after.chrome_follows && opacity_changed(before.chrome_opacity, after.chrome_opacity))
    {
        writes.push(Write::ChromeOpacity(if after.chrome_follows {
            None
        } else {
            Some(after.chrome_opacity)
        }));
    }
    if before.window_blur != after.window_blur {
        writes.push(Write::WindowBlur(after.window_blur));
    }
    if opacity_changed(before.pane_opacity_active, after.pane_opacity_active) {
        writes.push(Write::PaneActive(after.pane_opacity_active));
    }
    if opacity_changed(before.pane_opacity_inactive, after.pane_opacity_inactive) {
        writes.push(Write::PaneInactive(after.pane_opacity_inactive));
    }
    if before.background_image != after.background_image {
        writes.push(Write::BackgroundImage(after.background_image.clone()));
    }
    if opacity_changed(before.background_opacity, after.background_opacity) {
        writes.push(Write::ImageOpacity(after.background_opacity));
    }
    if before.background_blur_px != after.background_blur_px {
        writes.push(Write::ImageBlur(after.background_blur_px));
    }
    writes
}

pub(crate) fn layout(
    dialog: &Dialog,
    chrome: ChromeGeom,
    window_w: usize,
    window_h: usize,
) -> Layout {
    let metrics = metrics(chrome, window_w, window_h);
    let (dialog_w, dialog_h) = dialog_size(chrome, window_w, window_h);
    let origin = Rect::new(
        window_w.saturating_sub(dialog_w) / 2,
        window_h.saturating_sub(dialog_h) / 2,
        dialog_w,
        dialog_h,
    );
    let pad = chrome.px(PAD);
    let header = chrome.px(HEADER_H);
    let preview_w = chrome.px(PREVIEW_W);
    let gap = chrome.px(GAP);
    let list = Rect::new(
        origin.x + pad,
        origin.y + header,
        dialog_w
            .saturating_sub(pad.saturating_mul(2))
            .saturating_sub(gap)
            .saturating_sub(preview_w),
        metrics.list_h,
    );
    let preview = Rect::new(list.right() + gap, list.y, preview_w, list.h);
    let scroll = dialog
        .scroll
        .min(max_scroll(metrics.list_h, metrics.content_h));
    let content_w = list.w.saturating_sub(chrome.px(SCROLLBAR_W));
    let mut rows = Vec::new();
    for (index, id) in ROWS.iter().enumerate() {
        let y = list.y + index * metrics.row_h;
        let raw = Rect::new(list.x, y.saturating_sub(scroll), content_w, metrics.row_h);
        let Some(bounds) = clip_rect(raw, list) else {
            rows.push(RowBox {
                id: *id,
                bounds: Rect::new(0, 0, 0, 0),
                slider: None,
                toggle: None,
                clear: None,
            });
            continue;
        };
        let slider = slider_rect(*id, raw, chrome).and_then(|rect| clip_rect(rect, list));
        let toggle = toggle_rect(*id, raw, chrome).and_then(|rect| clip_rect(rect, list));
        let clear = (*id == RowId::BackgroundImage)
            .then(|| clear_rect(raw, chrome))
            .and_then(|rect| clip_rect(rect, list));
        rows.push(RowBox {
            id: *id,
            bounds,
            slider,
            toggle,
            clear,
        });
    }
    let close_s = chrome.px(22.0).max(12);
    let close = Rect::new(
        origin.x + dialog_w - pad - close_s,
        origin.y + (header.saturating_sub(close_s)) / 2,
        close_s,
        close_s,
    );
    let max = max_scroll(metrics.list_h, metrics.content_h);
    let thumb = if max == 0 {
        None
    } else {
        let track_h = list.h.max(1);
        let thumb_h =
            (track_h * track_h / metrics.content_h.max(1)).clamp(chrome.px(24.0), track_h);
        let travel = track_h.saturating_sub(thumb_h);
        let thumb_y = list.y + scroll.saturating_mul(travel).checked_div(max).unwrap_or(0);
        Some(Rect::new(
            list.right().saturating_sub(chrome.px(SCROLLBAR_W)),
            thumb_y,
            chrome.px(SCROLLBAR_W).max(4),
            thumb_h,
        ))
    };
    let samples = preview_samples(chrome, preview);
    Layout {
        dialog: origin,
        list,
        scroll,
        max_scroll: max,
        row_h: metrics.row_h,
        rows,
        close,
        thumb,
        samples,
        preview,
        content_w,
        scale_milli: chrome.scale_milli,
    }
}

fn row_box(layout: &Layout, id: RowId) -> Option<&RowBox> {
    layout.rows.iter().find(|row| row.id == id)
}

fn slider_rect(id: RowId, row: Rect, chrome: ChromeGeom) -> Option<Rect> {
    if matches!(id, RowId::WindowBlur | RowId::BackgroundImage) {
        return None;
    }
    let h = chrome.px(6.0).max(4);
    let inset = chrome.px(8.0);
    let right_reserve = if id == RowId::ChromeOpacity {
        chrome.px(84.0)
    } else {
        chrome.px(44.0)
    };
    Some(Rect::new(
        row.x + inset,
        row.y + row.h.saturating_sub(h + chrome.px(8.0)),
        row.w.saturating_sub(inset * 2 + right_reserve),
        h,
    ))
}

fn toggle_rect(id: RowId, row: Rect, chrome: ChromeGeom) -> Option<Rect> {
    if !matches!(id, RowId::ChromeOpacity | RowId::WindowBlur) {
        return None;
    }
    let w = chrome.px(72.0);
    let h = chrome.px(22.0);
    Some(Rect::new(
        row.right().saturating_sub(w + chrome.px(8.0)),
        row.y + chrome.px(8.0),
        w,
        h,
    ))
}

fn clear_rect(row: Rect, chrome: ChromeGeom) -> Rect {
    let w = chrome.px(56.0);
    let h = chrome.px(22.0);
    Rect::new(
        row.right().saturating_sub(w + chrome.px(8.0)),
        row.y + (row.h.saturating_sub(h)) / 2,
        w,
        h,
    )
}

fn clip_rect(rect: Rect, view: Rect) -> Option<Rect> {
    let x0 = rect.x.max(view.x);
    let y0 = rect.y.max(view.y);
    let x1 = rect.right().min(view.right());
    let y1 = rect
        .y
        .saturating_add(rect.h)
        .min(view.y.saturating_add(view.h));
    if x0 < x1 && y0 < y1 {
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

fn slider_t(track: Rect, x: usize) -> f32 {
    if track.w <= 1 {
        return 0.0;
    }
    let delta = x.saturating_sub(track.x) as f32;
    (delta / (track.w - 1) as f32).clamp(0.0, 1.0)
}

pub(crate) fn hit(layout: &Layout, x: usize, y: usize) -> Option<Hit> {
    if layout.close.contains(x, y) {
        return Some(Hit::Close);
    }
    if layout.thumb.is_some_and(|thumb| thumb.contains(x, y)) {
        return Some(Hit::ScrollThumb);
    }
    if !layout.list.contains(x, y) {
        return None;
    }
    for row in &layout.rows {
        if row.toggle.is_some_and(|rect| rect.contains(x, y)) {
            return Some(Hit::Toggle(row.id));
        }
        if row.clear.is_some_and(|rect| rect.contains(x, y)) {
            return Some(Hit::ClearImage);
        }
        if row.slider.is_some_and(|rect| rect.contains(x, y)) {
            return Some(Hit::Slider(row.id));
        }
        if row.bounds.w > 0 && row.bounds.contains(x, y) {
            return Some(Hit::Row(row.id));
        }
    }
    None
}

pub(crate) fn paint(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    variant: ThemeVariant,
    dialog: &Dialog,
    layout: &Layout,
) {
    if stride == 0 || height == 0 || buffer.len() < stride * height {
        return;
    }
    dim(buffer, stride, height);
    let tok = graphite::tokens(variant);
    let card = if variant == ThemeVariant::Light {
        [0xff, 0xff, 0xff]
    } else {
        [0x18, 0x1b, 0x21]
    };
    graphite::fill_round_rect(
        buffer,
        stride,
        Rect::new(
            layout.dialog.x.saturating_sub(1),
            layout.dialog.y.saturating_sub(1),
            layout.dialog.w.saturating_add(2),
            layout.dialog.h.saturating_add(2),
        ),
        RADIUS + 1.0,
        tok.hairline,
        255,
    );
    graphite::fill_round_rect(buffer, stride, layout.dialog, RADIUS, card, 255);
    let accent = graphite::accent(
        tok,
        if variant == ThemeVariant::Light {
            [0x2f, 0x6f, 0xd0]
        } else {
            [0x5a, 0xa2, 0xff]
        },
    );
    paint_header(buffer, stride, tok, dialog, layout, card);
    paint_list(buffer, stride, height, tok, accent, dialog, layout, card);
    paint_preview(buffer, stride, height, variant, tok, accent, dialog, layout);
    paint_footer(buffer, stride, tok, dialog, layout);
}

fn dim(buffer: &mut [u32], stride: usize, height: usize) {
    for y in 0..height {
        for x in 0..stride {
            let idx = y * stride + x;
            let px = buffer[idx];
            let rgb = unpack_rgb(px);
            let scale = |channel: u8| (f32::from(channel) * 0.45).round() as u8;
            buffer[idx] = pack_argb(alpha_of(px), [scale(rgb[0]), scale(rgb[1]), scale(rgb[2])]);
        }
    }
}

fn mid_y(rect: Rect) -> f32 {
    rect.y as f32 + rect.h as f32 / 2.0
}

fn paint_header(
    buffer: &mut [u32],
    stride: usize,
    tok: &graphite::Tokens,
    dialog: &Dialog,
    layout: &Layout,
    card: [u8; 3],
) {
    let _ = (dialog, card);
    let chrome = layout_chrome(layout.scale_milli);
    graphite::draw_text(
        buffer,
        stride,
        layout.list.x as f32,
        mid_y(layout.close),
        Face::SemiBold,
        type_px(chrome, 16.0),
        "Transparency",
        tok.text_strong,
        layout.dialog.x,
        layout.close.x,
    );
    graphite::fill_round_rect(buffer, stride, layout.close, 6.0, tok.field, 255);
    let mark = type_px(chrome, 14.0);
    graphite::draw_text(
        buffer,
        stride,
        layout.close.x as f32 + (layout.close.w as f32 - mark) / 2.0,
        mid_y(layout.close),
        Face::Regular,
        mark,
        "×",
        tok.text,
        layout.close.x,
        layout.close.right(),
    );
}

fn paint_footer(
    buffer: &mut [u32],
    stride: usize,
    tok: &graphite::Tokens,
    dialog: &Dialog,
    layout: &Layout,
) {
    let text = dialog
        .notice
        .as_deref()
        .unwrap_or("↑↓ move    ←→ adjust    Enter toggle    Esc close");
    let ink = if dialog.notice.is_some() {
        tok.attention
    } else {
        tok.muted
    };
    let chrome = layout_chrome(layout.scale_milli);
    let y = layout.dialog.y + layout.dialog.h - chrome.px(FOOTER_H) / 2;
    graphite::draw_text(
        buffer,
        stride,
        layout.list.x as f32,
        y as f32,
        Face::Regular,
        type_px(chrome, 12.0),
        text,
        ink,
        layout.dialog.x,
        layout.dialog.right(),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_list(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    tok: &graphite::Tokens,
    accent: [u8; 3],
    dialog: &Dialog,
    layout: &Layout,
    card: [u8; 3],
) {
    let content_w = layout.content_w.max(1);
    let content_h = layout.max_scroll + layout.list.h;
    let mut content = vec![pack_argb(255, card); content_w * content_h.max(1)];
    let scale = if layout.row_h == 0 {
        1000
    } else {
        (layout.row_h as u32 * 1000) / (ROW_H as u32).max(1)
    };
    let chrome = ChromeGeom {
        graphite: true,
        scale_milli: scale,
    };
    for (index, id) in ROWS.iter().enumerate() {
        let row = Rect::new(0, index * layout.row_h, content_w, layout.row_h);
        if index == dialog.selected {
            fill_solid(&mut content, content_w, content_h, row, tok.tab_hover);
        }
        paint_row(
            &mut content,
            content_w,
            content_h,
            tok,
            accent,
            chrome,
            dialog,
            *id,
            row,
        );
    }
    paint_limits(&mut content, content_w, content_h, tok, layout);
    blit(
        buffer,
        stride,
        height,
        &content,
        content_w,
        layout.list,
        layout.scroll,
        layout.list.w.saturating_sub(layout.content_w),
    );
    if let Some(thumb) = layout.thumb {
        graphite::fill_round_rect(buffer, stride, thumb, 3.0, tok.muted, 255);
    }
}

fn paint_limits(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    tok: &graphite::Tokens,
    layout: &Layout,
) {
    let chrome = layout_chrome(layout.scale_milli);
    let pad = chrome.px(8.0);
    let line_h = chrome.px(22.0).max(1);
    let top = ROWS.len() * layout.row_h + pad;
    for (index, line) in LIMIT_LINES.iter().enumerate() {
        let face = if index == 0 {
            Face::SemiBold
        } else {
            Face::Regular
        };
        let ink = if index == 0 { tok.text } else { tok.muted };
        let y = top + index * line_h;
        if y + line_h >= height {
            break;
        }
        graphite::draw_text(
            buffer,
            stride,
            pad as f32,
            y as f32 + pad as f32,
            face,
            type_px(chrome, 12.0),
            line,
            ink,
            0,
            stride,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    tok: &graphite::Tokens,
    accent: [u8; 3],
    chrome: ChromeGeom,
    dialog: &Dialog,
    id: RowId,
    row: Rect,
) {
    let label_y = row.y as f32 + (row.h as f32 * 0.32);
    let value = row_value(dialog, id, chrome);
    let value_px = type_px(chrome, 12.0);
    let value_w = graphite::text_width(Face::Regular, value_px, &value);
    let inset = type_px(chrome, 8.0);
    let value_right = if id == RowId::BackgroundImage {
        clear_rect(row, chrome).x
    } else {
        row.right()
    };
    let value_x = value_right as f32 - value_w - type_px(chrome, 12.0);
    graphite::draw_text(
        buffer,
        stride,
        row.x as f32 + inset,
        label_y,
        Face::Regular,
        type_px(chrome, 13.0),
        row_title(id),
        tok.text,
        row.x,
        value_x.max(row.x as f32) as usize,
    );
    graphite::draw_text(
        buffer,
        stride,
        value_x,
        label_y,
        Face::Regular,
        value_px,
        &value,
        tok.muted,
        row.x,
        row.right(),
    );
    if let Some(track) = slider_rect(id, row, chrome) {
        fill_solid(buffer, stride, height, track, tok.hairline);
        let portion = slider_portion(dialog, id);
        let fill_w = ((track.w as f32) * portion).round() as usize;
        fill_solid(
            buffer,
            stride,
            height,
            Rect::new(track.x, track.y, fill_w.max(1).min(track.w), track.h),
            if id == RowId::ChromeOpacity && dialog.values.chrome_follows {
                tok.muted
            } else {
                accent
            },
        );
    }
    if let Some(toggle) = toggle_rect(id, row, chrome) {
        let on = match id {
            RowId::WindowBlur => dialog.values.window_blur,
            RowId::ChromeOpacity => dialog.values.chrome_follows,
            _ => false,
        };
        fill_solid(
            buffer,
            stride,
            height,
            toggle,
            if on { accent } else { tok.hairline },
        );
        let label = match id {
            RowId::WindowBlur => {
                if on {
                    "On"
                } else {
                    "Off"
                }
            }
            RowId::ChromeOpacity => "Follow",
            _ => "",
        };
        graphite::draw_text(
            buffer,
            stride,
            toggle.x as f32 + type_px(chrome, 8.0),
            mid_y(toggle),
            Face::Regular,
            type_px(chrome, 11.0),
            label,
            if on { [255, 255, 255] } else { tok.text },
            toggle.x,
            toggle.right(),
        );
    }
    if id == RowId::BackgroundImage {
        let clear = clear_rect(row, chrome);
        fill_solid(buffer, stride, height, clear, tok.field);
        graphite::draw_text(
            buffer,
            stride,
            clear.x as f32 + type_px(chrome, 10.0),
            mid_y(clear),
            Face::Regular,
            type_px(chrome, 12.0),
            "Clear",
            tok.text,
            clear.x,
            clear.right(),
        );
    }
}

fn row_title(id: RowId) -> &'static str {
    match id {
        RowId::WindowOpacity => "Window opacity",
        RowId::ChromeOpacity => "Chrome opacity",
        RowId::WindowBlur => "Window blur",
        RowId::PaneActive => "Active pane opacity",
        RowId::PaneInactive => "Inactive pane opacity",
        RowId::BackgroundImage => "Background image",
        RowId::ImageOpacity => "Image opacity",
        RowId::ImageBlur => "Image blur",
    }
}

fn row_value(dialog: &Dialog, id: RowId, chrome: ChromeGeom) -> String {
    match id {
        RowId::WindowOpacity => fmt_opacity(dialog.values.window_opacity),
        RowId::ChromeOpacity if dialog.values.chrome_follows => "Follow".into(),
        RowId::ChromeOpacity => fmt_opacity(dialog.values.chrome_opacity),
        RowId::WindowBlur => {
            if dialog.values.window_blur {
                "On".into()
            } else {
                "Off".into()
            }
        }
        RowId::PaneActive => fmt_opacity(dialog.values.pane_opacity_active),
        RowId::PaneInactive => fmt_opacity(dialog.values.pane_opacity_inactive),
        RowId::BackgroundImage => graphite::ellipsize(
            Face::Regular,
            type_px(chrome, 12.0),
            dialog
                .path_edit
                .as_deref()
                .or(dialog.values.background_image.as_deref())
                .unwrap_or("None"),
            type_px(chrome, 180.0),
        ),
        RowId::ImageOpacity => fmt_opacity(dialog.values.background_opacity),
        RowId::ImageBlur => format!("{} px", dialog.values.background_blur_px),
    }
}

fn slider_portion(dialog: &Dialog, id: RowId) -> f32 {
    match id {
        RowId::WindowOpacity => dialog.values.window_opacity,
        RowId::ChromeOpacity => {
            if dialog.values.chrome_follows {
                dialog.values.window_opacity
            } else {
                dialog.values.chrome_opacity
            }
        }
        RowId::PaneActive => dialog.values.pane_opacity_active,
        RowId::PaneInactive => dialog.values.pane_opacity_inactive,
        RowId::ImageOpacity => dialog.values.background_opacity,
        RowId::ImageBlur => dialog.values.background_blur_px as f32 / 64.0,
        RowId::WindowBlur | RowId::BackgroundImage => 0.0,
    }
}

#[allow(clippy::too_many_arguments)]
fn blit(
    dst: &mut [u32],
    dst_stride: usize,
    dst_h: usize,
    src: &[u32],
    src_stride: usize,
    view: Rect,
    scroll: usize,
    gutter: usize,
) {
    for row in 0..view.h {
        let sy = scroll + row;
        let dy = view.y + row;
        if dy >= dst_h {
            break;
        }
        for col in 0..view.w.saturating_sub(gutter) {
            let sx = col;
            if sx >= src_stride {
                break;
            }
            let dx = view.x + col;
            if dx >= dst_stride {
                break;
            }
            let Some(px) = src.get(sy * src_stride + sx).copied() else {
                continue;
            };
            dst[dy * dst_stride + dx] = px;
        }
    }
}

struct PreviewParts {
    stripes: [([u8; 3], Rect); 4],
    ground: Rect,
    bar: Rect,
    chip: Rect,
    badge: Rect,
    field: Rect,
    active: Rect,
    inactive: Rect,
    ring: Rect,
    spaces: Rect,
    text: Rect,
    cursor: Rect,
    dot: Rect,
    samples: Samples,
}

fn preview_parts(chrome: ChromeGeom, preview: Rect) -> PreviewParts {
    let inset = chrome.px(12.0).clamp(8, (preview.w / 10).max(8));
    let well = Rect::new(
        preview.x + 4,
        preview.y + 4,
        preview.w.saturating_sub(8),
        preview.h.saturating_sub(8),
    );
    let stripe_w = (well.w / 4).max(1);
    let stripes = [
        (
            [0x3a, 0x6e, 0xa5],
            Rect::new(well.x, well.y, stripe_w, well.h),
        ),
        (
            [0xc4, 0x7a, 0x3a],
            Rect::new(well.x + stripe_w, well.y, stripe_w, well.h),
        ),
        (
            [0x2f, 0x8f, 0x6b],
            Rect::new(well.x + stripe_w * 2, well.y, stripe_w, well.h),
        ),
        (
            [0x8a, 0x4e, 0x9a],
            Rect::new(
                well.x + stripe_w * 3,
                well.y,
                well.w.saturating_sub(stripe_w * 3),
                well.h,
            ),
        ),
    ];
    let bar_h = chrome.px(28.0).max(16);
    let chip_w = chrome.px(48.0).max(24);
    let chip_h = chrome.px(20.0).max(12);
    let badge_w = chrome.px(52.0).max(28);
    let badge_h = chrome.px(16.0).max(10);
    let field_w = chrome.px(56.0).max(28);
    let field_h = chrome.px(20.0).max(12);
    let bar = Rect::new(
        well.x + inset,
        well.y + inset,
        well.w.saturating_sub(inset * 2),
        bar_h,
    );
    let chip = Rect::new(
        bar.x + chrome.px(4.0),
        bar.y + chrome.px(4.0),
        chip_w,
        chip_h,
    );
    let badge = Rect::new(
        chip.right() + chrome.px(6.0),
        bar.y + chrome.px(6.0),
        badge_w,
        badge_h,
    );
    let field = Rect::new(
        bar.right().saturating_sub(field_w + chrome.px(4.0)),
        bar.y + chrome.px(4.0),
        field_w,
        field_h,
    );
    let spaces_h = chrome.px(22.0).max(14);
    let spaces = Rect::new(
        well.x + inset,
        well.y + well.h.saturating_sub(inset + spaces_h),
        well.w.saturating_sub(inset * 2),
        spaces_h,
    );
    let pane_gap = chrome.px(8.0).max(4);
    let pane_top = bar.y + bar.h + chrome.px(10.0);
    let pane_h = spaces.y.saturating_sub(chrome.px(16.0) + pane_top).max(24);
    let pane_w = well.w.saturating_sub(inset * 2 + pane_gap) / 2;
    let active = Rect::new(well.x + inset, pane_top, pane_w, pane_h);
    let inactive = Rect::new(active.right() + pane_gap, pane_top, pane_w, pane_h);
    let ring = Rect::new(active.x, active.y, active.w, chrome.px(2.0).max(2));
    let dot = Rect::new(
        active.x + chrome.px(8.0),
        active.y + chrome.px(8.0),
        chrome.px(6.0).max(4),
        chrome.px(6.0).max(4),
    );
    let text = Rect::new(
        active.x + chrome.px(18.0),
        active.y + chrome.px(8.0),
        chrome.px(28.0),
        chrome.px(8.0).max(4),
    );
    let cursor = Rect::new(
        text.right() + chrome.px(2.0),
        active.y + chrome.px(6.0),
        chrome.px(2.0).max(1),
        chrome.px(12.0).max(8),
    );
    let ground_sample = (
        well.x + well.w / 2,
        spaces.y.saturating_sub(chrome.px(8.0).max(4)),
    );
    let ground_under = stripe_at(&stripes, ground_sample.0);
    let bar_gap_x = badge.right() + chrome.px(4.0);
    let bar_sample = (
        bar_gap_x + field.x.saturating_sub(bar_gap_x) / 2,
        bar.y + bar.h / 2,
    );
    PreviewParts {
        samples: Samples {
            ground: ground_sample,
            bar: bar_sample,
            pane: (inactive.x + inactive.w / 2, inactive.y + inactive.h / 2),
            text: (text.x + text.w / 2, text.y + text.h / 2),
            cursor: (cursor.x, cursor.y + cursor.h / 2),
            dot: (dot.x + dot.w / 2, dot.y + dot.h / 2),
            badge: (badge.x + 3, badge.y + badge.h / 2),
            ring: (ring.x + ring.w / 2, ring.y),
            chip: (chip.x + 3, chip.y + chip.h / 2),
            field: (field.x + 3, field.y + field.h / 2),
            ground_under,
            bar_under: stripe_at(&stripes, bar_sample.0),
        },
        stripes,
        ground: well,
        bar,
        chip,
        badge,
        field,
        active,
        inactive,
        ring,
        spaces,
        text,
        cursor,
        dot,
    }
}

fn stripe_at(stripes: &[([u8; 3], Rect)], x: usize) -> [u8; 3] {
    stripes
        .iter()
        .find(|(_, rect)| rect.contains(x, rect.y))
        .map(|(rgb, _)| *rgb)
        .unwrap_or(stripes[0].0)
}

fn preview_samples(chrome: ChromeGeom, preview: Rect) -> Samples {
    preview_parts(chrome, preview).samples
}

#[allow(clippy::too_many_arguments)]
fn paint_preview(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    variant: ThemeVariant,
    tok: &graphite::Tokens,
    accent: [u8; 3],
    dialog: &Dialog,
    layout: &Layout,
) {
    let parts = preview_parts(
        ChromeGeom {
            graphite: true,
            scale_milli: layout.scale_milli,
        },
        layout.preview,
    );
    for (rgb, rect) in parts.stripes {
        fill_solid(buffer, stride, height, rect, rgb);
    }
    let ground = if variant == ThemeVariant::Light {
        [0xe9, 0xec, 0xf0]
    } else {
        tok.ground
    };
    let pane = if variant == ThemeVariant::Light {
        [0xff, 0xff, 0xff]
    } else {
        [0x18, 0x1b, 0x21]
    };
    let window_a = opacity_to_alpha(dialog.values.window_opacity);
    let chrome = if dialog.values.chrome_follows {
        dialog.values.window_opacity
    } else {
        dialog.values.chrome_opacity
    };
    let chrome_a = opacity_to_alpha(chrome);
    let active_a =
        opacity_to_alpha(dialog.values.window_opacity * dialog.values.pane_opacity_active);
    let inactive_a =
        opacity_to_alpha(dialog.values.window_opacity * dialog.values.pane_opacity_inactive);
    fill_over(buffer, stride, height, parts.ground, ground, window_a);
    fill_over(buffer, stride, height, parts.bar, tok.bar, chrome_a);
    fill_over(
        buffer,
        stride,
        height,
        parts.spaces,
        tok.status_bar,
        chrome_a,
    );
    fill_over(buffer, stride, height, parts.inactive, pane, inactive_a);
    fill_solid(buffer, stride, height, parts.ring, accent);
    fill_over(
        buffer,
        stride,
        height,
        Rect::new(
            parts.active.x,
            parts.active.y + 2,
            parts.active.w,
            parts.active.h.saturating_sub(2),
        ),
        pane,
        active_a,
    );
    // Opaque chrome: the chip, command field, badge, dot, text, and cursor.
    let chip = if variant == ThemeVariant::Light {
        [0xff, 0xff, 0xff]
    } else {
        tok.tab_active
    };
    fill_solid(buffer, stride, height, parts.chip, chip);
    fill_solid(buffer, stride, height, parts.field, tok.field);
    fill_solid(buffer, stride, height, parts.badge, tok.attention);
    fill_solid(buffer, stride, height, parts.dot, tok.working);
    fill_solid(buffer, stride, height, parts.text, tok.text);
    fill_solid(buffer, stride, height, parts.cursor, accent);
    let ui = layout_chrome(layout.scale_milli);
    let inset = type_px(ui, 8.0);
    graphite::draw_text(
        buffer,
        stride,
        parts.badge.x as f32 + inset,
        mid_y(parts.badge),
        Face::SemiBold,
        type_px(ui, 9.0),
        "needs you",
        tok.on_attention,
        parts.badge.x + ui.px(4.0),
        parts.badge.right(),
    );
    graphite::draw_text(
        buffer,
        stride,
        parts.field.x as f32 + inset,
        mid_y(parts.field),
        Face::Regular,
        type_px(ui, 9.0),
        "Run",
        tok.muted,
        parts.field.x + ui.px(4.0),
        parts.field.right(),
    );
}

fn fill_solid(buffer: &mut [u32], stride: usize, height: usize, rect: Rect, rgb: [u8; 3]) {
    fill_over(buffer, stride, height, rect, rgb, 255);
}

fn fill_over(
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    rect: Rect,
    rgb: [u8; 3],
    alpha: u8,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let y1 = rect.y.saturating_add(rect.h).min(height);
    let x1 = rect.right().min(stride);
    let y0 = rect.y.min(height);
    let x0 = rect.x.min(stride);
    for y in y0..y1 {
        for x in x0..x1 {
            let idx = y * stride + x;
            let Some(dst) = buffer.get_mut(idx) else {
                continue;
            };
            *dst = composite(*dst, rgb, alpha);
        }
    }
}

fn composite(dst: u32, src: [u8; 3], src_a: u8) -> u32 {
    if src_a == 255 {
        return pack_argb(255, src);
    }
    if src_a == 0 {
        return dst;
    }
    let da = alpha_of(dst);
    let dr = unpack_rgb(dst);
    let sa = f32::from(src_a) / 255.0;
    let da_f = f32::from(da) / 255.0;
    let out_a = sa + da_f * (1.0 - sa);
    if out_a <= 0.0 {
        return 0;
    }
    let mix = |s: u8, d: u8| {
        ((f32::from(s) * sa + f32::from(d) * da_f * (1.0 - sa)) / out_a).round() as u8
    };
    pack_argb(
        (out_a * 255.0).round() as u8,
        [mix(src[0], dr[0]), mix(src[1], dr[1]), mix(src[2], dr[2])],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome() -> ChromeGeom {
        ChromeGeom {
            graphite: true,
            scale_milli: 1000,
        }
    }

    fn sample_dialog() -> Dialog {
        let file = ConfigFile {
            window_opacity: Some(0.82),
            window_blur: Some(true),
            pane_opacity_inactive: Some(0.85),
            background_opacity: Some(0.35),
            ..ConfigFile::default()
        };
        Dialog::from_config(
            &file,
            SessionLimits {
                alpha: true,
                gpu: false,
            },
        )
    }

    fn frame(dialog: &Dialog) -> Layout {
        layout(dialog, chrome(), 1200, 800)
    }

    #[test]
    fn dialog_stays_760_by_460_while_the_list_scrolls() {
        let mut dialog = sample_dialog();
        let first = frame(&dialog);
        assert_eq!(first.dialog.w, 760);
        assert_eq!(first.dialog.h, 460);
        assert!(first.max_scroll > 0, "the limits block must scroll inside");
        let edit = dialog.edit(Input::Wheel(3), chrome(), 1200, 800);
        assert!(edit.writes.is_empty());
        assert!(dialog.scroll > 0);
        let scrolled = frame(&dialog);
        assert_eq!(scrolled.dialog, first.dialog);
        assert_eq!(scrolled.list, first.list);
        assert!(scrolled.scroll <= scrolled.max_scroll);
    }

    #[test]
    fn retina_type_scales_and_a_short_window_does_not_resize_the_dialog() {
        let dialog = sample_dialog();
        let retina = ChromeGeom {
            graphite: true,
            scale_milli: 2000,
        };
        assert_eq!(type_px(retina, 13.0), 26.0);
        assert_eq!(type_px(chrome(), 13.0), 13.0);
        let large = layout(&dialog, retina, 2400, 1600);
        assert_eq!((large.dialog.w, large.dialog.h), (1520, 920));
        assert!(large.max_scroll > 0, "limits still scroll inside the frame");
        let short = layout(&dialog, retina, 1000, 700);
        assert!(short.dialog.w <= 1000 - 64);
        assert!(short.dialog.h <= 700 - 64);
        assert!(short.dialog.right() <= 1000);
        assert!(short.dialog.y + short.dialog.h <= 700);
        assert!(short.dialog.w < large.dialog.w);
        let mut scrolled = dialog.clone();
        let _ = scrolled.edit(Input::Wheel(5), retina, 1000, 700);
        assert!(scrolled.scroll > 0);
        let after = layout(&scrolled, retina, 1000, 700);
        assert_eq!(after.dialog, short.dialog);
        assert_eq!(after.list.w, short.list.w);
        assert_eq!(after.list.h, short.list.h);
    }

    #[test]
    fn clicks_hit_controls_and_use_the_pointing_hand() {
        let dialog = sample_dialog();
        let layout = frame(&dialog);
        let close = hit(&layout, layout.close.x + 2, layout.close.y + 2);
        assert_eq!(close, Some(Hit::Close));
        assert_eq!(cursor(close, false), CursorKind::Pointer);
        let window = layout
            .rows
            .iter()
            .find(|row| row.id == RowId::WindowOpacity)
            .and_then(|row| row.slider)
            .expect("window slider");
        assert_eq!(
            hit(&layout, window.x + window.w / 2, window.y + 1),
            Some(Hit::Slider(RowId::WindowOpacity))
        );
        assert_eq!(
            cursor(Some(Hit::Slider(RowId::WindowOpacity)), true),
            CursorKind::Grab
        );
        assert_eq!(cursor(None, false), CursorKind::Default);
        assert!(hit(&layout, 1, 1).is_none());
    }

    #[test]
    fn edits_write_existing_keys_and_follow_clears_chrome_opacity() {
        let mut dialog = sample_dialog();
        assert!(dialog.values.chrome_follows);
        let edit = dialog.edit(Input::Right, chrome(), 1200, 800);
        assert_eq!(edit.writes, vec![Write::WindowOpacity(0.87)]);
        assert!(dialog.values.chrome_follows);
        dialog.selected = 1;
        let edit = dialog.edit(Input::Right, chrome(), 1200, 800);
        assert!(!dialog.values.chrome_follows);
        assert!(matches!(
            edit.writes.as_slice(),
            [Write::ChromeOpacity(Some(_))]
        ));
        let edit = dialog.edit(Input::Enter, chrome(), 1200, 800);
        assert_eq!(edit.writes, vec![Write::ChromeOpacity(None)]);
        assert!(dialog.values.chrome_follows);
        dialog.selected = 2;
        let edit = dialog.edit(Input::Enter, chrome(), 1200, 800);
        assert_eq!(edit.writes, vec![Write::WindowBlur(false)]);
    }

    #[test]
    fn relative_image_path_is_not_written() {
        let mut dialog = sample_dialog();
        dialog.selected = 5;
        dialog.path_edit = Some("relative.png".into());
        let edit = dialog.edit(Input::Enter, chrome(), 1200, 800);
        assert!(edit.writes.is_empty());
        assert!(dialog.notice.as_deref().unwrap().contains("absolute"));
        assert!(dialog.values.background_image.is_none());
        dialog.path_edit = Some("/tmp/wall.png".into());
        let edit = dialog.edit(Input::Enter, chrome(), 1200, 800);
        assert_eq!(
            edit.writes,
            vec![Write::BackgroundImage(Some("/tmp/wall.png".into()))]
        );
    }

    #[test]
    fn preview_keeps_marks_opaque_and_lets_the_ground_show_the_desktop() {
        let dialog = sample_dialog();
        let layout = frame(&dialog);
        let mut buffer = vec![pack_argb(255, [20, 20, 20]); 1200 * 800];
        paint(&mut buffer, 1200, 800, ThemeVariant::Dark, &dialog, &layout);
        let ground = pixel(&buffer, 1200, layout.samples.ground);
        let mixed = composite(
            pack_argb(255, layout.samples.ground_under),
            graphite::tokens(ThemeVariant::Dark).ground,
            opacity_to_alpha(0.82),
        );
        assert_eq!(ground, mixed);
        assert_ne!(
            unpack_rgb(ground),
            graphite::tokens(ThemeVariant::Dark).ground
        );
        let tok = graphite::tokens(ThemeVariant::Dark);
        let under_bar = composite(
            pack_argb(255, layout.samples.bar_under),
            tok.ground,
            opacity_to_alpha(0.82),
        );
        let bar = pixel(&buffer, 1200, layout.samples.bar);
        assert_eq!(
            bar,
            composite(under_bar, tok.bar, opacity_to_alpha(0.82)),
            "the bar background is translucent between the opaque chip and field"
        );
        for sample in [
            layout.samples.text,
            layout.samples.cursor,
            layout.samples.dot,
            layout.samples.badge,
            layout.samples.ring,
            layout.samples.chip,
            layout.samples.field,
        ] {
            let px = pixel(&buffer, 1200, sample);
            assert_eq!(alpha_of(px), 255, "{sample:?} stays opaque, pixel {px:#x}");
            assert_ne!(
                unpack_rgb(px),
                unpack_rgb(mixed),
                "{sample:?} is not the translucent ground"
            );
        }
        let pane = pixel(&buffer, 1200, layout.samples.pane);
        assert_ne!(unpack_rgb(pane), [0xff, 0xff, 0xff]);
        assert_eq!(alpha_of(pane), 255, "composited over an opaque desktop");
        maybe_dump_transparency_pngs(&dialog);
    }

    fn maybe_dump_transparency_pngs(dialog: &Dialog) {
        let Ok(dir) = std::env::var("PRISMATTYC_DUMP_TRANSPARENCY") else {
            return;
        };
        if dir.is_empty() {
            return;
        }
        let root = std::path::Path::new(&dir);
        let _ = std::fs::create_dir_all(root);
        for (variant, name) in [
            (ThemeVariant::Dark, "graphite-transparency-dark.png"),
            (ThemeVariant::Light, "graphite-transparency-light.png"),
        ] {
            let mut buffer = vec![pack_argb(255, [32, 36, 44]); 1200 * 800];
            let laid = frame(dialog);
            paint(&mut buffer, 1200, 800, variant, dialog, &laid);
            let path = root.join(name);
            write_rgba_png(&path, &buffer, 1200, 800)
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        }
        let retina = ChromeGeom {
            graphite: true,
            scale_milli: 2000,
        };
        for (variant, name) in [
            (ThemeVariant::Dark, "graphite-transparency-dark-2x.png"),
            (ThemeVariant::Light, "graphite-transparency-light-2x.png"),
        ] {
            let laid = layout(dialog, retina, 1800, 1200);
            let mut buffer = vec![pack_argb(255, [32, 36, 44]); 1800 * 1200];
            paint(&mut buffer, 1800, 1200, variant, dialog, &laid);
            let path = root.join(name);
            write_rgba_png(&path, &buffer, 1800, 1200)
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        }
    }

    fn write_rgba_png(
        path: &std::path::Path,
        pixels: &[u32],
        width: u32,
        height: u32,
    ) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
        let mut rgba = vec![0u8; pixels.len() * 4];
        for (index, px) in pixels.iter().enumerate() {
            let offset = index * 4;
            rgba[offset] = ((*px >> 16) & 0xff) as u8;
            rgba[offset + 1] = ((*px >> 8) & 0xff) as u8;
            rgba[offset + 2] = (*px & 0xff) as u8;
            rgba[offset + 3] = (*px >> 24) as u8;
        }
        writer
            .write_image_data(&rgba)
            .map_err(std::io::Error::other)
    }

    #[test]
    fn opaque_window_without_alpha_explains_the_restart() {
        let mut dialog = sample_dialog();
        dialog.limits.alpha = false;
        dialog.values.window_opacity = 1.0;
        let edit = dialog.edit(Input::Left, chrome(), 1200, 800);
        assert!(matches!(edit.writes.as_slice(), [Write::WindowOpacity(_)]));
        assert!(dialog.notice.unwrap().contains("restart"));
        dialog.limits.gpu = true;
        dialog.limits.alpha = false;
        dialog.notice = None;
        let _ = dialog.edit(Input::Left, chrome(), 1200, 800);
        assert!(dialog.notice.unwrap().contains("--gpu"));
    }

    #[test]
    fn classic_chrome_does_not_open_the_dialog() {
        assert!(!opens_for(config::ChromeStyle::Classic));
        assert!(opens_for(config::ChromeStyle::Graphite));
    }

    fn pixel(buffer: &[u32], stride: usize, at: (usize, usize)) -> u32 {
        buffer[at.1 * stride + at.0]
    }
}
