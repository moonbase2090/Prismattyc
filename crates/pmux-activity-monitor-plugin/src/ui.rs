// SPDX-License-Identifier: MIT

//! Layout and drawing. Everything here is a pure function of the model, the UI state,
//! the pane size and the clock, so every tier can be rendered and checked in tests.

use lunatui_core::{Attrs, Color, Rect, Style};
use lunatui_term::{Event, KeyCode, KeyEvent, KeyKind};
use lunatui_theme::{Theme, Token, Variant};
use lunatui_widgets::{RenderCx, Sparkline, SparklineBar, Widget};
use prismattyc_core::activity::{ActivityKind, ActivityRecord};
use prismattyc_core::AgentId;

use crate::model::{self, Host, Link, Seat, SeatState};

/// How long a state-change fade lasts.
pub const FADE_MS: u64 = 900;
const LIST_LANE_WINDOW_MS: u64 = 5 * 60_000;
const DETAIL_LANE_WINDOW_MS: u64 = 10 * 60_000;
const KIND_WIDTH: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub dark: bool,
    /// The pane background this palette assumes; used only to blend fades.
    pub ground: [u8; 3],
    pub text: Color,
    pub strong: Color,
    pub muted: Color,
    pub hairline: Color,
    pub selection: Color,
    pub raised: Color,
    pub focus: Color,
    pub working: Color,
    pub waiting: Color,
    pub failed: Color,
    pub stopped: Color,
}

const fn rgb(value: u32) -> Color {
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

impl Palette {
    /// Graphite dark tokens (docs/design/graphite-ui-brief.md).
    pub const DARK: Self = Self {
        dark: true,
        ground: [0x18, 0x1b, 0x21],
        text: rgb(0xd9dde4),
        strong: rgb(0xe6e9ee),
        muted: rgb(0xa3abb8),
        hairline: rgb(0x3a404b),
        selection: rgb(0x1c2330),
        raised: rgb(0x252a33),
        focus: rgb(0x5aa2ff),
        working: rgb(0x4cc98a),
        waiting: rgb(0xf2b84b),
        failed: rgb(0xff7a6b),
        stopped: rgb(0x7d8594),
    };

    /// Graphite light tokens.
    pub const LIGHT: Self = Self {
        dark: false,
        ground: [0xff, 0xff, 0xff],
        text: rgb(0x1f2329),
        strong: rgb(0x1f2329),
        muted: rgb(0x5b6472),
        hairline: rgb(0xc4cad2),
        selection: rgb(0xeaf1fc),
        raised: rgb(0xeef0f3),
        focus: rgb(0x2f6fd0),
        working: rgb(0x1f8a55),
        waiting: rgb(0xb87a0a),
        failed: rgb(0xc2392b),
        stopped: rgb(0x8a929e),
    };

    pub fn state(&self, state: SeatState) -> Color {
        match state {
            SeatState::Unknown => self.muted,
            SeatState::Working => self.working,
            SeatState::Waiting => self.waiting,
            SeatState::Idle => self.muted,
            SeatState::Stuck => self.failed,
            SeatState::Failed => self.failed,
            SeatState::Stopped => self.stopped,
        }
    }

    /// A Lunatui theme carrying the same colors, for widgets that read tokens.
    pub fn theme(&self) -> Theme {
        let variant = if self.dark {
            Variant::Dark
        } else {
            Variant::Light
        };
        let [r, g, b] = self.ground;
        Theme::with_overrides(
            variant,
            &[
                (Token::Surface0, Color::Rgb(r, g, b)),
                (Token::Surface1, self.raised),
                (Token::TextPrimary, self.text),
                (Token::Accent, self.selection),
                (Token::Ok, self.working),
                (Token::Warn, self.waiting),
                (Token::Fail, self.failed),
                (Token::FocusRing, self.focus),
            ],
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// One line of counts.
    Tiny,
    /// Glyph, seat and age.
    Small,
    /// Adds the event kind.
    Narrow,
    /// Adds the stale flag and the lane.
    Medium,
    /// Adds the timeline beside the list.
    Wide,
}

pub fn tier(width: u16, height: u16) -> Tier {
    if width < 24 || height < 4 {
        Tier::Tiny
    } else if width < 40 {
        Tier::Small
    } else if width < 72 {
        Tier::Narrow
    } else if width < 110 || height < 14 {
        Tier::Medium
    } else {
        Tier::Wide
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Attention,
    Working,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Attention,
            Self::Attention => Self::Working,
            Self::Working => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all seats",
            Self::Attention => "needs attention",
            Self::Working => "working",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Name,
    Recent,
    State,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Self::Name => Self::Recent,
            Self::Recent => Self::State,
            Self::State => Self::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Name => "by name",
            Self::Recent => "most recent",
            Self::State => "by state",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    List,
    Timeline,
    Details,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Host(usize),
    Seat(usize, AgentId),
    /// An explanation under a host with nothing to list.
    Note(usize),
}

pub struct Ui {
    pub screen: Screen,
    pub selected: Option<(usize, AgentId)>,
    pub filter: Filter,
    pub sort: Sort,
    pub help: bool,
    pub pixels: bool,
    pub motion: bool,
    /// Run and event hashes are hidden unless the operator opts into diagnostics.
    pub hashes: bool,
    list_offset: usize,
    timeline_offset: usize,
    /// Rows per page from the last frame, for PgUp/PgDn.
    page: usize,
    /// Tier of the last frame.
    last_tier: Tier,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            screen: Screen::List,
            selected: None,
            filter: Filter::All,
            sort: Sort::Name,
            help: false,
            pixels: true,
            motion: true,
            hashes: false,
            list_offset: 0,
            timeline_offset: 0,
            page: 10,
            last_tier: Tier::Medium,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// Wall clock, comparable with daemon receive times.
    pub wall_ms: u64,
    /// Monotonic viewer time, for motion.
    pub mono_ms: u64,
}

fn visible(host: &Host, seat: &Seat, filter: Filter, now: u64) -> bool {
    match filter {
        Filter::All => true,
        Filter::Working => seat.state == SeatState::Working,
        Filter::Attention => {
            matches!(
                seat.state,
                SeatState::Waiting | SeatState::Stuck | SeatState::Failed
            ) || (seat.state == SeatState::Working && model::is_stale(host, seat, now))
        }
    }
}

/// Rows in display order: each host, then its visible seats.
pub fn rows(hosts: &[Host], ui: &Ui, now: u64) -> Vec<Row> {
    let mut out = Vec::new();
    for (index, host) in hosts.iter().enumerate() {
        out.push(Row::Host(index));
        let mut seats: Vec<(&AgentId, &Seat)> = host
            .seats
            .iter()
            .filter(|(_, seat)| visible(host, seat, ui.filter, now))
            .collect();
        match ui.sort {
            Sort::Name => {}
            Sort::Recent => {
                seats.sort_by_key(|(_, seat)| std::cmp::Reverse(seat.latest().sequence))
            }
            Sort::State => seats.sort_by_key(|(_, seat)| seat.state.severity()),
        }
        if seats.is_empty() {
            out.push(Row::Note(index));
        }
        out.extend(
            seats
                .into_iter()
                .map(|(id, _)| Row::Seat(index, id.clone())),
        );
    }
    out
}

fn seat_rows(rows: &[Row]) -> Vec<(usize, AgentId)> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Seat(host, id) => Some((*host, id.clone())),
            _ => None,
        })
        .collect()
}

impl Ui {
    /// The selection, repaired to a visible seat when the selected one is gone.
    pub fn current(&mut self, rows: &[Row]) -> Option<(usize, AgentId)> {
        let seats = seat_rows(rows);
        let keep = self
            .selected
            .as_ref()
            .is_some_and(|selected| seats.contains(selected));
        if !keep {
            let fallback = self.selected.as_ref().and_then(|(host, _)| {
                seats
                    .iter()
                    .find(|(seat_host, _)| seat_host == host)
                    .cloned()
            });
            self.selected = fallback.or_else(|| seats.first().cloned());
        }
        self.selected.clone()
    }

    fn step(&mut self, rows: &[Row], delta: isize) {
        let seats = seat_rows(rows);
        if seats.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| seats.iter().position(|seat| seat == selected))
            .unwrap_or(0);
        let last = seats.len() as isize - 1;
        let next = (current as isize + delta).clamp(0, last) as usize;
        self.selected = Some(seats[next].clone());
    }

    fn jump_host(&mut self, rows: &[Row], forward: bool, hosts: usize) {
        let seats = seat_rows(rows);
        let Some(&(current, _)) = self.selected.as_ref().map(|(host, id)| (host, id)).as_ref()
        else {
            self.selected = seats.first().cloned();
            return;
        };
        for offset in 1..=hosts {
            let host = if forward {
                (current + offset) % hosts
            } else {
                (current + hosts * 2 - offset) % hosts
            };
            if let Some(seat) = seats.iter().find(|(seat_host, _)| *seat_host == host) {
                self.selected = Some(seat.clone());
                return;
            }
        }
    }

    /// Applies one key. Returns true to quit.
    pub fn key(&mut self, key: &KeyEvent, hosts: &[Host], now: u64) -> bool {
        if key.kind == KeyKind::Release {
            return false;
        }
        let ctrl = key.modifiers.is_control();
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return true;
        }
        if self.help {
            // Any key closes the help, and q still quits.
            self.help = false;
            return key.code == KeyCode::Char('q');
        }
        let rows = rows(hosts, self, now);
        self.current(&rows);
        let page = self.page.max(1) as isize;
        match (self.screen, key.code) {
            (_, KeyCode::Char('q')) => return true,
            (_, KeyCode::Char('?')) => self.help = true,
            (_, KeyCode::Char('p')) => self.pixels = !self.pixels,
            (_, KeyCode::Char('i')) => self.hashes = !self.hashes,
            (Screen::List, KeyCode::Up | KeyCode::Char('k')) => self.step(&rows, -1),
            (Screen::List, KeyCode::Down | KeyCode::Char('j')) => self.step(&rows, 1),
            (Screen::List, KeyCode::PageUp) => self.step(&rows, -page),
            (Screen::List, KeyCode::PageDown) => self.step(&rows, page),
            (Screen::List, KeyCode::Home | KeyCode::Char('g')) => self.step(&rows, isize::MIN / 2),
            (Screen::List, KeyCode::End | KeyCode::Char('G')) => self.step(&rows, isize::MAX / 2),
            (Screen::List, KeyCode::Tab) if key.modifiers.is_shift() => {
                self.jump_host(&rows, false, hosts.len());
            }
            (Screen::List, KeyCode::Tab) => self.jump_host(&rows, true, hosts.len()),
            (Screen::List, KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')) => {
                if self.selected.is_some() {
                    self.screen = Screen::Timeline;
                    self.timeline_offset = 0;
                }
            }
            (Screen::List, KeyCode::Char('d')) => {
                if self.selected.is_some() {
                    self.screen = Screen::Details;
                    self.timeline_offset = 0;
                }
            }
            (Screen::List, KeyCode::Char('f')) => self.filter = self.filter.next(),
            (Screen::List, KeyCode::Char('s')) => self.sort = self.sort.next(),
            (
                Screen::Timeline | Screen::Details,
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace,
            ) => {
                self.screen = Screen::List;
            }
            (Screen::Timeline | Screen::Details, KeyCode::Up | KeyCode::Char('k')) => {
                self.timeline_offset = self.timeline_offset.saturating_sub(1);
            }
            (Screen::Timeline | Screen::Details, KeyCode::Down | KeyCode::Char('j')) => {
                self.timeline_offset += 1
            }
            (Screen::Timeline | Screen::Details, KeyCode::PageUp) => {
                self.timeline_offset = self.timeline_offset.saturating_sub(page as usize);
            }
            (Screen::Timeline | Screen::Details, KeyCode::PageDown) => {
                self.timeline_offset += page as usize
            }
            (Screen::Timeline | Screen::Details, KeyCode::Home | KeyCode::Char('g')) => {
                self.timeline_offset = 0
            }
            (Screen::Timeline | Screen::Details, KeyCode::End | KeyCode::Char('G')) => {
                self.timeline_offset = usize::MAX / 2
            }
            _ => {}
        }
        false
    }

    pub fn event(&mut self, event: &Event, hosts: &[Host], now: u64) -> bool {
        match event {
            Event::Key(key) => self.key(key, hosts, now),
            _ => false,
        }
    }
}

/// True while any visible fade is still running.
pub fn animating(hosts: &[Host], ui: &Ui, mono_ms: u64) -> bool {
    ui.motion
        && hosts.iter().any(|host| {
            host.seats.values().any(|seat| {
                seat.changed_at
                    .is_some_and(|at| mono_ms.saturating_sub(at) < FADE_MS)
            })
        })
}

// ---------------------------------------------------------------------------
// Drawing helpers

struct Painter<'a, 'b> {
    cx: &'a mut RenderCx<'b>,
    palette: &'a Palette,
    pixels: bool,
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

fn bold(style: Style) -> Style {
    style.with_attrs(Attrs::new().set_bold(true))
}

fn truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        text.to_owned()
    } else if width == 0 {
        String::new()
    } else {
        let mut out: String = text.chars().take(width - 1).collect();
        out.push('…');
        out
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

impl Painter<'_, '_> {
    /// Writes `text` at `x`, clipped to end before `limit`. Returns the column after the text.
    fn text(&mut self, x: u16, y: u16, limit: u16, text: &str, style: Style) -> u16 {
        if x >= limit {
            return x;
        }
        let shown = truncate(text, usize::from(limit - x));
        if shown.is_empty() {
            return x;
        }
        self.cx.put_str(x, y, &shown, style);
        x + shown.chars().count() as u16
    }

    /// Writes `text` right-aligned so it ends at `right` (exclusive).
    fn right(&mut self, right: u16, y: u16, width: u16, text: &str, style: Style) {
        let shown = truncate(text, usize::from(width));
        let len = shown.chars().count() as u16;
        if len == 0 || right < len {
            return;
        }
        self.cx.put_str(right - len, y, &shown, style);
    }

    fn fill(&mut self, area: Rect, style: Style) {
        if area.width > 0 && area.height > 0 {
            self.cx.fill(area, style, ' ');
        }
    }

    fn lane(
        &mut self,
        area: Rect,
        history: &std::collections::VecDeque<ActivityRecord>,
        now: u64,
        window: u64,
        bg: Color,
    ) {
        let bins = model::lane(history, now, window, usize::from(area.width));
        self.lane_bins(area, &bins, bg, None);
    }

    /// Draws bins colored by state, or all in `ink` when given.
    fn lane_bins(&mut self, area: Rect, bins: &[model::Bin], bg: Color, ink: Option<Color>) {
        if area.width == 0 {
            return;
        }
        let bars: Vec<SparklineBar> = bins
            .iter()
            .map(|bin| match bin.state {
                None => SparklineBar::absent(),
                Some(state) => {
                    let base =
                        u64::from(state != SeatState::Stopped && state != SeatState::Unknown);
                    // Capped below a full block so lanes on adjacent rows stay apart.
                    let value = (base + u64::from(bin.events) * 2).min(7);
                    let color = ink.unwrap_or_else(|| self.palette.state(state));
                    SparklineBar::value(value).style(fg(color).bg(bg))
                }
            })
            .collect();
        Sparkline::new(&bars)
            .max(8)
            .style(Style::new().bg(bg))
            .absent_value_symbol(" ")
            .absent_value_style(Style::new().bg(bg))
            .pixels(self.pixels)
            .render(area, self.cx);
    }
}

fn blend(palette: &Palette, color: Color, alpha: f32) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let [gr, gg, gb] = palette.ground;
    let mix = |over: u8, under: u8| -> u8 {
        (f32::from(over) * alpha + f32::from(under) * (1.0 - alpha)).round() as u8
    };
    Color::Rgb(mix(r, gr), mix(g, gg), mix(b, gb))
}

fn counts(hosts: &[Host]) -> [usize; SeatState::ALL.len()] {
    let mut out = [0; SeatState::ALL.len()];
    for seat in hosts.iter().flat_map(|host| host.seats.values()) {
        if let Some(index) = SeatState::ALL.iter().position(|state| *state == seat.state) {
            out[index] += 1;
        }
    }
    out
}

/// Draws one frame.
pub fn render(
    cx: &mut RenderCx<'_>,
    area: Rect,
    hosts: &[Host],
    ui: &mut Ui,
    palette: &Palette,
    clock: Clock,
    pixels_available: bool,
) {
    let tier = tier(area.width, area.height);
    ui.last_tier = tier;
    let pixels = pixels_available && ui.pixels;
    let mut paint = Painter {
        cx,
        palette,
        pixels,
    };
    if tier == Tier::Tiny {
        draw_counts(
            &mut paint,
            area.x,
            area.y,
            area.x + area.width,
            hosts,
            false,
        );
        return;
    }
    let footer = area.height >= 8;
    let body_top = area.y + 1;
    let body_bottom = area.y + area.height - u16::from(footer);
    draw_header(&mut paint, area, hosts, ui, tier);
    let body = Rect::new(area.x, body_top, area.width, body_bottom - body_top);
    let rows = rows(hosts, ui, clock.wall_ms);
    let selected = ui.current(&rows);
    if ui.screen != Screen::List && selected.is_none() {
        ui.screen = Screen::List;
    }
    match (ui.screen, &selected) {
        (Screen::Timeline, Some((host, id))) => {
            draw_timeline(&mut paint, body, &hosts[*host], id, ui, clock, true);
        }
        (Screen::Details, Some((host, id))) => {
            draw_details(&mut paint, body, &hosts[*host], id, ui, clock);
        }
        _ if tier == Tier::Wide && hint(hosts).is_none() => {
            let list_width = (area.width * 52 / 100).clamp(56, 96);
            let list = Rect::new(body.x, body.y, list_width, body.height);
            draw_list(&mut paint, list, hosts, &rows, ui, clock, Tier::Medium);
            let divider = fg(palette.hairline);
            for y in body.y..body.y + body.height {
                paint.text(list_width, y, list_width + 1, "│", divider);
            }
            let detail = Rect::new(
                list_width + 2,
                body.y,
                area.width - list_width - 3,
                body.height,
            );
            match &selected {
                Some((host, id)) => {
                    draw_timeline(&mut paint, detail, &hosts[*host], id, ui, clock, false)
                }
                None => {
                    paint.text(
                        detail.x,
                        detail.y,
                        detail.x + detail.width,
                        "Select a seat to see its timeline.",
                        fg(palette.muted),
                    );
                }
            }
        }
        _ if hint(hosts).is_some() => draw_hint(&mut paint, body, hosts, clock.wall_ms),
        _ => draw_list(&mut paint, body, hosts, &rows, ui, clock, tier),
    }
    if footer {
        draw_footer(&mut paint, area, ui, hosts, pixels_available);
    }
    if ui.help {
        draw_help(&mut paint, area);
    }
}

fn draw_counts(
    paint: &mut Painter<'_, '_>,
    x: u16,
    y: u16,
    limit: u16,
    hosts: &[Host],
    words: bool,
) -> u16 {
    let totals = counts(hosts);
    let mut x = x;
    for (state, count) in SeatState::ALL.iter().zip(totals) {
        let color = paint.palette.state(*state);
        let text = if words {
            format!("{} {count} {}  ", state.glyph(), state.word())
        } else {
            format!("{}{count} ", state.glyph())
        };
        let style = if count == 0 {
            fg(paint.palette.muted)
        } else {
            fg(color)
        };
        x = paint.text(x, y, limit, &text, style);
    }
    if hosts.iter().any(|host| !host.connected()) {
        x = paint.text(x, y, limit, "!", bold(fg(paint.palette.waiting)));
    }
    x
}

fn draw_header(paint: &mut Painter<'_, '_>, area: Rect, hosts: &[Host], ui: &Ui, tier: Tier) {
    let limit = area.x + area.width;
    let y = area.y;
    let mut x = area.x + 1;
    if tier != Tier::Small {
        x = paint.text(
            x,
            y,
            limit,
            "Activity monitor",
            bold(fg(paint.palette.strong)),
        );
        x += 3;
    }
    let mut notes = Vec::new();
    if ui.filter != Filter::All {
        notes.push(ui.filter.label());
    }
    if ui.sort != Sort::Name {
        notes.push(ui.sort.label());
    }
    let note = notes.join(", ");
    let note_width = note.chars().count() as u16;
    let reserve = if note.is_empty() { 0 } else { note_width + 3 };
    // Spell out the states when they fit next to any filter or sort note.
    let worded = 4 * 14 + 2;
    let words = matches!(tier, Tier::Medium | Tier::Wide) && x + worded + reserve <= limit;
    let end = draw_counts(paint, x, y, limit.saturating_sub(reserve), hosts, words);
    if !note.is_empty() && end + 2 + note_width < limit {
        paint.right(limit - 1, y, note_width, &note, fg(paint.palette.focus));
    }
}

fn link_text(host: &Host, now: u64) -> (String, bool) {
    match host.link {
        Link::Connecting => ("connecting".to_owned(), false),
        Link::Connected => ("connected".to_owned(), false),
        Link::Retrying { since_ms } => (
            format!(
                "retrying for {}",
                model::short_age(now.saturating_sub(since_ms))
            ),
            true,
        ),
        Link::Disabled => ("feed off".to_owned(), true),
        Link::Unsupported => ("no activity feed".to_owned(), true),
        Link::PermissionDenied => ("permission denied".to_owned(), true),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_list(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    hosts: &[Host],
    rows: &[Row],
    ui: &mut Ui,
    clock: Clock,
    tier: Tier,
) {
    if area.height == 0 {
        return;
    }
    let height = usize::from(area.height);
    ui.page = height.saturating_sub(1).max(1);
    // Keep the selected row on screen, with its host row when there is room.
    if let Some(selected) = &ui.selected {
        if let Some(index) = rows.iter().position(
            |row| matches!(row, Row::Seat(h, id) if (h, id) == (&selected.0, &selected.1)),
        ) {
            if index < ui.list_offset {
                ui.list_offset = index.saturating_sub(1);
            } else if index + 1 >= ui.list_offset + height {
                // Leave the last line for the overflow marker.
                ui.list_offset = (index + 2)
                    .saturating_sub(height)
                    .min(rows.len().saturating_sub(height));
            }
        }
    }
    ui.list_offset = ui.list_offset.min(rows.len().saturating_sub(height));
    // When rows are cut off, the last line says how many instead of showing a partial list.
    let overflow = rows.len() > ui.list_offset + height;
    let shown = if overflow { height - 1 } else { height };
    let longest = hosts
        .iter()
        .flat_map(|host| host.seats.keys())
        .map(|id| id.as_str().len())
        .max()
        .unwrap_or(8);
    for (line, row) in rows.iter().skip(ui.list_offset).take(shown).enumerate() {
        let y = area.y + line as u16;
        match row {
            Row::Host(index) => draw_host_row(paint, area, y, &hosts[*index], clock, tier, longest),
            Row::Note(index) => draw_note(paint, area, y, &hosts[*index], ui.filter),
            Row::Seat(index, id) => {
                let host = &hosts[*index];
                if let Some(seat) = host.seats.get(id) {
                    let selected = ui.selected.as_ref() == Some(&(*index, id.clone()));
                    draw_seat_row(
                        paint, area, y, host, id, seat, selected, ui.motion, clock, tier, longest,
                    );
                }
            }
        }
    }
    if overflow {
        let hidden = rows.len() - ui.list_offset - shown;
        let text = format!("{hidden} more ↓");
        paint.text(
            area.x + 3,
            area.y + area.height - 1,
            area.x + area.width,
            &text,
            fg(paint.palette.muted),
        );
    }
}

struct Columns {
    name: u16,
    kind: Option<u16>,
    age: u16,
    flag: Option<u16>,
    lane: Option<(u16, u16)>,
}

fn columns(area: Rect, tier: Tier, longest: usize) -> Columns {
    let left = area.x + 3;
    let right = area.x + area.width;
    let name_width = (longest as u16).clamp(8, 24);
    match tier {
        Tier::Tiny | Tier::Small => Columns {
            name: left,
            kind: None,
            age: right.saturating_sub(1),
            flag: None,
            lane: None,
        },
        Tier::Narrow => {
            let name_width = name_width.min(area.width.saturating_sub(3 + KIND_WIDTH as u16 + 7));
            let kind = left + name_width + 2;
            Columns {
                name: left,
                kind: Some(kind),
                age: (kind + KIND_WIDTH as u16 + 5).min(right.saturating_sub(1)),
                flag: None,
                lane: None,
            }
        }
        Tier::Medium | Tier::Wide => {
            let kind = left + name_width + 2;
            let age = kind + KIND_WIDTH as u16 + 5;
            let flag = age + 2;
            let lane_x = flag + 6;
            let lane = (right > lane_x + 8).then(|| (lane_x, right - lane_x - 1));
            Columns {
                name: left,
                kind: Some(kind),
                age,
                flag: Some(flag),
                lane,
            }
        }
    }
}

fn draw_host_row(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    y: u16,
    host: &Host,
    clock: Clock,
    tier: Tier,
    longest: usize,
) {
    let palette = *paint.palette;
    let limit = area.x + area.width;
    let mut x = paint.text(area.x + 1, y, limit, &host.label, bold(fg(palette.strong)));
    let (link, alarm) = link_text(host, clock.wall_ms);
    x = paint.text(
        x + 2,
        y,
        limit,
        &link,
        fg(if alarm {
            palette.waiting
        } else {
            palette.muted
        }),
    );
    if tier == Tier::Small {
        return;
    }
    let seats = host.seats.len();
    let need = host
        .seats
        .values()
        .filter(|seat| matches!(seat.state, SeatState::Waiting | SeatState::Failed))
        .count();
    let plural = if seats == 1 { "" } else { "s" };
    x = paint.text(
        x + 2,
        y,
        limit,
        &format!("{seats} seat{plural}"),
        fg(palette.muted),
    );
    if need > 0 {
        x = paint.text(
            x + 2,
            y,
            limit,
            &format!("{need} need you"),
            fg(palette.waiting),
        );
    }
    if !host.connected() && seats > 0 {
        x = paint.text(x + 2, y, limit, "cached", fg(palette.muted));
    }
    if let Some(reset) = &host.reset {
        x = paint.text(
            x + 2,
            y,
            limit,
            &format!("reset: {}", reset.replace('_', " ")),
            fg(palette.muted),
        );
    }
    if host.evicted > 0 {
        x = paint.text(
            x + 2,
            y,
            limit,
            &format!("{} evicted", host.evicted),
            fg(palette.muted),
        );
    }
    if host.more {
        x = paint.text(x + 2, y, limit, "catching up", fg(palette.muted));
    }
    // A lane of the host's combined activity, aligned with the seat lanes.
    if let Some((lane_x, lane_w)) = columns(area, tier, longest).lane {
        if x + 2 <= lane_x && !host.seats.is_empty() {
            let width = usize::from(lane_w);
            let mut total = vec![
                model::Bin {
                    events: 0,
                    state: None
                };
                width
            ];
            for seat in host.seats.values() {
                for (sum, bin) in total.iter_mut().zip(model::lane(
                    &seat.history,
                    clock.wall_ms,
                    LIST_LANE_WINDOW_MS,
                    width,
                )) {
                    sum.events += bin.events;
                    if bin.state.is_some() {
                        sum.state = Some(SeatState::Stopped);
                    }
                }
            }
            let seats = u32::try_from(host.seats.len()).unwrap_or(u32::MAX).max(1);
            for bin in &mut total {
                // Volume only: average events per seat, in the hairline color, with no state base.
                bin.events = bin.events.div_ceil(seats);
            }
            let hairline = palette.hairline;
            paint.lane_bins(
                Rect::new(lane_x, y, lane_w, 1),
                &total,
                Color::Default,
                Some(hairline),
            );
        }
    }
}

/// What a lone host with nothing to show should tell the person, if anything.
pub fn hint(hosts: &[Host]) -> Option<(&'static str, &'static str)> {
    let [host] = hosts else { return None };
    if !host.seats.is_empty() {
        return None;
    }
    match host.link {
        Link::PermissionDenied => Some((
            "Activity read access has not been granted.",
            "Enable this plug-in with an approved activity.read scope. This view cannot change its permissions.",
        )),
        Link::Disabled => Some((
            "Activity feed is disabled for this daemon.",
            "Turn it on with experimental_activity = true under [mux] in the Prismattyc config or the settings panel. The running pmuxd picks it up within a second, and this view connects by itself.",
        )),
        Link::Unsupported => Some((
            "This pmuxd has no activity feed.",
            "It is older than the feed. Update Prismattyc for a pmuxd that has one, then set experimental_activity = true under [mux]; the feed is off by default.",
        )),
        Link::Retrying { .. } => Some((
            "Can't reach pmuxd.",
            "Start Prismattyc, or run pmux up in another pane. This view connects by itself.",
        )),
        Link::Connecting | Link::Connected => None,
    }
}

/// Splits text into lines of at most `width` characters at spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn draw_hint(paint: &mut Painter<'_, '_>, area: Rect, hosts: &[Host], now: u64) {
    let Some((title, body)) = hint(hosts) else {
        return;
    };
    let palette = *paint.palette;
    let left = area.x + if area.width >= 40 { 3 } else { 1 };
    let width = usize::from(area.x + area.width - left)
        .saturating_sub(1)
        .clamp(1, 72);
    let lines = wrap(body, width);
    let height = 2 + lines.len() as u16;
    let top = area.y + area.height.saturating_sub(height) / 2;
    let limit = area.x + area.width;
    let host = &hosts[0];
    let label = format!("{}  {}", host.label, link_text(host, now).0);
    if top > area.y {
        paint.text(area.x + 1, area.y, limit, &label, fg(palette.muted));
    }
    paint.text(left, top, limit, title, bold(fg(palette.strong)));
    for (index, line) in lines.iter().enumerate() {
        let y = top + 2 + index as u16;
        if y >= area.y + area.height {
            break;
        }
        paint.text(left, y, limit, line, fg(palette.muted));
    }
}

fn draw_note(paint: &mut Painter<'_, '_>, area: Rect, y: u16, host: &Host, filter: Filter) {
    let text = match (&host.link, host.seats.is_empty()) {
        (Link::Connecting, _) => "Connecting to the activity feed.",
        (Link::Disabled, _) => "Activity feed is disabled for this daemon.",
        (Link::PermissionDenied, _) => "Activity read access has not been granted.",
        (Link::Unsupported, _) => "This pmuxd is older than the activity feed.",
        (Link::Retrying { .. }, true) => "Can't reach this host's feed. Retrying.",
        (_, true) => "No activity yet. Seats appear when their hooks publish.",
        (_, false) if filter != Filter::All => "No seats match this filter. Press f to change it.",
        _ => "",
    };
    paint.text(
        area.x + 3,
        y,
        area.x + area.width,
        text,
        fg(paint.palette.muted),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_seat_row(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    y: u16,
    host: &Host,
    id: &AgentId,
    seat: &Seat,
    selected: bool,
    motion: bool,
    clock: Clock,
    tier: Tier,
    longest: usize,
) {
    let palette = *paint.palette;
    let limit = area.x + area.width;
    let stale = model::is_stale(host, seat, clock.wall_ms);
    let fade = motion
        .then_some(seat.changed_at)
        .flatten()
        .map(|at| clock.mono_ms.saturating_sub(at))
        .filter(|elapsed| *elapsed < FADE_MS);
    let bg = match (fade, selected) {
        (Some(elapsed), _) => {
            let alpha = 0.38 * (1.0 - elapsed as f32 / FADE_MS as f32);
            blend(&palette, palette.state(seat.state), alpha)
        }
        (None, true) => palette.selection,
        (None, false) => Color::Default,
    };
    let base = Style::new().bg(bg);
    paint.fill(Rect::new(area.x, y, area.width, 1), base);
    if selected {
        paint.text(area.x, y, limit, "▌", base.fg(palette.focus));
    }
    let state_color = palette.state(seat.state);
    paint.text(
        area.x + 1,
        y,
        limit,
        &seat.state.glyph().to_string(),
        base.fg(state_color),
    );
    let cols = columns(area, tier, longest);
    let name_color = if seat.state == SeatState::Stopped {
        palette.muted
    } else {
        palette.text
    };
    let name_style = if selected {
        bold(base.fg(palette.strong))
    } else {
        base.fg(name_color)
    };
    let name_limit = cols
        .kind
        .map_or(cols.age.saturating_sub(5), |kind| kind - 1);
    paint.text(cols.name, y, name_limit, id.as_str(), name_style);
    let latest = seat.latest();
    if let Some(kind) = cols.kind {
        paint.text(
            kind,
            y,
            kind + KIND_WIDTH as u16,
            &model::label(&latest.event.kind),
            base.fg(palette.muted),
        );
    }
    let age = model::short_age(model::age_ms(seat, clock.wall_ms));
    let age_style = if stale {
        base.fg(palette.waiting)
    } else {
        base.fg(palette.muted)
    };
    paint.right(cols.age + 1, y, 4, &age, age_style);
    if let Some(flag) = cols.flag {
        match seat.state {
            SeatState::Idle | SeatState::Stuck | SeatState::Stopped => {
                paint.text(
                    flag,
                    y,
                    flag + 5,
                    seat.state.word(),
                    base.fg(palette.state(seat.state)),
                );
            }
            _ if stale => {
                paint.text(flag, y, flag + 5, "stale", base.fg(palette.waiting));
            }
            _ => {}
        }
    }
    if let Some((lane_x, lane_w)) = cols.lane {
        paint.lane(
            Rect::new(lane_x, y, lane_w, 1),
            &seat.history,
            clock.wall_ms,
            LIST_LANE_WINDOW_MS,
            bg,
        );
    }
}

fn kind_glyph(kind: ActivityKind) -> (char, SeatState) {
    use ActivityKind::*;
    match kind {
        ToolStarted | CommandStarted | EditStarted | TestStarted => ('▸', SeatState::Working),
        ToolFinished | CommandFinished | EditFinished => ('·', SeatState::Unknown),
        ToolFailed | CommandFailed | EditFailed | TestFailed => ('✕', SeatState::Failed),
        TestPassed => ('✓', SeatState::Working),
        Waiting | TurnFinished => ('◆', SeatState::Waiting),
        Stopped | SessionFinished => ('■', SeatState::Stopped),
        Working | TurnStarted => ('●', SeatState::Working),
        SessionStarted => ('○', SeatState::Waiting),
        MailSent => ('»', SeatState::Unknown),
        MailReceived => ('«', SeatState::Unknown),
    }
}

fn is_failure(kind: ActivityKind) -> bool {
    kind_glyph(kind).1 == SeatState::Failed
}

fn draw_timeline(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    host: &Host,
    id: &AgentId,
    ui: &mut Ui,
    clock: Clock,
    full: bool,
) {
    let palette = *paint.palette;
    let Some(seat) = host.seats.get(id) else {
        return;
    };
    if area.height == 0 || area.width < 8 {
        return;
    }
    let limit = area.x + area.width;
    let mut y = area.y;
    let mut x = area.x + u16::from(full);
    if full {
        x = paint.text(x, y, limit, "← ", fg(palette.muted));
    }
    x = paint.text(x, y, limit, id.as_str(), bold(fg(palette.strong)));
    x = paint.text(
        x + 1,
        y,
        limit,
        &format!("on {}", host.label),
        fg(palette.muted),
    );
    let state = format!(
        "{} {} for {}",
        seat.state.glyph(),
        seat.state.word(),
        model::short_age(clock.wall_ms.saturating_sub(seat.state_since_ms))
    );
    x = paint.text(x + 3, y, limit, &state, fg(palette.state(seat.state)));
    if matches!(
        seat.state,
        SeatState::Idle | SeatState::Stuck | SeatState::Stopped
    ) {
        paint.text(
            x + 2,
            y,
            limit,
            seat.state.word(),
            fg(palette.state(seat.state)),
        );
    } else if model::is_stale(host, seat, clock.wall_ms) {
        paint.text(x + 2, y, limit, "stale", fg(palette.waiting));
    }
    y += 1;
    let left = area.x + u16::from(full);
    let bottom = area.y + area.height;
    if y < bottom {
        let tasks: std::collections::BTreeSet<&str> = seat
            .history
            .iter()
            .map(|record| {
                record
                    .event
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.task.as_deref())
                    .unwrap_or("unassigned")
            })
            .collect();
        let failures = seat
            .history
            .iter()
            .filter(|r| is_failure(r.event.kind))
            .count();
        let summary = format!(
            "{} event{}  {} task{}  {} failure{}  last {} ago",
            seat.history.len(),
            if seat.history.len() == 1 { "" } else { "s" },
            tasks.len(),
            if tasks.len() == 1 { "" } else { "s" },
            failures,
            if failures == 1 { "" } else { "s" },
            model::short_age(model::age_ms(seat, clock.wall_ms)),
        );
        paint.text(left, y, limit, &summary, fg(palette.muted));
        y += 1;
    }
    let lane_width = limit.saturating_sub(left + 1);
    if area.height >= 8 && y + 1 < bottom && lane_width >= 12 {
        y += u16::from(area.height >= 12);
        paint.lane(
            Rect::new(left, y, lane_width, 1),
            &seat.history,
            clock.wall_ms,
            DETAIL_LANE_WINDOW_MS,
            Color::Default,
        );
        y += 1;
        let axis = fg(palette.muted);
        paint.text(left, y, limit, "-10m", axis);
        paint.text(left + lane_width / 2 - 1, y, limit, "-5m", axis);
        paint.right(left + lane_width, y, 3, "now", axis);
        y += 2;
    }
    if y >= bottom {
        return;
    }
    // Newest first, grouped by the ticket/task label rather than a correlation hash.
    let mut lines: Vec<Option<&ActivityRecord>> = Vec::new();
    let mut previous_task: Option<&str> = None;
    for record in seat.history.iter().rev() {
        let task = record
            .event
            .detail
            .as_ref()
            .and_then(|detail| detail.task.as_deref())
            .unwrap_or("unassigned");
        if previous_task.is_some_and(|previous| previous != task) {
            lines.push(None);
        }
        previous_task = Some(task);
        lines.push(Some(record));
    }
    let height = usize::from(bottom - y);
    ui.page = height.saturating_sub(1).max(1);
    ui.timeline_offset = ui.timeline_offset.min(lines.len().saturating_sub(height));
    let width = limit.saturating_sub(left);
    let show_gap = width >= 40;
    let show_source = width >= 52;
    let show_run = ui.hashes && width >= 62;
    let show_event = ui.hashes && width >= 72;
    let show_seq = width >= 82;
    let history: Vec<&ActivityRecord> = seat.history.iter().collect();
    for (line, entry) in lines
        .iter()
        .skip(ui.timeline_offset)
        .take(height)
        .enumerate()
    {
        let row_y = y + line as u16;
        let Some(record) = entry else {
            // The task below this hairline is older.
            let older = lines[ui.timeline_offset + line + 1..]
                .iter()
                .flatten()
                .next();
            let label = older
                .map(|record| {
                    let task = record
                        .event
                        .detail
                        .as_ref()
                        .and_then(|detail| detail.task.as_deref())
                        .unwrap_or("unassigned");
                    format!("── task {task} ── ")
                })
                .unwrap_or_default();
            let x = paint.text(left + 6, row_y, limit, &label, fg(palette.muted));
            let rule = "─".repeat(usize::from(limit.saturating_sub(x + 1)));
            paint.text(x, row_y, limit - 1, &rule, fg(palette.hairline));
            continue;
        };
        let mut x = left;
        let age = model::short_age(clock.wall_ms.saturating_sub(record.received_at_ms));
        paint.right(x + 5, row_y, 5, &age, fg(palette.muted));
        x += 6;
        if show_gap {
            let position = history
                .iter()
                .position(|r| r.sequence == record.sequence)
                .unwrap_or(0);
            let gap = position
                .checked_sub(1)
                .map(|before| {
                    model::short_gap(
                        record
                            .received_at_ms
                            .saturating_sub(history[before].received_at_ms),
                    )
                })
                .unwrap_or_default();
            paint.text(x, row_y, limit, &gap, fg(palette.hairline_text()));
            x += 7;
        }
        let (glyph, role) = kind_glyph(record.event.kind);
        let glyph_color = if role == SeatState::Unknown {
            palette.muted
        } else {
            palette.state(role)
        };
        paint.text(x, row_y, limit, &glyph.to_string(), fg(glyph_color));
        x += 2;
        let kind_color = if role == SeatState::Failed {
            palette.failed
        } else {
            palette.text
        };
        paint.text(
            x,
            row_y,
            limit,
            &model::label(&record.event.kind),
            fg(kind_color),
        );
        x += KIND_WIDTH as u16 + 1;
        if let Some(detail) = record.event.detail.as_ref() {
            if let Some(tool) = detail.tool.as_deref() {
                x = paint.text(x, row_y, limit, tool, fg(palette.muted)) + 1;
            }
            if let Some(file) = detail.file.as_deref() {
                x = paint.text(x, row_y, limit, file, fg(palette.text)) + 1;
            } else if let Some(command) = detail.command.as_deref() {
                x = paint.text(x, row_y, limit, command, fg(palette.text)) + 1;
            }
            if let Some(duration_ms) = detail.duration_ms {
                x = paint.text(
                    x,
                    row_y,
                    limit,
                    &format!("{duration_ms}ms"),
                    fg(palette.muted),
                ) + 1;
            }
            if let Some(exit_code) = detail.exit_code {
                x = paint.text(
                    x,
                    row_y,
                    limit,
                    &format!("exit {exit_code}"),
                    fg(palette.muted),
                ) + 1;
            }
        }
        if show_source {
            x = paint.text(
                x,
                row_y,
                limit,
                &model::label(&record.event.source),
                fg(palette.muted),
            ) + 1;
        }
        if show_run {
            paint.text(
                x,
                row_y,
                limit,
                &format!("run {}", short_id(&record.event.run_id)),
                fg(palette.muted),
            );
            x += 13;
        }
        if show_event {
            paint.text(
                x,
                row_y,
                limit,
                &format!("ev {}", short_id(&record.event.event_id)),
                fg(palette.muted),
            );
            x += 12;
        }
        if show_seq {
            paint.text(
                x,
                row_y,
                limit,
                &format!("#{}", record.sequence),
                fg(palette.muted),
            );
        }
    }
}

/// One seat at a glance: who it is, then what it did, grouped by task.
fn draw_details(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    host: &Host,
    id: &AgentId,
    ui: &mut Ui,
    clock: Clock,
) {
    let palette = *paint.palette;
    let Some(seat) = host.seats.get(id) else {
        return;
    };
    if area.height == 0 || area.width < 8 {
        return;
    }
    let limit = area.x + area.width;
    let bottom = area.y + area.height;
    let mut y = area.y;
    let mut x = paint.text(area.x, y, limit, "← ", fg(palette.muted));
    x = paint.text(x, y, limit, id.as_str(), bold(fg(palette.strong)));
    x = paint.text(
        x + 1,
        y,
        limit,
        &format!("on {}", host.label),
        fg(palette.muted),
    );
    let state = format!(
        "{} {} for {}",
        seat.state.glyph(),
        seat.state.word(),
        model::short_age(clock.wall_ms.saturating_sub(seat.state_since_ms))
    );
    paint.text(x + 3, y, limit, &state, fg(palette.state(seat.state)));
    y += 1;

    let groups = model::task_groups(seat);
    let unknown = "unknown";
    let info = &seat.info;
    let facts = [
        ("Harness", info.harness.as_deref().unwrap_or(unknown)),
        ("Model", info.model.as_deref().unwrap_or(unknown)),
        ("Quota", info.quota.as_deref().unwrap_or(unknown)),
        ("Project", info.project.as_deref().unwrap_or(unknown)),
    ];
    let ticket = groups
        .iter()
        .find_map(|group| group.task)
        .unwrap_or("none reported");
    let progress = format!(
        "{} ago",
        model::short_age(model::age_ms(seat, clock.wall_ms))
    );
    // Facts wrap onto as many lines as the width needs.
    let mut fact_x = area.x + 2;
    for (label, value) in facts
        .into_iter()
        .chain([("Ticket", ticket), ("Last progress", progress.as_str())])
    {
        let width = (label.chars().count() + value.chars().count() + 4) as u16;
        if fact_x > area.x + 2 && fact_x + width > limit {
            fact_x = area.x + 2;
            y += 1;
        }
        if y >= bottom {
            return;
        }
        let value_x = paint.text(fact_x, y, limit, label, fg(palette.muted)) + 1;
        let color = if value == unknown {
            palette.muted
        } else {
            palette.text
        };
        fact_x = paint.text(value_x, y, limit, value, fg(color)) + 3;
    }
    y += 2;
    if y >= bottom {
        return;
    }
    if groups.is_empty() {
        paint.text(
            area.x + 2,
            y,
            limit,
            "No activity reported yet.",
            fg(palette.muted),
        );
        return;
    }

    // One line per group heading and per activity, newest first.
    let mut lines: Vec<(Option<&model::TaskGroup<'_>>, Option<&model::Activity<'_>>)> = Vec::new();
    for group in &groups {
        lines.push((Some(group), None));
        lines.extend(
            group
                .activities
                .iter()
                .map(|activity| (None, Some(activity))),
        );
    }
    let height = usize::from(bottom - y);
    ui.page = height.saturating_sub(1).max(1);
    ui.timeline_offset = ui.timeline_offset.min(lines.len().saturating_sub(height));
    let width = usize::from(limit.saturating_sub(area.x));
    let show_tool = width >= 48;
    for (line, (group, activity)) in lines
        .iter()
        .skip(ui.timeline_offset)
        .take(height)
        .enumerate()
    {
        let row_y = y + line as u16;
        if let Some(group) = group {
            draw_task_heading(paint, area.x, row_y, limit, group, clock);
            continue;
        }
        if let Some(activity) = activity {
            draw_activity(
                paint,
                area.x + 2,
                row_y,
                limit,
                activity,
                ui,
                clock,
                show_tool,
            );
        }
    }
}

fn draw_task_heading(
    paint: &mut Painter<'_, '_>,
    x: u16,
    y: u16,
    limit: u16,
    group: &model::TaskGroup<'_>,
    clock: Clock,
) {
    let palette = *paint.palette;
    let name = group.task.unwrap_or("no ticket");
    let x = paint.text(x, y, limit, name, bold(fg(palette.strong)));
    let operations = group
        .activities
        .iter()
        .filter(|activity| activity.action.is_some())
        .count();
    let mut summary = format!(
        "{operations} activit{}",
        if operations == 1 { "y" } else { "ies" }
    );
    let failed = group.failed();
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    summary.push_str(&format!(
        ", last {} ago",
        model::short_age(clock.wall_ms.saturating_sub(group.last_ms()))
    ));
    paint.text(x + 2, y, limit, &summary, fg(palette.muted));
}

#[allow(clippy::too_many_arguments)]
fn draw_activity(
    paint: &mut Painter<'_, '_>,
    left: u16,
    y: u16,
    limit: u16,
    activity: &model::Activity<'_>,
    ui: &Ui,
    clock: Clock,
    show_tool: bool,
) {
    use model::Outcome;
    let palette = *paint.palette;
    let age = model::short_age(clock.wall_ms.saturating_sub(activity.at_ms));
    paint.right(left + 4, y, 4, &age, fg(palette.muted));
    let mut x = left + 5;
    let (glyph, color) = match activity.outcome {
        Outcome::Succeeded => ('✓', palette.state(SeatState::Working)),
        Outcome::Failed => ('✗', palette.failed),
        Outcome::Running => ('…', palette.state(SeatState::Working)),
        Outcome::NoResult => ('?', palette.muted),
        Outcome::Note(_) => ('·', palette.muted),
    };
    paint.text(x, y, limit, &glyph.to_string(), fg(color));
    x += 2;
    let Some(action) = activity.action else {
        if let Outcome::Note(text) = activity.outcome {
            paint.text(x, y, limit, text, fg(palette.muted));
        }
        return;
    };
    paint.text(x, y, limit, action.word(), fg(palette.text));
    x += 8;
    if show_tool {
        if let Some(tool) = activity.tool {
            paint.text(x, y, limit, &truncate(tool, 10), fg(palette.muted));
        }
        x += 11;
    }
    // The result sits at the right edge so durations line up.
    let result = match activity.outcome {
        Outcome::Running => "running".to_owned(),
        Outcome::NoResult => "no result".to_owned(),
        _ => {
            let mut text = activity
                .duration_ms
                .map(model::duration_text)
                .unwrap_or_default();
            if activity.outcome == Outcome::Failed {
                if let Some(code) = activity.exit_code {
                    if !text.is_empty() {
                        text.push_str("  ");
                    }
                    text.push_str(&format!("exit {code}"));
                }
            }
            text
        }
    };
    let ids = ui.hashes.then(|| {
        let mut ids = format!("run {}", short_id(activity.run_id));
        if let Some(operation) = activity.operation_id {
            ids.push_str(&format!(" op {}", short_id(operation)));
        }
        ids
    });
    let tail = match &ids {
        Some(ids) if result.is_empty() => ids.clone(),
        Some(ids) => format!("{result}  {ids}"),
        None => result,
    };
    let tail_width = tail.chars().count() as u16;
    let tail_x = limit.saturating_sub(tail_width + 1).max(x);
    let target_width = usize::from(tail_x.saturating_sub(x + 1));
    if let Some(target) = activity.target {
        paint.text(
            x,
            y,
            limit,
            &truncate(target, target_width),
            fg(palette.text),
        );
    }
    let tail_color = if activity.outcome == Outcome::Failed {
        palette.failed
    } else {
        palette.muted
    };
    paint.text(tail_x, y, limit, &tail, fg(tail_color));
}

impl Palette {
    /// Quieter than muted text, for secondary numbers.
    fn hairline_text(&self) -> Color {
        if self.dark {
            rgb(0x7d8594)
        } else {
            rgb(0x8a929e)
        }
    }
}

fn draw_footer(
    paint: &mut Painter<'_, '_>,
    area: Rect,
    ui: &Ui,
    hosts: &[Host],
    pixels_available: bool,
) {
    let y = area.y + area.height - 1;
    let limit = area.x + area.width;
    let idle = hint(hosts).is_some();
    let hints: &[(&str, &str)] = match ui.screen {
        _ if idle => &[("q", "quit")],
        Screen::List => &[
            ("↑↓", "move"),
            ("⏎", "timeline"),
            ("d", "details"),
            ("q", "quit"),
            ("⇥", "host"),
            ("f", "filter"),
            ("?", "keys"),
            ("s", "sort"),
            ("p", "pixels"),
            ("i", "IDs"),
        ],
        Screen::Timeline => &[
            ("↑↓", "scroll"),
            ("esc", "back"),
            ("i", "IDs"),
            ("q", "quit"),
            ("?", "keys"),
        ],
        Screen::Details => &[
            ("↑↓", "scroll"),
            ("esc", "back"),
            ("i", "IDs"),
            ("q", "quit"),
            ("?", "keys"),
        ],
    };
    // Keep the most important hints that fit, then show them in a stable order.
    let mut budget = usize::from(area.width.saturating_sub(2));
    let mut keep = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        let cost = key.chars().count() + label.len() + 3;
        if *key == "p" && !pixels_available {
            continue;
        }
        if cost > budget {
            break;
        }
        budget -= cost;
        keep.push(index);
    }
    let order: &[usize] = match ui.screen {
        _ if idle => &[0],
        Screen::List => &[0, 1, 2, 4, 5, 7, 8, 9, 6, 3],
        Screen::Timeline => &[0, 1, 2, 3, 4],
        Screen::Details => &[0, 1, 2, 3],
    };
    let mut x = area.x + 1;
    for index in order.iter().filter(|index| keep.contains(index)) {
        let (key, label) = hints[*index];
        x = paint.text(x, y, limit, key, fg(paint.palette.text));
        x = paint.text(x + 1, y, limit, label, fg(paint.palette.muted));
        x += 2;
    }
}

fn draw_help(paint: &mut Painter<'_, '_>, area: Rect) {
    let lines = [
        ("↑ ↓  j k", "move, or scroll the timeline or details"),
        ("PgUp PgDn", "move by a page"),
        ("g G", "first or last"),
        ("Tab", "next host (Shift-Tab: previous)"),
        ("Enter → l", "open the timeline"),
        ("d", "open selected seat details"),
        ("i", "show or hide run and operation ids"),
        ("Esc ← h", "back to the list"),
        ("f", "filter: all, needs attention, working"),
        ("s", "sort: name, most recent, state"),
        ("p", "pixel lanes on or off, where supported"),
        ("q", "quit"),
    ];
    let palette = *paint.palette;
    let width = 58.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(1));
    if width < 20 || height < 5 {
        return;
    }
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let surface = Style::new().bg(palette.raised);
    paint.fill(Rect::new(x, y, width, height), surface);
    let border = surface.fg(palette.hairline);
    let limit = x + width;
    paint.text(
        x,
        y,
        limit,
        &format!("╭{}╮", "─".repeat(usize::from(width - 2))),
        border,
    );
    for row in y + 1..y + height - 1 {
        paint.text(x, row, limit, "│", border);
        paint.text(limit - 1, row, limit, "│", border);
    }
    paint.text(
        x,
        y + height - 1,
        limit,
        &format!("╰{}╯", "─".repeat(usize::from(width - 2))),
        border,
    );
    paint.text(
        x + 2,
        y + 1,
        limit - 1,
        "Keys",
        bold(surface.fg(palette.strong)),
    );
    for (index, (key, action)) in lines.iter().enumerate().take(usize::from(height - 4)) {
        let row = y + 3 + index as u16;
        paint.text(x + 2, row, x + 13, key, surface.fg(palette.text));
        paint.text(x + 14, row, limit - 1, action, surface.fg(palette.muted));
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::app::Monitor;
    use crate::model::tests::{page_in, record_run};
    use anyhow::Result;
    use lunatui_core::{ClusterTable, Grid};

    pub const NOW: u64 = 1_000_000;

    pub fn press(monitor: &mut Monitor, code: KeyCode) -> bool {
        monitor.event(&Event::Key(KeyEvent::press(code)))
    }

    /// Two hosts: mac with three seats in different states, windows disconnected with one.
    pub fn fleet() -> Result<Monitor> {
        let mut monitor = Monitor::new(vec!["mac".into(), "windows".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        monitor.hosts[0].apply(
            page_in(
                "e1",
                vec![
                    record_run("builder", 1, "turn_started", NOW - 200_000, "run-aaaaaaaa1")?,
                    record_run("builder", 2, "test_started", NOW - 150_000, "run-aaaaaaaa1")?,
                    record_run("builder", 3, "test_failed", NOW - 120_000, "run-aaaaaaaa1")?,
                    record_run("reviewer", 4, "turn_started", NOW - 90_000, "run-bbbbbbbb1")?,
                    record_run(
                        "reviewer",
                        5,
                        "turn_finished",
                        NOW - 41_000,
                        "run-bbbbbbbb1",
                    )?,
                    record_run("worker-a", 6, "turn_started", NOW - 60_000, "run-cccccccc1")?,
                    record_run("worker-a", 7, "edit_finished", NOW - 9_000, "run-cccccccc2")?,
                    record_run(
                        "worker-a",
                        8,
                        "command_started",
                        NOW - 4_000,
                        "run-cccccccc2",
                    )?,
                ],
                None,
            )?,
            0,
        );
        monitor.hosts[1].apply(
            page_in(
                "e2",
                vec![record_run(
                    "win-agent",
                    1,
                    "tool_started",
                    NOW - 19_000,
                    "run-dddddddd1",
                )?],
                None,
            )?,
            0,
        );
        monitor.hosts[1].disconnect(NOW - 12_000);
        Ok(monitor)
    }

    pub fn draw(monitor: &mut Monitor, width: u16, height: u16) -> (String, Grid, ClusterTable) {
        let (grid, clusters) = monitor.frame(width, height, 10_000);
        (grid.display_text(&clusters), grid, clusters)
    }

    pub fn line_of<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle}:\n{text}"))
    }

    pub fn column_of(line: &str, needle: &str) -> u16 {
        line[..line.find(needle).unwrap()].chars().count() as u16
    }

    pub fn press_char(monitor: &mut Monitor, key: char) -> bool {
        press(monitor, KeyCode::Char(key))
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::app::Monitor;
    use crate::model::tests::{page_in, record_run};
    use anyhow::Result;

    /// Reviewed frames: each was checked against the design note before being pinned here.
    const FRAMES: &[(u16, u16, &str, &str)] = &[
        (
            100,
            14,
            "",
            r#"
 Activity monitor   ● 2 working  ◆ 1 waiting  ○ 0 idle  ! 0 stuck  ✕ 1 failed  ■ 0 down  !
 mac  connected  3 seats  2 need you                         ▂         ▂    ▂     ▂    ▂   ▂     ▂▂
▌✕ builder    test_failed         2m stale                   ▃▁▁▁▁▁▁▁▁▁▃▁▁▁▁▃▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁
 ◆ reviewer   turn_finished      41s stale                                        ▃▁▁▁▁▁▁▁▁▃▁▁▁▁▁▁▁
 ● worker-a   command_started     4s                                                   ▃▁▁▁▁▁▁▁▁▁▃▃
 windows  retrying for 12s  1 seat  cached
 ● win-agent  tool_started       19s stale                                                     ▃▁▁▁






 ↑↓ move  ⏎ timeline  d details  ⇥ host  f filter  s sort  i IDs  ? keys  q quit"#,
        ),
        (
            60,
            12,
            "",
            r#"
 Activity monitor   ●2 ◆1 ○0 !0 ✕1 ■0 !
 mac  connected  3 seats  2 need you
▌✕ builder    test_failed         2m
 ◆ reviewer   turn_finished      41s
 ● worker-a   command_started     4s
 windows  retrying for 12s  1 seat  cached
 ● win-agent  tool_started       19s




 ↑↓ move  ⏎ timeline  d details  ⇥ host  f filter  q quit"#,
        ),
        (
            30,
            8,
            "",
            r#"
 ●2 ◆1 ○0 !0 ✕1 ■0 !
 mac  connected
▌✕ builder                  2m
 ◆ reviewer                41s
 ● worker-a                 4s
 windows  retrying for 12s
 ● win-agent               19s
 ↑↓ move  ⏎ timeline"#,
        ),
        (
            120,
            20,
            "j",
            r#"
 Activity monitor   ● 2 working  ◆ 1 waiting  ○ 0 idle  ! 0 stuck  ✕ 1 failed  ■ 0 down  !
 mac  connected  3 seats  2 need you             ▂  ▂▂ ▂ ▂▂ ▂ │ reviewer on mac   ◆ waiting for 41s  stale
 ✕ builder    test_failed         2m stale       ▃▁▁▃▃▁▁▁▁▁▁▁ │ 2 events  1 task  0 failures  last 41s ago
▌◆ reviewer   turn_finished      41s stale             ▃▁▁▃▁▁ │
 ● worker-a   command_started     4s                     ▃▁▁▅ │                                              ▃▁▁▁▁▃▁▁▁
 windows  retrying for 12s  1 seat  cached                    │ -10m                      -5m                      now
 ● win-agent  tool_started       19s stale                 ▃▁ │
                                                              │   41s +49s   ◆ turn_finished    hooks
                                                              │    1m        ● turn_started     hooks
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
                                                              │
 ↑↓ move  ⏎ timeline  d details  ⇥ host  f filter  s sort  i IDs  ? keys  q quit"#,
        ),
        (
            90,
            16,
            "jl",
            r#"
 Activity monitor   ● 2 working  ◆ 1 waiting  ○ 0 idle  ! 0 stuck  ✕ 1 failed  ■ 0 down  !
 ← reviewer on mac   ◆ waiting for 41s  stale
 2 events  1 task  0 failures  last 41s ago

                                                                           ▃▁▁▁▁▁▁▃▁▁▁▁▁▁
 -10m                                       -5m                                       now

   41s +49s   ◆ turn_finished    hooks #5
    1m        ● turn_started     hooks #4






 ↑↓ scroll  esc back  i IDs  q quit  ? keys"#,
        ),
        (
            70,
            20,
            "?",
            r#"
 Activity monitor   ●2 ◆1 ○0 !0 ✕1 ■0 !
 mac  connected  3 seats  2 need you
▌✕ bui╭────────────────────────────────────────────────────────╮
 ◆ rev│ Keys                                                   │
 ● wor│                                                        │
 windo│ ↑ ↓  j k    move, or scroll the timeline or details    │
 ● win│ PgUp PgDn   move by a page                             │
      │ g G         first or last                              │
      │ Tab         next host (Shift-Tab: previous)            │
      │ Enter → l   open the timeline                          │
      │ d           open selected seat details                 │
      │ i           show or hide run and operation ids         │
      │ Esc ← h     back to the list                           │
      │ f           filter: all, needs attention, working      │
      │ s           sort: name, most recent, state             │
      │ p           pixel lanes on or off, where supported     │
      │ q           quit                                       │
      ╰────────────────────────────────────────────────────────╯

 ↑↓ move  ⏎ timeline  d details  ⇥ host  f filter  ? keys  q quit"#,
        ),
        (
            44,
            10,
            "l",
            r#"
 Activity monitor   ●2 ◆1 ○0 !0 ✕1 ■0 !
 ← builder on mac   ✕ failed for 2m  stale
 3 events  1 task  1 failure  last 2m ago
                             ▃▁▁▃▁▃▁▁▁▁▁▁▁▁
 -10m                -5m                now

    2m +30s   ✕ test_failed
    2m +50s   ▸ test_started
    3m        ● turn_started
 ↑↓ scroll  esc back  i IDs  q quit"#,
        ),
    ];

    #[test]
    fn every_tier_draws_the_reviewed_frame() -> Result<()> {
        for (width, height, keys, expected) in FRAMES {
            let mut monitor = fleet()?;
            for key in keys.chars() {
                press_char(&mut monitor, key);
            }
            let (text, ..) = draw(&mut monitor, *width, *height);
            let actual: Vec<&str> = text.lines().map(str::trim_end).collect();
            let expected: Vec<&str> = expected.trim_start_matches('\n').lines().collect();
            assert_eq!(actual, expected, "{width}x{height} after {keys:?}");
        }
        Ok(())
    }

    fn selected(monitor: &Monitor) -> Option<(usize, String)> {
        monitor
            .ui
            .selected
            .as_ref()
            .map(|(host, id)| (*host, id.as_str().to_owned()))
    }

    /// One host with 30 seats named seat-01..seat-30, drawn once at 60x12.
    fn many() -> Result<Monitor> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        let records = (1..=30)
            .map(|n| record_run(&format!("seat-{n:02}"), n, "working", NOW - 1_000, "run-1"))
            .collect::<Result<Vec<_>>>()?;
        monitor.hosts[0].apply(page_in("e", records, None)?, 0);
        monitor.frame(60, 12, 0);
        Ok(monitor)
    }

    #[test]
    fn page_keys_move_by_the_visible_rows_and_home_end_reach_the_ends() -> Result<()> {
        let mut monitor = many()?;
        // 60x12: header and footer leave 10 list rows, so a page is 9 rows.
        press(&mut monitor, KeyCode::PageDown);
        assert_eq!(selected(&monitor), Some((0, "seat-10".into())));
        press(&mut monitor, KeyCode::PageUp);
        assert_eq!(selected(&monitor), Some((0, "seat-01".into())));
        press(&mut monitor, KeyCode::End);
        assert_eq!(selected(&monitor), Some((0, "seat-30".into())));
        press(&mut monitor, KeyCode::Char('k'));
        assert_eq!(selected(&monitor), Some((0, "seat-29".into())));
        press(&mut monitor, KeyCode::Up);
        assert_eq!(selected(&monitor), Some((0, "seat-28".into())));
        press(&mut monitor, KeyCode::Home);
        assert_eq!(selected(&monitor), Some((0, "seat-01".into())));
        press(&mut monitor, KeyCode::Char('G'));
        assert_eq!(selected(&monitor), Some((0, "seat-30".into())));
        press(&mut monitor, KeyCode::Char('g'));
        assert_eq!(selected(&monitor), Some((0, "seat-01".into())));
        Ok(())
    }

    #[test]
    fn timeline_keys_scroll_and_every_back_key_returns_to_the_list() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        let records = (1..=40)
            .map(|n| record_run("busy", n, "tool_started", NOW - 50_000 + n * 1_000, "run-1"))
            .collect::<Result<Vec<_>>>()?;
        monitor.hosts[0].apply(page_in("e", records, None)?, 0);
        let first_row = |monitor: &mut Monitor| -> String {
            let (text, ..) = draw(monitor, 60, 16);
            text.lines()
                .find(|line| line.contains("tool_started"))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        press(&mut monitor, KeyCode::Enter);
        assert!(first_row(&mut monitor).starts_with("10s"), "newest first");
        press(&mut monitor, KeyCode::Char('j'));
        press(&mut monitor, KeyCode::Down);
        assert!(first_row(&mut monitor).starts_with("12s"));
        press(&mut monitor, KeyCode::Char('k'));
        assert!(first_row(&mut monitor).starts_with("11s"));
        press(&mut monitor, KeyCode::Up);
        press(&mut monitor, KeyCode::Up);
        assert!(
            first_row(&mut monitor).starts_with("10s"),
            "scrolling stops at the top"
        );
        press(&mut monitor, KeyCode::PageDown);
        let paged = first_row(&mut monitor);
        assert!(
            paged.starts_with("17s"),
            "a page is the visible rows minus one: {paged}"
        );
        press(&mut monitor, KeyCode::PageUp);
        assert!(first_row(&mut monitor).starts_with("10s"));
        press(&mut monitor, KeyCode::End);
        assert!(
            first_row(&mut monitor).starts_with("42s"),
            "End shows the oldest page"
        );
        press(&mut monitor, KeyCode::Char('g'));
        assert!(first_row(&mut monitor).starts_with("10s"));
        press(&mut monitor, KeyCode::Char('G'));
        assert!(first_row(&mut monitor).starts_with("42s"));
        press(&mut monitor, KeyCode::Home);
        assert!(first_row(&mut monitor).starts_with("10s"));
        for back in [
            KeyCode::Esc,
            KeyCode::Left,
            KeyCode::Char('h'),
            KeyCode::Backspace,
        ] {
            press(&mut monitor, KeyCode::Char('l'));
            assert_eq!(monitor.ui.screen, Screen::Timeline);
            press(&mut monitor, back);
            assert_eq!(monitor.ui.screen, Screen::List, "{back:?}");
        }
        press(&mut monitor, KeyCode::Right);
        assert_eq!(monitor.ui.screen, Screen::Timeline);
        Ok(())
    }

    #[test]
    fn key_releases_are_ignored_and_q_closes_help_and_quits() -> Result<()> {
        let mut monitor = fleet()?;
        monitor.frame(80, 24, 0);
        let release = KeyEvent::press(KeyCode::Char('j')).kind(KeyKind::Release);
        monitor.event(&Event::Key(release));
        assert_eq!(selected(&monitor), Some((0, "builder".into())));
        monitor.event(&Event::Key(
            KeyEvent::press(KeyCode::Char('q')).kind(KeyKind::Release),
        ));
        press(&mut monitor, KeyCode::Char('?'));
        assert!(monitor.ui.help);
        assert!(
            press(&mut monitor, KeyCode::Char('q')),
            "q quits from the help"
        );
        Ok(())
    }

    #[test]
    fn shift_tab_jumps_back_and_hosts_without_seats_are_skipped() -> Result<()> {
        let mut monitor = Monitor::new(
            vec!["a-host".into(), "empty".into(), "c-host".into()],
            Palette::DARK,
            0,
        );
        monitor.now_ms = NOW;
        monitor.hosts[0].apply(
            page_in("e", vec![record_run("one", 1, "working", NOW, "r")?], None)?,
            0,
        );
        monitor.hosts[2].apply(
            page_in("e", vec![record_run("two", 1, "working", NOW, "r")?], None)?,
            0,
        );
        press(&mut monitor, KeyCode::Tab);
        assert_eq!(
            selected(&monitor),
            Some((2, "two".into())),
            "Tab skips the empty host"
        );
        let back = KeyEvent::press(KeyCode::Tab).modifiers(lunatui_term::Modifiers::new().shift());
        monitor.event(&Event::Key(back.clone()));
        assert_eq!(selected(&monitor), Some((0, "one".into())));
        monitor.event(&Event::Key(back));
        assert_eq!(
            selected(&monitor),
            Some((2, "two".into())),
            "Shift-Tab wraps backwards"
        );
        Ok(())
    }

    #[test]
    fn a_fade_blends_toward_the_ground_color() {
        // Dark ground is #181b21; waiting is #f2b84b.
        assert_eq!(
            blend(&Palette::DARK, Palette::DARK.waiting, 1.0),
            Palette::DARK.waiting
        );
        assert_eq!(
            blend(&Palette::DARK, Palette::DARK.waiting, 0.0),
            Color::Rgb(0x18, 0x1b, 0x21)
        );
        assert_eq!(
            blend(&Palette::DARK, Palette::DARK.waiting, 0.5),
            Color::Rgb(133, 106, 54)
        );
        assert_eq!(blend(&Palette::DARK, Color::Default, 0.5), Color::Default);
    }

    #[test]
    fn the_selected_seat_is_bold_and_marked_with_the_focus_color() -> Result<()> {
        let mut monitor = fleet()?;
        let (text, grid, _) = draw(&mut monitor, 80, 24);
        let y = text
            .lines()
            .position(|line| line.contains("builder"))
            .unwrap() as u16;
        let x = column_of(text.lines().nth(usize::from(y)).unwrap(), "builder");
        assert!(grid.get(x, y).unwrap().style().attrs.bold());
        assert_eq!(grid.get(0, y).unwrap().style().fg, Palette::DARK.focus);
        assert_eq!(grid.get(x, y).unwrap().style().bg, Palette::DARK.selection);
        let other = text
            .lines()
            .position(|line| line.contains("reviewer"))
            .unwrap() as u16;
        assert!(!grid.get(x, other).unwrap().style().attrs.bold());
        Ok(())
    }

    #[test]
    fn the_light_palette_uses_the_graphite_light_tokens() {
        let light = Palette::LIGHT;
        assert_eq!(light.working, Color::Rgb(0x1f, 0x8a, 0x55));
        assert_eq!(light.waiting, Color::Rgb(0xb8, 0x7a, 0x0a));
        assert_eq!(light.failed, Color::Rgb(0xc2, 0x39, 0x2b));
        assert_eq!(Palette::DARK.working, Color::Rgb(0x4c, 0xc9, 0x8a));
        assert!(!light.dark && Palette::DARK.dark);
    }

    #[test]
    fn a_lone_host_with_the_feed_off_says_it_cannot_be_enabled_here_yet() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        monitor.hosts[0].link = Link::Disabled;
        let (text, ..) = draw(&mut monitor, 80, 24);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let title = lines
            .iter()
            .position(|l| l.trim() == "Activity feed is disabled for this daemon.")
            .unwrap();
        assert!(
            title > 4 && title < 14,
            "roughly centered: line {title}\n{text}"
        );
        assert!(text.contains("experimental_activity = true under [mux]"));
        assert!(!text.contains("not available yet"), "{text}");
        assert!(lines[1].trim_start().starts_with("mac  feed off"));
        // At a small size it still wraps inside the pane.
        let (small, ..) = draw(&mut monitor, 30, 12);
        assert!(small.contains("Activity feed is disabled"));
        assert!(small.lines().all(|l| l.trim_end().chars().count() <= 30));
        assert!(small.contains("experimental_activity"), "{small}");
        Ok(())
    }

    #[test]
    fn a_lone_host_that_predates_the_feed_says_so() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        monitor.hosts[0].link = Link::Unsupported;
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(text.contains("This pmuxd has no activity feed."), "{text}");
        assert!(text.contains("older than the feed"));
        assert!(text.contains("mac  no activity feed"));
        let footer = text.lines().last().unwrap_or_default().trim();
        assert_eq!(footer, "q quit", "nothing to move through yet");
        assert!(!text.contains("Can't reach"));
        Ok(())
    }

    #[test]
    fn an_unreachable_lone_host_says_how_to_start_pmuxd() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        monitor.hosts[0].disconnect(NOW - 5_000);
        let (text, ..) = draw(&mut monitor, 120, 30);
        assert!(text.contains("Can't reach pmuxd."), "{text}");
        assert!(text.contains("run pmux up"));
        assert!(text.contains("mac  retrying for 5s"));
        assert!(!text.contains("Select a seat"), "the wide split is skipped");
        Ok(())
    }

    #[test]
    fn hints_are_only_for_a_lone_host_with_nothing_to_show() -> Result<()> {
        let mut two = Monitor::new(vec!["mac".into(), "windows".into()], Palette::DARK, 0);
        two.hosts[0].link = Link::Disabled;
        two.hosts[1].link = Link::Disabled;
        assert_eq!(hint(&two.hosts), None);
        let mut monitor = fleet()?;
        monitor.hosts.truncate(1);
        monitor.hosts[0].link = Link::Disabled;
        assert_eq!(hint(&monitor.hosts), None, "cached seats stay visible");
        let fresh = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        assert_eq!(hint(&fresh.hosts), None, "connecting is not a problem yet");
        Ok(())
    }

    #[test]
    fn wrapping_keeps_words_whole_and_lines_short() {
        assert_eq!(
            wrap("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        assert_eq!(
            wrap("--experimental-activity now", 10),
            vec!["--experimental-activity", "now"]
        );
    }

    #[test]
    fn tiers_change_at_the_documented_sizes() {
        assert_eq!(tier(23, 24), Tier::Tiny);
        assert_eq!(tier(80, 3), Tier::Tiny);
        assert_eq!(tier(24, 4), Tier::Small);
        assert_eq!(tier(39, 24), Tier::Small);
        assert_eq!(tier(40, 24), Tier::Narrow);
        assert_eq!(tier(71, 24), Tier::Narrow);
        assert_eq!(tier(72, 24), Tier::Medium);
        assert_eq!(tier(109, 40), Tier::Medium);
        assert_eq!(tier(110, 13), Tier::Medium);
        assert_eq!(tier(110, 14), Tier::Wide);
    }

    #[test]
    fn wide_layout_groups_seats_by_host_and_shows_the_selected_timeline() -> Result<()> {
        let mut monitor = fleet()?;
        let (text, ..) = draw(&mut monitor, 140, 30);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("Activity monitor"));
        assert!(
            lines[0].contains("● 2 working")
                && lines[0].contains("◆ 1 waiting")
                && lines[0].contains("✕ 1 failed")
        );
        let mac = lines
            .iter()
            .position(|l| l.contains("mac  connected"))
            .unwrap();
        let builder = lines.iter().position(|l| l.contains("✕ builder")).unwrap();
        let windows = lines
            .iter()
            .position(|l| l.contains("windows  retrying for 12s"))
            .unwrap();
        let win_agent = lines
            .iter()
            .position(|l| l.contains("● win-agent"))
            .unwrap();
        assert!(mac < builder && builder < windows && windows < win_agent);
        assert!(lines[mac].contains("3 seats") && lines[mac].contains("2 need you"));
        assert!(lines[windows].contains("cached"));
        // The first seat is selected, and its timeline is beside the list.
        assert!(lines[1].contains("builder on mac") && lines[1].contains("✕ failed for 2m"));
        let timeline_row = text
            .lines()
            .filter(|line| line.contains("test_failed"))
            .nth(1)
            .unwrap_or_default();
        assert!(timeline_row.contains("✕ test_failed"), "{text}");
        assert!(text.contains("3 events  1 task  1 failure  last 2m ago"));
        press(&mut monitor, KeyCode::End);
        let (single, ..) = draw(&mut monitor, 140, 30);
        assert!(
            single.contains("1 event  1 task  0 failures  last 19s ago"),
            "{single}"
        );
        Ok(())
    }

    #[test]
    fn timeline_groups_by_task_and_hides_diagnostic_hashes_by_default() -> Result<()> {
        let mut monitor = fleet()?;
        let seat = monitor.hosts[0]
            .seats
            .get_mut(&AgentId::new("worker-a").unwrap())
            .unwrap();
        for record in &mut seat.history {
            let (task, tool, file, command, duration_ms, exit_code) = match record.sequence {
                6 => ("PT-107", None, None, None, None, None),
                7 => (
                    "PT-108",
                    Some("Edit"),
                    Some("src/lib.rs"),
                    None,
                    Some(950),
                    Some(0),
                ),
                8 => (
                    "PT-108",
                    Some("Bash"),
                    None,
                    Some("cargo test --locked"),
                    Some(1250),
                    None,
                ),
                _ => continue,
            };
            record.event.detail = Some(prismattyc_core::activity::ActivityDetail {
                task: Some(task.into()),
                tool: tool.map(str::to_owned),
                file: file.map(str::to_owned),
                command: command.map(str::to_owned),
                duration_ms,
                exit_code,
                operation_id: Some(format!("op-{}", record.sequence)),
                model: None,
            });
        }
        press(&mut monitor, KeyCode::End);
        press(&mut monitor, KeyCode::Up);
        assert_eq!(
            monitor.ui.selected.as_ref().map(|(_, id)| id.as_str()),
            Some("worker-a")
        );
        press(&mut monitor, KeyCode::Enter);
        assert_eq!(monitor.ui.screen, Screen::Timeline);
        let (text, ..) = draw(&mut monitor, 140, 24);
        let started = text.find("command_started").unwrap();
        let edit = text.find("edit_finished").unwrap();
        let rule = text.find("── task PT-107").unwrap();
        let turn = text.find("turn_started").unwrap();
        assert!(started < edit && edit < rule && rule < turn, "{text}");
        assert!(text.contains("cargo test --locked"), "{text}");
        assert!(text.contains("src/lib.rs"), "{text}");
        assert!(
            !text.contains("run run-cccc") && !text.contains("ev cccc"),
            "{text}"
        );
        let row = line_of(&text, "command_started");
        let (_, grid, _) = draw(&mut monitor, 140, 24);
        let y = text.lines().position(|line| line == row).unwrap() as u16;
        let gap_x = column_of(row, "+5.0s");
        assert_eq!(
            grid.get(gap_x, y).unwrap().style().fg,
            Color::Rgb(0x7d, 0x85, 0x94),
            "gaps are quieter than muted text"
        );
        assert!(
            row.contains("4s")
                && row.contains("+5.0s")
                && row.contains("hooks")
                && row.contains("Bash")
                && row.contains("#8")
        );
        press(&mut monitor, KeyCode::Char('i'));
        let (with_ids, ..) = draw(&mut monitor, 140, 24);
        assert!(
            with_ids.contains("run run-cccc") && with_ids.contains("ev event-8"),
            "{with_ids}"
        );
        press(&mut monitor, KeyCode::Esc);
        assert_eq!(monitor.ui.screen, Screen::List);
        Ok(())
    }

    #[test]
    fn narrow_panes_drop_columns_from_the_right() -> Result<()> {
        let mut monitor = fleet()?;
        let (medium, ..) = draw(&mut monitor, 80, 24);
        assert!(line_of(&medium, "worker-a").contains("command_started"));
        assert!(line_of(&medium, "win-agent").contains("stale"));
        let (narrow, ..) = draw(&mut monitor, 50, 12);
        assert!(line_of(&narrow, "worker-a").contains("command_started"));
        assert!(
            !line_of(&narrow, "win-agent").contains("stale"),
            "no flag column below 72"
        );
        let (small, ..) = draw(&mut monitor, 30, 10);
        let row = line_of(&small, "worker-a");
        assert!(!row.contains("command"), "{row}");
        assert!(row.trim_end().ends_with("4s"), "{row}");
        assert!(small.lines().next().unwrap().contains("●2"));
        let (tiny, ..) = draw(&mut monitor, 20, 3);
        assert!(tiny.lines().next().unwrap().contains("●2"));
        Ok(())
    }

    #[test]
    fn stale_ages_are_amber_and_fresh_ages_are_muted() -> Result<()> {
        let mut monitor = fleet()?;
        let (text, grid, _) = draw(&mut monitor, 80, 24);
        let row = line_of(&text, "reviewer");
        let y = text.lines().position(|line| line == row).unwrap() as u16;
        let x = column_of(row, "41s");
        assert_eq!(grid.get(x, y).unwrap().style().fg, Palette::DARK.waiting);
        let row = line_of(&text, "worker-a");
        let y = text.lines().position(|line| line == row).unwrap() as u16;
        let x = column_of(row, "4s ");
        assert_eq!(grid.get(x, y).unwrap().style().fg, Palette::DARK.muted);
        Ok(())
    }

    /// The fleet plus a seat whose hooks report safe details for two tasks.
    fn fleet_with_details() -> Result<Monitor> {
        use crate::model::tests::record_detail;
        use serde_json::json;
        let mut monitor = fleet()?;
        let command =
            |seq, kind, at, detail| record_detail("coder", seq, kind, at, "run-eeeeeeee1", detail);
        monitor.hosts[0].apply(
            page_in(
                "e1",
                vec![
                    command(
                        9,
                        "edit_started",
                        NOW - 80_000,
                        json!({"tool": "Edit", "file": "src/ui.rs", "task": "TASK-12", "operation_id": "op-edit"}),
                    )?,
                    command(
                        10,
                        "edit_finished",
                        NOW - 78_800,
                        json!({"operation_id": "op-edit"}),
                    )?,
                    command(
                        11,
                        "command_started",
                        NOW - 50_000,
                        json!({"tool": "Bash", "command": "cargo test -p pmux-activity-monitor-plugin", "operation_id": "op-test"}),
                    )?,
                    command(
                        12,
                        "command_failed",
                        NOW - 16_000,
                        json!({"exit_code": 101, "duration_ms": 34_000, "operation_id": "op-test"}),
                    )?,
                    record_detail(
                        "coder",
                        13,
                        "tool_started",
                        NOW - 3_000,
                        "run-eeeeeeee2",
                        json!({"tool": "Read", "file": "docs/plan.md", "task": "TASK-31", "operation_id": "op-read"}),
                    )?,
                ],
                None,
            )?,
            0,
        );
        Ok(monitor)
    }

    fn open_details(monitor: &mut Monitor, seat: &str) {
        monitor.ui.selected = Some((0, AgentId::new(seat).unwrap()));
        press_char(monitor, 'd');
    }

    #[test]
    fn seat_details_group_activities_by_task_with_results() -> Result<()> {
        let mut monitor = fleet_with_details()?;
        open_details(&mut monitor, "coder");
        let (text, ..) = draw(&mut monitor, 100, 24);
        assert!(text.contains("coder on mac"), "{text}");
        // Header facts: unknown until pmux reports them, ticket from the latest task.
        assert!(text.contains("Harness unknown"), "{text}");
        assert!(text.contains("Model unknown"), "{text}");
        assert!(text.contains("Quota unknown"), "{text}");
        assert!(text.contains("Project unknown"), "{text}");
        assert!(text.contains("Ticket TASK-31"), "{text}");
        // The newest task first, each start and result on one line.
        let pt108 = text.find("TASK-31  1 activity, last 3s ago").expect(&text);
        let pt111 = text.find("TASK-12  2 activities, 1 failed").expect(&text);
        assert!(pt108 < pt111, "{text}");
        let read = line_of(&text, "docs/plan.md");
        assert!(read.contains("… tool") && read.contains("Read"), "{read}");
        assert!(read.trim_end().ends_with("running"), "{read}");
        let test = line_of(&text, "cargo test");
        assert!(
            test.contains("✗ command") && test.contains("Bash"),
            "{test}"
        );
        assert!(test.trim_end().ends_with("34s  exit 101"), "{test}");
        let edit = line_of(&text, "src/ui.rs");
        assert!(
            edit.contains("✓ edit") && edit.trim_end().ends_with("1.2s"),
            "{edit}"
        );
        // No raw kind names or hashes unless asked for.
        assert!(!text.contains("command_failed"), "{text}");
        assert!(!text.contains("run eeee"), "{text}");
        press(&mut monitor, KeyCode::Esc);
        assert_eq!(monitor.ui.screen, Screen::List);
        Ok(())
    }

    #[test]
    fn seat_details_header_shows_reported_seat_facts() -> Result<()> {
        let mut monitor = fleet_with_details()?;
        let seat = monitor.hosts[0]
            .seats
            .get_mut(&AgentId::new("coder").unwrap())
            .unwrap();
        seat.info.harness = Some("claude".into());
        seat.info.project = Some("Prismattyc".into());
        open_details(&mut monitor, "coder");
        let (text, ..) = draw(&mut monitor, 100, 24);
        assert!(text.contains("Harness claude"), "{text}");
        assert!(text.contains("Project Prismattyc"), "{text}");
        assert!(text.contains("Model unknown"), "{text}");
        assert!(text.contains("Quota unknown"), "{text}");
        Ok(())
    }

    #[test]
    fn seat_details_show_ids_only_when_toggled() -> Result<()> {
        let mut monitor = fleet_with_details()?;
        open_details(&mut monitor, "coder");
        press_char(&mut monitor, 'i');
        let (text, ..) = draw(&mut monitor, 120, 24);
        let test = line_of(&text, "cargo test");
        assert!(test.contains("exit 101  run "), "{test}");
        assert!(test.contains(" op "), "{test}");
        Ok(())
    }

    #[test]
    fn seat_details_without_detail_fields_still_read_as_activities() -> Result<()> {
        let mut monitor = fleet()?;
        open_details(&mut monitor, "builder");
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(text.contains("no ticket  1 activity, 1 failed"), "{text}");
        let test = line_of(&text, "✗ test");
        assert!(test.trim_end().ends_with("30s"), "{test}");
        assert!(text.contains("Ticket none reported"), "{text}");
        Ok(())
    }

    #[test]
    fn seat_details_scroll_and_fit_a_narrow_pane() -> Result<()> {
        let mut monitor = fleet_with_details()?;
        open_details(&mut monitor, "coder");
        let (text, ..) = draw(&mut monitor, 40, 12);
        assert!(text.contains("coder"), "{text}");
        assert!(
            text.lines().all(|line| line.chars().count() <= 40),
            "{text}"
        );
        let before = monitor.ui.timeline_offset;
        press(&mut monitor, KeyCode::Down);
        draw(&mut monitor, 40, 12);
        assert!(monitor.ui.timeline_offset >= before);
        press_char(&mut monitor, 'g');
        assert_eq!(monitor.ui.timeline_offset, 0);
        Ok(())
    }

    #[test]
    fn lanes_color_each_bin_by_the_state_at_that_time() -> Result<()> {
        let mut monitor = fleet()?;
        let (text, grid, _) = draw(&mut monitor, 100, 24);
        let row = line_of(&text, "builder");
        let y = text.lines().position(|line| line == row).unwrap() as u16;
        // The lane ends one column before the right edge, at "now", where builder is failed.
        let now_cell = grid.get(98, y).unwrap();
        assert_eq!(now_cell.style().fg, Palette::DARK.failed);
        let row = line_of(&text, "reviewer");
        let y = text.lines().position(|line| line == row).unwrap() as u16;
        assert_eq!(grid.get(98, y).unwrap().style().fg, Palette::DARK.waiting);
        // The host lane carries volume only, in the hairline color, not a state color.
        let y = text
            .lines()
            .position(|line| line.contains("mac  connected"))
            .unwrap() as u16;
        let host_cell = grid.get(98, y).unwrap();
        assert_eq!(host_cell.style().fg, Palette::DARK.hairline);
        assert_ne!(
            Some(host_cell.glyph()),
            lunatui_core::GlyphSlot::inline_scalar(' ')
        );
        Ok(())
    }

    #[test]
    fn keys_move_within_and_across_hosts() -> Result<()> {
        let mut monitor = fleet()?;
        let selected = |m: &Monitor| {
            m.ui.selected
                .as_ref()
                .map(|(h, id)| (*h, id.as_str().to_owned()))
        };
        monitor.frame(80, 24, 0);
        assert_eq!(selected(&monitor), Some((0, "builder".into())));
        press(&mut monitor, KeyCode::Char('j'));
        assert_eq!(selected(&monitor), Some((0, "reviewer".into())));
        press(&mut monitor, KeyCode::Tab);
        assert_eq!(selected(&monitor), Some((1, "win-agent".into())));
        press(&mut monitor, KeyCode::Tab);
        assert_eq!(
            selected(&monitor),
            Some((0, "builder".into())),
            "Tab wraps to the first host"
        );
        press(&mut monitor, KeyCode::Char('G'));
        assert_eq!(selected(&monitor), Some((1, "win-agent".into())));
        press(&mut monitor, KeyCode::Down);
        assert_eq!(
            selected(&monitor),
            Some((1, "win-agent".into())),
            "stops at the last seat"
        );
        assert!(press(&mut monitor, KeyCode::Char('q')));
        Ok(())
    }

    #[test]
    fn the_attention_filter_keeps_waiting_failed_and_stale_working_seats() -> Result<()> {
        let mut monitor = fleet()?;
        press(&mut monitor, KeyCode::Char('f'));
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(
            text.contains("builder") && text.contains("reviewer") && text.contains("win-agent")
        );
        assert!(!text.contains("worker-a"));
        assert!(
            text.lines().next().unwrap().contains("needs attention"),
            "{text}"
        );
        press(&mut monitor, KeyCode::Char('f'));
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(
            text.contains("worker-a") && text.contains("win-agent") && !text.contains("builder")
        );
        Ok(())
    }

    #[test]
    fn sorting_by_state_puts_failures_first_within_each_host() -> Result<()> {
        let mut monitor = fleet()?;
        let order = |text: &str| -> Vec<usize> {
            ["builder", "reviewer", "worker-a"]
                .iter()
                .map(|n| text.find(n).unwrap())
                .collect()
        };
        press(&mut monitor, KeyCode::Char('s'));
        let (recent, ..) = draw(&mut monitor, 80, 24);
        assert!(
            recent.lines().next().unwrap().contains("most recent"),
            "{recent}"
        );
        let at = order(&recent);
        assert!(at[2] < at[1] && at[1] < at[0], "most recent first");
        // Make name order differ from state order: builder works, worker-a fails.
        monitor.hosts[0].apply(
            page_in(
                "e1",
                vec![
                    record_run("builder", 20, "turn_started", NOW - 1_000, "run-aaaaaaaa1")?,
                    record_run(
                        "worker-a",
                        21,
                        "command_failed",
                        NOW - 1_000,
                        "run-cccccccc2",
                    )?,
                ],
                None,
            )?,
            0,
        );
        press(&mut monitor, KeyCode::Char('s'));
        let (state, ..) = draw(&mut monitor, 80, 24);
        assert!(
            state.lines().next().unwrap().contains("by state"),
            "{state}"
        );
        let at = order(&state);
        assert!(
            at[2] < at[1] && at[1] < at[0],
            "failed, waiting, working: {state}"
        );
        Ok(())
    }

    #[test]
    fn a_state_change_fades_from_a_tint_back_to_the_pane_background() -> Result<()> {
        let mut monitor = fleet()?;
        monitor.hosts[0].apply(
            page_in(
                "e1",
                vec![record_run(
                    "worker-a",
                    9,
                    "waiting",
                    NOW - 1_000,
                    "run-cccccccc2",
                )?],
                None,
            )?,
            5_000,
        );
        let row_bg = |monitor: &mut Monitor, mono: u64| {
            let (grid, clusters) = monitor.frame(80, 24, mono);
            let text = grid.display_text(&clusters);
            let y = text
                .lines()
                .position(|line| line.contains("worker-a"))
                .unwrap() as u16;
            grid.get(40, y).unwrap().style().bg
        };
        assert!(animating(&monitor.hosts, &monitor.ui, 5_000));
        let start = row_bg(&mut monitor, 5_000);
        let mid = row_bg(&mut monitor, 5_450);
        let end = row_bg(&mut monitor, 5_000 + FADE_MS);
        assert_eq!(start, blend(&Palette::DARK, Palette::DARK.waiting, 0.38));
        assert_ne!(mid, start);
        assert_ne!(mid, Color::Default);
        assert_eq!(end, Color::Default);
        assert!(!animating(&monitor.hosts, &monitor.ui, 5_000 + FADE_MS));
        monitor.ui.motion = false;
        assert_eq!(row_bg(&mut monitor, 5_000), Color::Default);
        Ok(())
    }

    #[test]
    fn the_selection_stays_visible_when_the_pane_shrinks() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        let records = (1..=30)
            .map(|n| record_run(&format!("seat-{n:02}"), n, "working", NOW - 1_000, "run-1"))
            .collect::<Result<Vec<_>>>()?;
        monitor.hosts[0].apply(page_in("e", records, None)?, 0);
        monitor.frame(120, 40, 0);
        press(&mut monitor, KeyCode::End);
        let (tall, ..) = draw(&mut monitor, 120, 40);
        assert!(tall.contains("seat-30"));
        let (short, ..) = draw(&mut monitor, 60, 10);
        assert!(short.contains("seat-30"), "{short}");
        assert!(!short.contains("seat-01"));
        Ok(())
    }

    #[test]
    fn empty_and_unavailable_feeds_explain_what_to_do() -> Result<()> {
        let mut monitor = Monitor::new(vec!["mac".into(), "windows".into()], Palette::DARK, 0);
        monitor.now_ms = NOW;
        monitor.hosts[0].apply(page_in("e", Vec::new(), None)?, 0);
        monitor.hosts[1].link = Link::Disabled;
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(text.contains("No activity yet. Seats appear when their hooks publish."));
        assert!(line_of(&text, "windows").contains("feed off"));
        assert!(text.contains("Activity feed is disabled for this daemon."));
        Ok(())
    }

    #[test]
    fn help_lists_the_keys_and_any_key_closes_it() -> Result<()> {
        let mut monitor = fleet()?;
        press(&mut monitor, KeyCode::Char('?'));
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(text.contains("Keys") && text.contains("next host (Shift-Tab: previous)"));
        assert!(!press(&mut monitor, KeyCode::Char('x')));
        let (text, ..) = draw(&mut monitor, 80, 24);
        assert!(!text.contains("next host (Shift-Tab"));
        Ok(())
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let mut monitor = Monitor::new(vec!["mac".into()], Palette::DARK, 0);
        monitor.ui.help = true;
        let key =
            KeyEvent::press(KeyCode::Char('c')).modifiers(lunatui_term::Modifiers::new().control());
        assert!(monitor.event(&Event::Key(key)));
    }
}
