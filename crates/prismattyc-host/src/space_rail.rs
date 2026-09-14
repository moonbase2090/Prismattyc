//! Spaces rail (PT-91): the saved spaces (`spaces/*.json`) as fixed-width
//! chips on one edge of the window, the space counterpart of the tab strip.
//!
//! The rail is a view over the spaces directory. It never owns a Session:
//! a chip opens a space through `pmux space open`, renames the file, or
//! deletes it after a confirm. Layout and hit-testing live here so the
//! paint code and the input code read one geometry.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::mux::{effective_tab_end_pad, tab_close_left, HostGeom};

/// Where the rail sits. `Off` reserves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RailSide {
    #[default]
    Bottom,
    Left,
    Top,
    Right,
    Off,
}

impl RailSide {
    /// Config spelling (`space_rail = "bottom"`).
    pub fn parse(spec: &str) -> Option<Self> {
        match spec.trim().to_ascii_lowercase().as_str() {
            "bottom" => Some(Self::Bottom),
            "left" => Some(Self::Left),
            "top" => Some(Self::Top),
            "right" => Some(Self::Right),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Top => "top",
            Self::Right => "right",
            Self::Off => "off",
        }
    }

    /// One row of chips (bottom/top) rather than one column (left/right).
    pub fn horizontal(self) -> bool {
        matches!(self, Self::Bottom | Self::Top)
    }
}

/// Reserved thickness of the rail in pixels for a side.
pub fn rail_thickness_px(side: RailSide, cell_w: usize, cell_h: usize, chip_cols: usize) -> usize {
    match side {
        RailSide::Off => 0,
        RailSide::Bottom | RailSide::Top => cell_h.max(1),
        RailSide::Left | RailSide::Right => cell_w.max(1).saturating_mul(chip_cols.max(1)),
    }
}

/// Inset of chip text from the chip's left edge (matches the tab strip).
pub const RAIL_LABEL_INSET: usize = 4;

/// Widest chip when `space_rail_chip_cols = 0` (PT-123).
pub const DEFAULT_CHIP_CAP: usize = 28;
/// Narrowest chip: room for a few glyphs plus the close cell.
pub const MIN_CHIP_COLS: usize = 6;

/// Width of one chip in cells: the label, the close/marker cell, and one
/// cell of room, clamped to `MIN_CHIP_COLS..=cap` (`cap` is
/// `space_rail_chip_cols`, or [`DEFAULT_CHIP_CAP`] when that is 0).
pub fn chip_cells_for(name_cells: usize, cap: usize) -> usize {
    let cap = cap.max(MIN_CHIP_COLS);
    // Three cells over the text: the close/marker cell (which carries
    // TAB_CLOSE_INSET) and the label insets on both sides.
    name_cells.saturating_add(3).clamp(MIN_CHIP_COLS, cap)
}

/// Effective cap for a `space_rail_chip_cols` value (0 = default).
pub fn chip_cap(config_cols: usize) -> usize {
    if config_cols == 0 {
        DEFAULT_CHIP_CAP
    } else {
        config_cols
    }
}

/// Display cells of a chip label.
pub fn name_cells(name: &str) -> usize {
    name.chars()
        .map(|ch| prismattyc_core::char_display_width(ch).max(1))
        .sum()
}

/// Hit in the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailHit {
    /// A space chip, or its close glyph.
    Chip { index: usize, close: bool },
    /// The trailing `+` chip (create a fresh Space).
    Plus,
    /// Open the searchable list of all Spaces.
    Overflow,
    /// Inside the rail, on no chip.
    Empty,
}

/// Pixel box of the rail and the chip grid inside it. Chips are sized to
/// their labels (PT-123): `chip_px[i]` is chip `i`'s width on a horizontal
/// rail; a side rail is one column, every chip the rail's width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailLayout {
    pub side: RailSide,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    /// Widest chip in cells (`space_rail_chip_cols`, 0 → default cap).
    pub chip_cols: usize,
    /// Per-chip widths in pixels, in rail order (without the `+`).
    pub chip_px: Vec<usize>,
    pub overflow: bool,
    /// Gap between chips (the configured `pane_gap_px`).
    pub gap: usize,
    /// Leading pad before the first chip.
    pub pad: usize,
}

impl RailLayout {
    pub fn with_pane_names(mut self, enabled: bool) -> Self {
        if enabled && !self.side.horizontal() {
            self.cell_h = self.cell_h.saturating_mul(2);
        }
        self
    }

    /// The rail box for a window and `names` (rail order), or `None` when
    /// the rail is off or the window cannot hold it.
    pub fn for_window(
        geom: HostGeom,
        width: usize,
        height: usize,
        names: &[String],
    ) -> Option<Self> {
        let px = geom.rail_px;
        if geom.rail_side == RailSide::Off || px == 0 || width == 0 || height == 0 {
            return None;
        }
        let cell_w = geom.cell_w.max(1);
        let cell_h = geom.cell_h.max(1);
        let cap = chip_cap(geom.rail_chip_cols);
        let chip_px: Vec<usize> = names
            .iter()
            .map(|name| chip_cells_for(name_cells(name), cap).saturating_mul(cell_w))
            .collect();
        let common = |x, y, w, h, pad| Self {
            side: geom.rail_side,
            x,
            y,
            w,
            h,
            cell_w,
            cell_h,
            chip_cols: cap,
            chip_px: chip_px.clone(),
            overflow: false,
            gap: geom.rail_gap,
            pad,
        };
        let layout = match geom.rail_side {
            RailSide::Off => return None,
            RailSide::Bottom => {
                if px >= height {
                    return None;
                }
                common(
                    0,
                    height - px,
                    width,
                    px,
                    effective_tab_end_pad(geom.window_pad, width),
                )
            }
            RailSide::Top => {
                let y: usize = 0;
                if y.saturating_add(px) >= height {
                    return None;
                }
                common(
                    0,
                    y,
                    width,
                    px,
                    effective_tab_end_pad(geom.window_pad, width),
                )
            }
            RailSide::Left | RailSide::Right => {
                if px >= width {
                    return None;
                }
                let y = geom.top_chrome_px;
                let h = height.saturating_sub(y);
                let x = if geom.rail_side == RailSide::Left {
                    0
                } else {
                    width - px
                };
                common(x, y, px, h, geom.window_pad)
            }
        };
        Some(layout)
    }

    /// Width of the `+` chip: one cell of glyph plus the label inset on
    /// both sides. A side rail keeps it column-wide so the column stays
    /// left-aligned.
    pub fn plus_w(&self) -> usize {
        if self.side.horizontal() {
            self.cell_w
                .saturating_add(RAIL_LABEL_INSET.saturating_mul(2))
        } else {
            self.w
        }
    }

    /// Pixel box of chip `index` among `n` chips; `index == n` is the `+`
    /// chip. `None` when it would not fit inside the rail.
    pub fn chip_bounds(&self, index: usize, n: usize) -> Option<(usize, usize, usize, usize)> {
        if n > self.chip_px.len() {
            return None;
        }
        if self.overflow && index >= n && index <= n + 1 {
            if self.side.horizontal() {
                let w = self.plus_w();
                let right = self.x + self.w - self.pad;
                let offset = if index == n { 2 * w + self.gap } else { w };
                return (right >= self.x + offset).then_some((
                    right.saturating_sub(offset),
                    self.y,
                    w,
                    self.h,
                ));
            }
            let offset = if index == n {
                2 * self.cell_h + self.gap
            } else {
                self.cell_h
            };
            return (self.h >= offset).then_some((
                self.x,
                self.y + self.h.saturating_sub(offset),
                self.w,
                self.cell_h,
            ));
        }
        if index > n || (index < n && self.chip_px[index] == 0) {
            return None;
        }
        let plus = index == n;
        if self.side.horizontal() {
            let before: usize = self.chip_px[..index].iter().sum();
            let x = self
                .x
                .saturating_add(self.pad)
                .saturating_add(before)
                .saturating_add(
                    self.chip_px[..index]
                        .iter()
                        .filter(|w| **w > 0)
                        .count()
                        .saturating_mul(self.gap),
                );
            let w = if plus {
                self.plus_w()
            } else {
                self.chip_px[index]
            };
            let right = self.x.saturating_add(self.w).saturating_sub(self.pad);
            (x.saturating_add(w) <= right && w > 0).then_some((x, self.y, w, self.h))
        } else {
            let step = self.cell_h.saturating_add(self.gap);
            let y = self.y.saturating_add(self.pad).saturating_add(
                self.chip_px[..index]
                    .iter()
                    .filter(|w| **w > 0)
                    .count()
                    .saturating_mul(step),
            );
            let h = self.cell_h;
            let bottom = self.y.saturating_add(self.h);
            (y.saturating_add(h) <= bottom).then_some((self.x, y, self.w, h))
        }
    }

    /// Left edge of the close cell inside a chip, if the chip can hold it.
    pub fn close_left(&self, x0: usize, width: usize) -> Option<usize> {
        tab_close_left(x0, width, self.cell_w)
    }

    /// What sits under a window pixel, or `None` outside the rail.
    pub fn hit(&self, px: usize, py: usize, n: usize) -> Option<RailHit> {
        if px < self.x
            || py < self.y
            || px >= self.x.saturating_add(self.w)
            || py >= self.y.saturating_add(self.h)
        {
            return None;
        }
        for index in 0..=n + usize::from(self.overflow) {
            let Some((x0, y0, w, h)) = self.chip_bounds(index, n) else {
                continue;
            };
            if px >= x0 && px < x0.saturating_add(w) && py >= y0 && py < y0.saturating_add(h) {
                if index == n + 1 {
                    return Some(RailHit::Overflow);
                }
                if index == n {
                    return Some(RailHit::Plus);
                }
                let close = self.close_left(x0, w).is_some_and(|left| px >= left);
                return Some(RailHit::Chip { index, close });
            }
        }
        Some(RailHit::Empty)
    }
}

/// One keystroke inside the inline name editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditStroke {
    Insert(char),
    Backspace,
    /// A key that is neither text nor a command: drop the select-all state.
    DropSelection,
}

/// A key while the rail owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailKey {
    First,
    Last,
    Prev,
    Next,
    Enter,
    Escape,
    Delete,
    Rename,
    Menu,
    Edit(EditStroke),
}

/// What the host must do after a rail key or click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailVerdict {
    Consumed,
    /// Keyboard focus returns to the pane.
    Leave,
    Open(String),
    Rename {
        old: String,
        new: String,
    },
    Delete(String),
    Create(String),
    Menu {
        index: usize,
    },
    /// The edit stays open; show the message.
    Invalid(&'static str),
}

/// Inline name editor over a chip (`target = Some(i)`) or the `+` chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailEdit {
    pub target: Option<usize>,
    pub buffer: String,
    pub selected: bool,
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_NAME_CHARS: usize = 64;

/// Rail state: the chip list, the current space, and the keyboard mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRail {
    pub save_status: String,
    pub names: Vec<String>,
    pub live_pane_names: std::collections::HashMap<String, Vec<String>>,
    pub attention_counts: std::collections::HashMap<String, usize>,
    /// Space this host last opened (or was launched with).
    pub current: Option<String>,
    /// Keyboard focus, `0..=names.len()` (`len` is the `+` chip).
    pub focus: Option<usize>,
    pub edit: Option<RailEdit>,
    /// Chip awaiting delete confirmation.
    pub confirm: Option<usize>,
    /// Transient message painted in place of the confirm text.
    pub notice: Option<&'static str>,
    /// Focus arrived by keyboard (`space_rail_focus`); a mouse-started edit
    /// or confirm hands the keyboard back to the pane when it ends.
    pub keyboard: bool,
    last_poll: Option<Instant>,
    dir_stamp: Option<SystemTime>,
    /// Inputs of the last [`Self::infer_current`] scan: directory stamp and
    /// normalized live tabs. A repeat with the same inputs is a no-op, so
    /// an unknown space costs one scan per change, not one per pump tick.
    infer_tried: Option<(Option<SystemTime>, Vec<Vec<String>>)>,
}

impl SpaceRail {
    pub fn new(current: Option<String>) -> Self {
        Self {
            names: Vec::new(),
            live_pane_names: Default::default(),
            attention_counts: Default::default(),
            current: current.filter(|name| !name.is_empty()),
            focus: None,
            edit: None,
            confirm: None,
            save_status: String::new(),
            notice: None,
            keyboard: false,
            last_poll: None,
            dir_stamp: None,
            infer_tried: None,
        }
    }

    /// Use the same detail-aware widths for paint and hit testing.
    pub fn layout(
        &self,
        geom: HostGeom,
        width: usize,
        height: usize,
        details: bool,
    ) -> Option<RailLayout> {
        let labels: Vec<_> = self
            .names
            .iter()
            .map(|name| self.attention_label(name))
            .collect();
        let mut layout =
            RailLayout::for_window(geom, width, height, &labels)?.with_pane_names(details);
        if details && layout.side.horizontal() {
            for (index, name) in self.names.iter().enumerate() {
                if let Some(names) = self.live_pane_names.get(name) {
                    let detail_cells =
                        name_cells(&compact_names(names, layout.chip_cols.saturating_sub(2)));
                    let cells = name_cells(&labels[index]).max(detail_cells);
                    layout.chip_px[index] = chip_cells_for(cells, layout.chip_cols) * layout.cell_w;
                }
            }
        }
        let n = self.names.len();
        if layout.chip_bounds(n, n).is_none() {
            layout.overflow = true;
            let available = if layout.side.horizontal() {
                layout
                    .w
                    .saturating_sub(2 * layout.pad + 2 * layout.plus_w() + 2 * layout.gap)
            } else {
                layout
                    .h
                    .saturating_sub(layout.pad + 2 * layout.cell_h + 2 * layout.gap)
            };
            let start = self
                .focus
                .filter(|i| *i < n)
                .or_else(|| self.current_index())
                .unwrap_or(0);
            let mut used = 0;
            let mut end = start;
            while end < n {
                let size = if layout.side.horizontal() {
                    layout.chip_px[end].min(available)
                } else {
                    layout.cell_h
                };
                if used + size > available {
                    break;
                }
                layout.chip_px[end] = if layout.side.horizontal() {
                    size
                } else {
                    layout.chip_px[end]
                };
                used += size + layout.gap;
                end += 1;
            }
            for i in 0..n {
                if i < start || i >= end {
                    layout.chip_px[i] = 0;
                }
            }
        }
        Some(layout)
    }

    /// Reload the chip list from `dir`. Returns whether anything changed.
    pub fn refresh(&mut self, dir: &Path) -> bool {
        self.dir_stamp = dir_stamp(dir);
        self.last_poll = Some(Instant::now());
        self.infer_tried = None;
        let names: Vec<String> = prismattyc_mux::list_spaces(dir)
            .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
            .unwrap_or_default();
        if names == self.names {
            return false;
        }
        self.names = names;
        let n = self.names.len();
        if let Some(focus) = self.focus.as_mut() {
            *focus = (*focus).min(n);
        }
        if self.confirm.is_some_and(|index| index >= n) {
            self.confirm = None;
        }
        if self
            .edit
            .as_ref()
            .is_some_and(|edit| edit.target.is_some_and(|index| index >= n))
        {
            self.edit = None;
        }
        true
    }

    /// Once a second, re-read the directory when its mtime moved (a
    /// `pmux space save` / `rm` from a terminal). Returns whether the chip
    /// list changed.
    pub fn poll(&mut self, dir: &Path, now: Instant) -> bool {
        if self
            .last_poll
            .is_some_and(|last| now.duration_since(last) < REFRESH_INTERVAL)
        {
            return false;
        }
        self.last_poll = Some(now);
        let stamp = dir_stamp(dir);
        if stamp == self.dir_stamp && !self.names.is_empty() {
            return false;
        }
        self.refresh(dir)
    }

    pub fn set_current(&mut self, name: Option<String>) {
        self.current = name.filter(|name| !name.is_empty());
    }

    /// Longest saved name in cells; zero when there are no spaces.
    pub fn longest_name_cells(&self) -> usize {
        self.names
            .iter()
            .map(|name| name_cells(name))
            .max()
            .unwrap_or(0)
    }

    /// The host does not know its space (launched by `pmux attach --all`,
    /// or the cache was written by an older `pmux`). When exactly one saved
    /// space has the same tabs — the same session names grouped the same
    /// way, order-free — that space is current (PT-123). Returns whether
    /// `current` changed.
    pub fn infer_current(&mut self, dir: &Path, live_tabs: &[Vec<String>]) -> bool {
        if self.current_index().is_some() || live_tabs.is_empty() {
            return false;
        }
        let live = normalized_tabs(live_tabs.iter().cloned());
        let key = (self.dir_stamp, live.clone());
        if self.infer_tried.as_ref() == Some(&key) {
            return false;
        }
        self.infer_tried = Some(key);
        let mut matches = self.names.iter().filter(|name| {
            prismattyc_mux::load_space(dir, name).is_ok_and(|space| {
                !space.tabs.is_empty()
                    && normalized_tabs(space.tabs.iter().map(|tab| tab.sessions.clone())) == live
            })
        });
        let Some(found) = matches.next().cloned() else {
            return false;
        };
        if matches.next().is_some() {
            return false;
        }
        self.current = Some(found);
        true
    }

    pub fn current_index(&self) -> Option<usize> {
        let current = self.current.as_deref()?;
        self.names.iter().position(|name| name == current)
    }

    /// Whether the rail owns the keyboard.
    pub fn is_active(&self) -> bool {
        self.focus.is_some() || self.edit.is_some() || self.confirm.is_some()
    }

    /// `space_rail_focus`: move keyboard focus onto the current chip.
    pub fn focus_rail(&mut self) {
        self.focus = Some(self.current_index().unwrap_or(0));
        self.keyboard = true;
        self.notice = None;
    }

    /// Esc, or a click outside the rail.
    pub fn leave(&mut self) {
        self.focus = None;
        self.edit = None;
        self.confirm = None;
        self.notice = None;
        self.keyboard = false;
    }

    /// An edit or confirm finished. Keyboard-driven focus stays on the rail;
    /// a mouse-started interaction gives the keyboard back to the pane.
    pub fn settle(&mut self) {
        if !self.keyboard {
            self.leave();
        }
    }

    /// Move focus over the chips and the `+`, wrapping.
    pub fn step(&mut self, delta: i32) {
        let slots = self.names.len().saturating_add(1);
        let from = self
            .focus
            .unwrap_or_else(|| self.current_index().unwrap_or(0));
        let next = (from as i64 + i64::from(delta)).rem_euclid(slots as i64) as usize;
        self.focus = Some(next);
        self.confirm = None;
        self.notice = None;
    }

    /// `space_rail_next` / `space_rail_prev` from the pane: open the
    /// neighbour of the current space directly.
    pub fn neighbour(&self, delta: i32) -> Option<String> {
        if self.names.is_empty() {
            return None;
        }
        let n = self.names.len() as i64;
        let from = self
            .current_index()
            .map_or(if delta > 0 { -1 } else { 0 }, |i| i as i64);
        let next = (from + i64::from(delta)).rem_euclid(n) as usize;
        self.names.get(next).cloned()
    }

    pub fn begin_rename(&mut self, index: usize) -> bool {
        let Some(name) = self.names.get(index) else {
            return false;
        };
        self.edit = Some(RailEdit {
            target: Some(index),
            buffer: name.clone(),
            selected: true,
        });
        self.focus = Some(index);
        self.confirm = None;
        self.notice = None;
        true
    }

    pub fn begin_new(&mut self) {
        self.edit = Some(RailEdit {
            target: None,
            buffer: String::new(),
            selected: false,
        });
        self.focus = Some(self.names.len());
        self.confirm = None;
        self.notice = None;
    }

    /// Arm the delete confirm on a chip. The current space cannot be
    /// deleted from the rail (its chip shows a marker, not a `×`).
    pub fn begin_confirm(&mut self, index: usize) -> bool {
        if index >= self.names.len() || self.current_index() == Some(index) {
            return false;
        }
        self.confirm = Some(index);
        self.focus = Some(index);
        self.edit = None;
        self.notice = None;
        true
    }

    /// A key while the rail is active.
    pub fn key(&mut self, key: RailKey) -> RailVerdict {
        if let Some(edit) = self.edit.as_mut() {
            return match key {
                RailKey::Escape => {
                    self.edit = None;
                    self.notice = None;
                    self.settle();
                    RailVerdict::Consumed
                }
                RailKey::Enter => {
                    let verdict = self.commit_edit();
                    if self.edit.is_none() {
                        self.settle();
                    }
                    verdict
                }
                RailKey::Edit(stroke) => {
                    self.notice = None;
                    match stroke {
                        EditStroke::Insert(ch) => {
                            if edit.selected {
                                edit.buffer.clear();
                                edit.selected = false;
                            }
                            if !ch.is_control() && edit.buffer.chars().count() < MAX_NAME_CHARS {
                                edit.buffer.push(ch);
                            }
                        }
                        EditStroke::Backspace => {
                            if edit.selected {
                                edit.buffer.clear();
                                edit.selected = false;
                            } else {
                                edit.buffer.pop();
                            }
                        }
                        EditStroke::DropSelection => edit.selected = false,
                    }
                    RailVerdict::Consumed
                }
                _ => RailVerdict::Consumed,
            };
        }
        if let Some(index) = self.confirm {
            return match key {
                RailKey::Enter | RailKey::Delete => {
                    self.confirm = None;
                    let verdict = match self.names.get(index) {
                        Some(name) => RailVerdict::Delete(name.clone()),
                        None => RailVerdict::Consumed,
                    };
                    self.settle();
                    verdict
                }
                RailKey::Escape => {
                    self.confirm = None;
                    self.settle();
                    RailVerdict::Consumed
                }
                _ => RailVerdict::Consumed,
            };
        }
        let Some(focus) = self.focus else {
            return RailVerdict::Consumed;
        };
        match key {
            RailKey::Prev => {
                self.step(-1);
                RailVerdict::Consumed
            }
            RailKey::Next => {
                self.step(1);
                RailVerdict::Consumed
            }
            RailKey::First => {
                self.focus = Some(0);
                RailVerdict::Consumed
            }
            RailKey::Last => {
                self.focus = Some(self.names.len().saturating_sub(1));
                RailVerdict::Consumed
            }
            RailKey::Enter => {
                if focus >= self.names.len() {
                    self.begin_new();
                    RailVerdict::Consumed
                } else {
                    RailVerdict::Open(self.names[focus].clone())
                }
            }
            RailKey::Escape => {
                self.leave();
                RailVerdict::Leave
            }
            RailKey::Delete => {
                if !self.begin_confirm(focus) {
                    self.notice = Some(if focus < self.names.len() {
                        "current space: open another first"
                    } else {
                        ""
                    });
                }
                RailVerdict::Consumed
            }
            RailKey::Rename => {
                self.begin_rename(focus);
                RailVerdict::Consumed
            }
            RailKey::Menu => {
                if focus < self.names.len() {
                    RailVerdict::Menu { index: focus }
                } else {
                    RailVerdict::Consumed
                }
            }
            RailKey::Edit(_) => RailVerdict::Consumed,
        }
    }

    fn commit_edit(&mut self) -> RailVerdict {
        let Some(edit) = self.edit.as_ref() else {
            return RailVerdict::Consumed;
        };
        let name = edit.buffer.trim().to_string();
        if name.is_empty() {
            self.edit = None;
            self.notice = None;
            return RailVerdict::Consumed;
        }
        if prismattyc_mux::validate_layout_name(&name).is_err() {
            self.notice = Some("no '/' or '..' in a space name");
            return RailVerdict::Invalid("no '/' or '..' in a space name");
        }
        let exists = self.names.contains(&name);
        match edit.target {
            Some(index) => {
                let old = self.names.get(index).cloned().unwrap_or_default();
                if old == name {
                    self.edit = None;
                    self.notice = None;
                    return RailVerdict::Consumed;
                }
                if exists {
                    self.notice = Some("a space with that name exists");
                    return RailVerdict::Invalid("a space with that name exists");
                }
                self.edit = None;
                self.notice = None;
                RailVerdict::Rename { old, new: name }
            }
            None => {
                if exists {
                    self.notice = Some("a space with that name exists");
                    return RailVerdict::Invalid("a space with that name exists");
                }
                self.edit = None;
                self.notice = None;
                RailVerdict::Create(name)
            }
        }
    }
}

/// Tabs as a sorted list of sorted session-name sets; empty tabs dropped.
fn normalized_tabs<I: IntoIterator<Item = Vec<String>>>(tabs: I) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = tabs
        .into_iter()
        .map(|mut tab| {
            tab.sort();
            tab.dedup();
            tab
        })
        .filter(|tab| !tab.is_empty())
        .collect();
    out.sort();
    out
}

fn dir_stamp(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).ok()?.modified().ok()
}

/// What one chip paints as (built by the host from [`SpaceRail`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailChipView {
    pub label: String,
    pub pane_names: String,
    pub current: bool,
    pub focused: bool,
    /// Inline editor: `Some(select_all)`.
    pub editing: Option<bool>,
    pub confirm: bool,
    pub plus: bool,
}

impl SpaceRail {
    /// Chips in rail order, the `+` last. An open editor replaces the
    /// chip's label (or the `+` glyph) with the buffer.
    pub fn views(&self) -> Vec<RailChipView> {
        let current = self.current_index();
        let n = self.names.len();
        let mut views: Vec<RailChipView> = self
            .names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let editing = self.edit.as_ref().filter(|edit| edit.target == Some(index));
                let confirm = self.confirm == Some(index);
                let label = match (editing, confirm, self.notice) {
                    (Some(edit), _, _) => edit.buffer.clone(),
                    (None, true, _) => "delete? ⏎ · Esc".to_string(),
                    (None, false, Some(notice))
                        if !notice.is_empty() && self.focus == Some(index) =>
                    {
                        notice.to_string()
                    }
                    _ => self.attention_label(name),
                };
                RailChipView {
                    label,
                    pane_names: self
                        .live_pane_names
                        .get(name)
                        .map(|names| names.join(" · "))
                        .unwrap_or_default(),
                    current: current == Some(index),
                    focused: self.focus == Some(index),
                    editing: editing.map(|edit| edit.selected),
                    confirm,
                    plus: false,
                }
            })
            .collect();
        // The `+` stays a compact glyph; its name editor is a modal dialog
        // painted by the host (PT-123), so the chip only shows focus.
        let new_edit = self.edit.as_ref().filter(|edit| edit.target.is_none());
        views.push(RailChipView {
            label: "+".to_string(),
            pane_names: String::new(),
            current: false,
            focused: self.focus == Some(n) || new_edit.is_some(),
            editing: None,
            confirm: false,
            plus: true,
        });
        views
    }

    fn attention_label(&self, name: &str) -> String {
        match self.attention_counts.get(name).copied().unwrap_or(0) {
            0 if self.current.as_deref() == Some(name) && !self.save_status.is_empty() => {
                format!(
                    "{name} · {}",
                    if self.save_status == "Unsaved changes" {
                        "Unsaved"
                    } else {
                        &self.save_status
                    }
                )
            }
            0 => name.to_string(),
            1 => format!("{name} · 1 session needs you"),
            count => format!("{name} · {count} sessions need you"),
        }
    }
}

pub fn compact_names(names: &[String], max_cells: usize) -> String {
    if names.len() <= 2 && name_cells(&names.join(" · ")) <= max_cells {
        return names.join(" · ");
    }
    let Some(first) = names.first() else {
        return String::new();
    };
    let suffix = if names.len() > 1 {
        if max_cells < 12 {
            format!(" +{}", names.len() - 1)
        } else {
            format!(" · +{} more", names.len() - 1)
        }
    } else {
        String::new()
    };
    let limit = max_cells.saturating_sub(1 + name_cells(&suffix));
    let mut short = String::new();
    for ch in first.chars() {
        if name_cells(&short) + name_cells(&ch.to_string()) > limit {
            break;
        }
        short.push(ch);
    }
    if short != *first {
        short.push('…');
    }
    short + &suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(side: RailSide) -> HostGeom {
        let mut geom = HostGeom::tight(8, 16);
        geom.window_pad = 4;
        geom.rail_gap = 2;
        geom.rail_side = side;
        geom.rail_chip_cols = 10;
        geom.rail_px = rail_thickness_px(side, 8, 16, 10);
        geom
    }

    #[test]
    fn side_parses_config_spellings() {
        assert_eq!(RailSide::parse("bottom"), Some(RailSide::Bottom));
        assert_eq!(RailSide::parse(" Left "), Some(RailSide::Left));
        assert_eq!(RailSide::parse("TOP"), Some(RailSide::Top));
        assert_eq!(RailSide::parse("right"), Some(RailSide::Right));
        assert_eq!(RailSide::parse("off"), Some(RailSide::Off));
        assert_eq!(RailSide::parse("none"), Some(RailSide::Off));
        assert_eq!(RailSide::parse("middle"), None);
        for side in [
            RailSide::Bottom,
            RailSide::Left,
            RailSide::Top,
            RailSide::Right,
            RailSide::Off,
        ] {
            assert_eq!(RailSide::parse(side.as_str()), Some(side));
        }
    }

    #[test]
    fn thickness_is_one_row_or_chip_cols_wide() {
        assert_eq!(rail_thickness_px(RailSide::Bottom, 8, 16, 10), 16);
        assert_eq!(rail_thickness_px(RailSide::Top, 8, 16, 10), 16);
        assert_eq!(rail_thickness_px(RailSide::Left, 8, 16, 10), 80);
        assert_eq!(rail_thickness_px(RailSide::Right, 8, 16, 10), 80);
        assert_eq!(rail_thickness_px(RailSide::Off, 8, 16, 10), 0);
    }

    #[test]
    fn chip_cells_follow_the_label_within_min_and_cap() {
        assert_eq!(chip_cells_for(0, 28), MIN_CHIP_COLS);
        assert_eq!(chip_cells_for(3, 28), 6, "cur + close + insets");
        assert_eq!(chip_cells_for(4, 28), 7, "beta");
        assert_eq!(chip_cells_for(5, 28), 8, "alpha");
        assert_eq!(chip_cells_for(14, 28), 17, "prismattyc-web paints whole");
        assert_eq!(
            chip_cells_for(15, 28),
            18,
            "prismattyc-work differs from -web"
        );
        assert_eq!(chip_cells_for(40, 28), 28, "cap");
        assert_eq!(
            chip_cells_for(40, 10),
            10,
            "explicit space_rail_chip_cols caps"
        );
        assert_eq!(
            chip_cells_for(40, 2),
            MIN_CHIP_COLS,
            "cap never below the minimum"
        );
        assert_eq!(chip_cap(0), DEFAULT_CHIP_CAP);
        assert_eq!(chip_cap(20), 20);
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn bottom_rail_sizes_each_chip_to_its_label_from_the_left() {
        let list = names(&["alpha", "beta", "gamma-long-space-name"]);
        let layout = RailLayout::for_window(geom(RailSide::Bottom), 400, 300, &list).unwrap();
        assert_eq!((layout.x, layout.y, layout.w, layout.h), (0, 284, 400, 16));
        // cap 10 cells × 8 px: alpha 8 cells, beta 7, the long name capped.
        assert_eq!(layout.chip_px, vec![64, 56, 80]);
        assert_eq!(layout.chip_bounds(0, 3), Some((4, 284, 64, 16)));
        assert_eq!(layout.chip_bounds(1, 3), Some((70, 284, 56, 16)));
        assert_eq!(layout.chip_bounds(2, 3), Some((128, 284, 80, 16)));
        // The `+` is narrow and follows the last chip.
        assert_eq!(layout.chip_bounds(3, 3), Some((210, 284, 16, 16)));
        assert_eq!(layout.chip_bounds(4, 3), None);
        // A chip past the window edge is not laid out.
        let many = names(&["a"; 10]);
        let layout = RailLayout::for_window(geom(RailSide::Bottom), 200, 300, &many).unwrap();
        // "a" chips are the 6-cell minimum (48 px) + 2 px gap from x = 4.
        assert_eq!(layout.chip_bounds(2, 10), Some((104, 284, 48, 16)));
        assert_eq!(
            layout.chip_bounds(3, 10),
            None,
            "4th chip would cross the pad"
        );
    }

    #[test]
    fn top_rail_sits_above_the_tab_strip() {
        let mut g = geom(RailSide::Top);
        g.top_chrome_px = 32;
        let layout = RailLayout::for_window(g, 400, 300, &names(&["a"])).unwrap();
        assert_eq!((layout.x, layout.y, layout.w, layout.h), (0, 0, 400, 16));
    }

    #[test]
    fn side_rails_stack_chips_top_down_at_column_width() {
        let list = names(&["alpha", "beta"]);
        let left = RailLayout::for_window(geom(RailSide::Left), 400, 300, &list).unwrap();
        assert_eq!((left.x, left.y, left.w, left.h), (0, 0, 80, 300));
        assert_eq!(left.chip_bounds(0, 2), Some((0, 4, 80, 16)));
        assert_eq!(left.chip_bounds(1, 2), Some((0, 22, 80, 16)));
        assert_eq!(
            left.chip_bounds(2, 2),
            Some((0, 40, 80, 16)),
            "+ is column-wide"
        );
        let right = RailLayout::for_window(geom(RailSide::Right), 400, 300, &list).unwrap();
        assert_eq!((right.x, right.y, right.w, right.h), (320, 0, 80, 300));
        assert_eq!(right.chip_bounds(0, 2), Some((320, 4, 80, 16)));
        assert_eq!(right.chip_bounds(0, 3), None, "n beyond the names");
    }

    #[test]
    fn off_or_too_small_reserves_nothing() {
        let list = names(&["a"]);
        assert!(RailLayout::for_window(geom(RailSide::Off), 400, 300, &list).is_none());
        assert!(RailLayout::for_window(geom(RailSide::Bottom), 400, 16, &list).is_none());
        assert!(RailLayout::for_window(geom(RailSide::Left), 80, 300, &list).is_none());
    }

    #[test]
    fn hit_test_returns_chip_close_plus_and_empty() {
        let list = names(&["alpha", "beta"]);
        let layout = RailLayout::for_window(geom(RailSide::Bottom), 400, 300, &list).unwrap();
        assert_eq!(layout.hit(10, 100, 2), None, "above the rail");
        assert_eq!(
            layout.hit(10, 290, 2),
            Some(RailHit::Chip {
                index: 0,
                close: false
            })
        );
        // close cell: alpha spans 4..68, close_left = 68 - (8 + 8) = 52.
        assert_eq!(
            layout.hit(54, 290, 2),
            Some(RailHit::Chip {
                index: 0,
                close: true
            })
        );
        assert_eq!(
            layout.hit(74, 290, 2),
            Some(RailHit::Chip {
                index: 1,
                close: false
            })
        );
        // beta spans 70..126, gap, then the + at 128..144.
        assert_eq!(layout.hit(131, 290, 2), Some(RailHit::Plus));
        assert_eq!(layout.hit(300, 290, 2), Some(RailHit::Empty));
        assert_eq!(
            layout.hit(69, 290, 2),
            Some(RailHit::Empty),
            "the gap is empty"
        );
    }

    fn rail(names: &[&str], current: Option<&str>) -> SpaceRail {
        let mut rail = SpaceRail::new(current.map(str::to_string));
        rail.names = names.iter().map(|name| (*name).to_string()).collect();
        rail
    }

    #[test]
    fn focus_moves_over_chips_and_plus_and_opens_on_enter() {
        let mut rail = rail(&["alpha", "beta"], Some("beta"));
        rail.focus_rail();
        assert_eq!(rail.focus, Some(1));
        assert_eq!(rail.key(RailKey::Next), RailVerdict::Consumed);
        assert_eq!(rail.focus, Some(2), "the + chip");
        assert_eq!(rail.key(RailKey::Next), RailVerdict::Consumed);
        assert_eq!(rail.focus, Some(0), "wraps");
        assert_eq!(rail.key(RailKey::Prev), RailVerdict::Consumed);
        assert_eq!(rail.focus, Some(2));
        rail.step(-1);
        assert_eq!(
            rail.key(RailKey::Enter),
            RailVerdict::Open("beta".to_string())
        );
        assert_eq!(rail.key(RailKey::Escape), RailVerdict::Leave);
        assert!(!rail.is_active());
    }

    #[test]
    fn neighbour_wraps_around_the_current_space() {
        let rail = rail(&["a", "b", "c"], Some("c"));
        assert_eq!(rail.neighbour(1).as_deref(), Some("a"));
        assert_eq!(rail.neighbour(-1).as_deref(), Some("b"));
        let none = rail_none();
        assert_eq!(none.neighbour(1).as_deref(), Some("a"));
        assert_eq!(none.neighbour(-1).as_deref(), Some("c"));
        assert!(SpaceRail::new(None).neighbour(1).is_none());
    }

    fn rail_none() -> SpaceRail {
        rail(&["a", "b", "c"], None)
    }

    #[test]
    fn rename_flow_edits_in_place_and_refuses_collisions() {
        let mut rail = rail(&["alpha", "beta"], None);
        assert!(rail.begin_rename(0));
        let edit = rail.edit.as_ref().unwrap();
        assert_eq!(edit.buffer, "alpha");
        assert!(edit.selected);
        // Typing replaces the selected title.
        rail.key(RailKey::Edit(EditStroke::Insert('b')));
        rail.key(RailKey::Edit(EditStroke::Insert('e')));
        rail.key(RailKey::Edit(EditStroke::Insert('t')));
        rail.key(RailKey::Edit(EditStroke::Insert('a')));
        assert_eq!(
            rail.key(RailKey::Enter),
            RailVerdict::Invalid("a space with that name exists")
        );
        assert!(rail.edit.is_some(), "the editor stays open");
        rail.key(RailKey::Edit(EditStroke::Backspace));
        rail.key(RailKey::Edit(EditStroke::Insert('/')));
        assert!(matches!(rail.key(RailKey::Enter), RailVerdict::Invalid(_)));
        rail.key(RailKey::Edit(EditStroke::Backspace));
        rail.key(RailKey::Edit(EditStroke::Insert('2')));
        assert_eq!(
            rail.key(RailKey::Enter),
            RailVerdict::Rename {
                old: "alpha".into(),
                new: "bet2".into()
            }
        );
        assert!(rail.edit.is_none());
        // Same name commits nothing.
        rail.begin_rename(1);
        assert_eq!(rail.key(RailKey::Enter), RailVerdict::Consumed);
        // Esc cancels.
        rail.begin_rename(1);
        rail.key(RailKey::Edit(EditStroke::Insert('x')));
        assert_eq!(rail.key(RailKey::Escape), RailVerdict::Consumed);
        assert!(rail.edit.is_none());
        assert_eq!(rail.names[1], "beta");
    }

    #[test]
    fn plus_opens_a_name_editor_that_saves_new() {
        let mut rail = rail(&["alpha"], Some("alpha"));
        rail.focus_rail();
        rail.step(1);
        assert_eq!(rail.key(RailKey::Enter), RailVerdict::Consumed);
        assert_eq!(rail.edit.as_ref().map(|edit| edit.target), Some(None));
        for ch in "alpha".chars() {
            rail.key(RailKey::Edit(EditStroke::Insert(ch)));
        }
        assert!(matches!(rail.key(RailKey::Enter), RailVerdict::Invalid(_)));
        rail.key(RailKey::Edit(EditStroke::Insert('2')));
        assert_eq!(
            rail.key(RailKey::Enter),
            RailVerdict::Create("alpha2".into())
        );
        // An empty name just closes the editor.
        rail.begin_new();
        assert_eq!(rail.key(RailKey::Enter), RailVerdict::Consumed);
        assert!(rail.edit.is_none());
    }

    #[test]
    fn delete_needs_a_confirm_and_spares_the_current_space() {
        let mut rail = rail(&["alpha", "beta"], Some("alpha"));
        rail.focus_rail();
        assert_eq!(rail.key(RailKey::Delete), RailVerdict::Consumed);
        assert!(rail.confirm.is_none(), "current space is not deletable");
        assert!(rail.notice.is_some());
        rail.step(1);
        assert!(rail.notice.is_none(), "moving focus clears the notice");
        assert_eq!(rail.key(RailKey::Delete), RailVerdict::Consumed);
        assert_eq!(rail.confirm, Some(1));
        assert_eq!(rail.views()[1].label, "delete? ⏎ · Esc");
        assert_eq!(rail.key(RailKey::Escape), RailVerdict::Consumed);
        assert!(rail.confirm.is_none());
        assert!(rail.begin_confirm(1));
        assert_eq!(rail.key(RailKey::Enter), RailVerdict::Delete("beta".into()));
        assert!(!rail.begin_confirm(0));
        assert!(!rail.begin_confirm(9));
    }

    #[test]
    fn mouse_started_edits_hand_the_keyboard_back_but_keyboard_focus_stays() {
        // Mouse: right-click rename, commit → rail inactive again.
        let mut rail = rail(&["alpha", "beta"], None);
        assert!(rail.begin_rename(0));
        assert!(rail.is_active());
        rail.key(RailKey::Edit(EditStroke::Insert('z')));
        assert!(matches!(
            rail.key(RailKey::Enter),
            RailVerdict::Rename { .. }
        ));
        assert!(!rail.is_active(), "mouse rename must not trap the keyboard");
        // Mouse: confirm then Esc → inactive.
        assert!(rail.begin_confirm(1));
        assert_eq!(rail.key(RailKey::Escape), RailVerdict::Consumed);
        assert!(!rail.is_active());
        // Keyboard: focus_rail, F2, commit → focus stays on the rail.
        rail.focus_rail();
        assert_eq!(rail.key(RailKey::Rename), RailVerdict::Consumed);
        rail.key(RailKey::Edit(EditStroke::Insert('q')));
        assert!(matches!(
            rail.key(RailKey::Enter),
            RailVerdict::Rename { .. }
        ));
        assert!(rail.is_active(), "keyboard focus survives a commit");
        assert_eq!(rail.key(RailKey::Escape), RailVerdict::Leave);
        assert!(!rail.is_active());
    }

    #[test]
    fn longest_name_cells_counts_display_cells() {
        let rail = rail(&["a", "prismattyc-work", "bb"], None);
        assert_eq!(rail.longest_name_cells(), 15);
        assert_eq!(SpaceRail::new(None).longest_name_cells(), 0);
    }

    #[test]
    fn infer_current_needs_exactly_one_matching_space() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-rail-infer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let window = |name: &str| prismattyc_mux::SavedWindow {
            title: name.into(),
            cols: 80,
            rows: 24,
            root: prismattyc_mux::SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            },
        };
        let session = |name: &str| prismattyc_mux::SavedSpaceSession {
            name: name.into(),
            agent: None,
            windows: vec![window(name)],
        };
        let space = |tabs: &[(&str, &[&str])]| prismattyc_mux::SavedSpace {
            id: None,
            version: prismattyc_mux::SAVED_SPACE_VERSION,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: tabs
                .iter()
                .flat_map(|(_, members)| members.iter().map(|m| session(m)))
                .collect(),
            tabs: tabs
                .iter()
                .map(|(title, members)| prismattyc_mux::SavedSpaceTab {
                    title: (*title).into(),
                    sessions: members.iter().map(|m| (*m).to_string()).collect(),
                })
                .collect(),
            active_tab: 0,
            focused_session: None,
        };
        prismattyc_mux::save_space(&dir, "work", &space(&[("A", &["x", "y"]), ("B", &["z"])]))
            .unwrap();
        prismattyc_mux::save_space(&dir, "solo", &space(&[("A", &["x"])])).unwrap();
        prismattyc_mux::save_space(&dir, "solo2", &space(&[("A", &["x"])])).unwrap();
        let mut rail = SpaceRail::new(None);
        rail.refresh(&dir);
        // Same tabs, different order inside and across tabs → work.
        let live = vec![
            vec!["z".to_string()],
            vec!["y".to_string(), "x".to_string()],
        ];
        assert!(rail.infer_current(&dir, &live));
        assert_eq!(rail.current.as_deref(), Some("work"));
        // Already current: no change.
        assert!(!rail.infer_current(&dir, &live));
        // Two identical candidates: ambiguous, stays unknown.
        let mut rail = SpaceRail::new(None);
        rail.refresh(&dir);
        assert!(!rail.infer_current(&dir, &[vec!["x".to_string()]]));
        assert!(rail.current.is_none());
        // No match / no live attach tabs.
        assert!(!rail.infer_current(&dir, &[vec!["q".to_string()]]));
        assert!(!rail.infer_current(&dir, &[]));
        // Same inputs again: gated, no rescan even if a match appears now
        // (the directory stamp is unchanged from the rail's view)...
        let live = vec![
            vec!["x".to_string(), "y".to_string()],
            vec!["z".to_string()],
        ];
        let mut rail = SpaceRail::new(None);
        rail.refresh(&dir);
        prismattyc_mux::remove_space(&dir, "work").unwrap();
        assert!(!rail.infer_current(&dir, &live));
        prismattyc_mux::save_space(&dir, "work", &space(&[("A", &["x", "y"]), ("B", &["z"])]))
            .unwrap();
        assert!(
            !rail.infer_current(&dir, &live),
            "same stamp + same live tabs: no rescan"
        );
        // ...until a refresh (names / directory change) resets the gate.
        rail.refresh(&dir);
        assert!(rail.infer_current(&dir, &live));
        assert_eq!(rail.current.as_deref(), Some("work"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn views_mark_current_focused_editing_and_plus() {
        let mut rail = rail(&["alpha", "beta"], Some("beta"));
        rail.begin_rename(0);
        let views = rail.views();
        assert_eq!(views.len(), 3);
        assert_eq!(views[0].editing, Some(true));
        assert!(views[0].focused);
        assert!(views[1].current);
        assert!(views[2].plus);
        assert_eq!(views[2].label, "+");
        rail.leave();
        rail.begin_new();
        rail.key(RailKey::Edit(EditStroke::Insert('n')));
        assert_eq!(
            rail.edit.as_ref().map(|edit| edit.buffer.as_str()),
            Some("n"),
            "the modal buffer stores the glyph; chrome does not"
        );
        let views = rail.views();
        assert_eq!(
            views[2].label, "+",
            "the + stays compact; the editor is modal"
        );
        assert!(views[2].focused);
        assert_eq!(views[2].editing, None);
    }

    #[test]
    fn refresh_reads_the_directory_and_clamps_state() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-rail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut rail = SpaceRail::new(Some("two".into()));
        assert!(!rail.refresh(&dir), "missing dir: still empty");
        let mut space = prismattyc_mux::SavedSpace {
            id: None,
            version: prismattyc_mux::SAVED_SPACE_VERSION,
            created_at_unix_ms: Some(1),
            saved_at_unix: 1,
            sessions: vec![prismattyc_mux::SavedSpaceSession {
                name: "a".into(),
                agent: None,
                windows: vec![prismattyc_mux::SavedWindow {
                    title: "a".into(),
                    cols: 80,
                    rows: 24,
                    root: prismattyc_mux::SavedNode::Leaf {
                        cwd: None,
                        program: None,
                        command: None,
                        title: None,
                    },
                }],
            }],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        prismattyc_mux::save_space(&dir, "two", &space).unwrap();
        space.created_at_unix_ms = Some(2);
        prismattyc_mux::save_space(&dir, "one", &space).unwrap();
        assert!(rail.refresh(&dir));
        assert_eq!(rail.names, vec!["two".to_string(), "one".to_string()]);
        assert_eq!(rail.current_index(), Some(0));
        rail.focus = Some(2);
        rail.confirm = Some(1);
        prismattyc_mux::remove_space(&dir, "two").unwrap();
        assert!(rail.refresh(&dir));
        assert_eq!(rail.focus, Some(1), "focus clamps to the + chip");
        assert!(rail.confirm.is_none());
        assert_eq!(rail.current_index(), None);
        // poll is throttled: an immediate second poll does nothing.
        assert!(!rail.poll(&dir, Instant::now()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod polish_tests {
    use super::*;
    #[test]
    fn crowded_rails_keep_focused_space_plus_and_overflow_clickable_on_every_edge() {
        for side in [
            RailSide::Bottom,
            RailSide::Top,
            RailSide::Left,
            RailSide::Right,
        ] {
            let mut rail = SpaceRail::new(Some("space-17".into()));
            rail.names = (0..20).map(|i| format!("space-{i}")).collect();
            rail.focus = Some(17);
            let mut geom = HostGeom::tight(8, 16);
            geom.rail_side = side;
            geom.rail_px = if side.horizontal() { 32 } else { 160 };
            let layout = rail.layout(geom, 400, 300, true).unwrap();
            assert!(layout.overflow, "{side:?}");
            for (index, expected) in [
                (
                    17,
                    RailHit::Chip {
                        index: 17,
                        close: false,
                    },
                ),
                (20, RailHit::Plus),
                (21, RailHit::Overflow),
            ] {
                let (x, y, w, h) = layout.chip_bounds(index, 20).unwrap();
                assert!(x + w <= 400 && y + h <= 300);
                assert_eq!(layout.hit(x + 1, y + 1, 20), Some(expected), "{side:?}");
            }
        }
    }
    #[test]
    fn crowded_session_labels_keep_the_hidden_count() {
        let names = vec![
            "a-very-long-session-name-with-many-characters".into(),
            "second".into(),
            "third".into(),
            "fourth".into(),
        ];
        let label = compact_names(&names, DEFAULT_CHIP_CAP - 2);
        assert!(label.ends_with("+3 more"));
        assert!(name_cells(&label) <= DEFAULT_CHIP_CAP - 2);
    }
}
