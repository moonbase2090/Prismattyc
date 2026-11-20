// SPDX-License-Identifier: MIT
//! The Update and restart confirmation: a modal with the version change, the
//! three choices, and the running sessions folded behind Show and Hide.
use super::*;
use graphite::{Face, Rect};

/// The choices, their hints, and what each one does.
const OPTIONS: [(&str, &str, update_flow::Action); 3] = [
    (
        "Save and restart",
        "Recreate sessions afterwards",
        update_flow::Action::SaveAndContinue,
    ),
    (
        "Restart without saving",
        "Sessions are discarded",
        update_flow::Action::Continue,
    ),
    ("Cancel", "Nothing changes now", update_flow::Action::Cancel),
];
/// Selection index of the Show and Hide toggle, after the three choices.
const TOGGLE: usize = OPTIONS.len();
const EXPLANATION: &str = "Save the session configuration to recreate your sessions after restart. Unsaved work inside programs is not saved.";
const SAVED: &str = "names, directories, launch commands, pane layouts";
const KEYS: &str = "Enter select · Esc close · ↑↓ move";
/// The choices after an update fails.
const FAILED_OPTIONS: [(&str, &str); 2] = [
    ("Check again", "Run the update check again"),
    ("Close", "Keep the installed version"),
];
const WORKING: &str = "Please wait. The app restarts when the update finishes.";

/// The update running in the dialog after a restart choice.
struct Running {
    meter: update_meter::Meter,
    /// Set when the update stopped; the meter holds the failed step.
    failure: Option<String>,
}

pub(super) struct RestartModal {
    /// The installed version, and the release that replaces it when one does.
    installed: String,
    available: Option<String>,
    sessions: Vec<String>,
    expanded: bool,
    selected: usize,
    chosen: Option<update_flow::Action>,
    hits: Hits,
    running: Option<Running>,
    /// Close was chosen after a failure.
    dismissed: bool,
}

#[derive(Clone, Copy, Default)]
struct Hits {
    options: [Option<Rect>; 3],
    toggle: Option<Rect>,
}

impl RestartModal {
    pub(super) fn new(installed: &str, available: Option<&str>, sessions: Vec<String>) -> Self {
        RestartModal {
            installed: installed.into(),
            available: available.map(Into::into),
            sessions,
            expanded: false,
            selected: 0,
            chosen: None,
            hits: Hits::default(),
            running: None,
            dismissed: false,
        }
    }

    fn failure(&self) -> Option<&str> {
        self.running.as_ref()?.failure.as_deref()
    }

    fn title(&self) -> &'static str {
        match &self.running {
            Some(Running {
                failure: Some(_), ..
            }) => return "Update did not finish",
            Some(_) if self.available.is_some() => return "Updating",
            Some(_) => return "Restarting components",
            None => {}
        }
        if self.available.is_some() {
            "Restart to update"
        } else {
            "Restart components"
        }
    }

    fn version(&self) -> String {
        match &self.available {
            Some(available) => format!("{} → {available}", self.installed),
            None => self.installed.clone(),
        }
    }

    fn summary(&self) -> String {
        match self.sessions.len() {
            0 => "No sessions are running".into(),
            1 => format!("1 session · {SAVED}"),
            n => format!("{n} sessions · {SAVED}"),
        }
    }

    fn toggle_label(&self) -> Option<&'static str> {
        if self.sessions.is_empty() {
            None
        } else if self.expanded {
            Some("Hide")
        } else {
            Some("Show")
        }
    }

    fn last(&self) -> usize {
        if self.sessions.is_empty() {
            TOGGLE - 1
        } else {
            TOGGLE
        }
    }

    /// Act on the selected row: choose an option or fold the session list.
    fn activate(&mut self, index: usize) {
        if self.running.is_some() {
            // Only the choices after a failure act; a running update has none.
            match (self.failure().is_some(), index) {
                (true, 0) => self.chosen = Some(update_flow::Action::Start),
                (true, 1) => self.dismissed = true,
                _ => {}
            }
            return;
        }
        if index == TOGGLE {
            self.expanded = !self.expanded;
        } else if let Some((_, _, action)) = OPTIONS.get(index) {
            self.chosen = Some(*action);
        }
    }

    fn key(&mut self, key: &Key) {
        if self.running.is_some() {
            if self.failure().is_none() {
                return;
            }
            match key {
                Key::Named(NamedKey::ArrowUp | NamedKey::Home) => self.selected = 0,
                Key::Named(NamedKey::ArrowDown | NamedKey::Tab | NamedKey::End) => {
                    self.selected = 1
                }
                Key::Named(NamedKey::Enter | NamedKey::Space) => self.activate(self.selected),
                Key::Named(NamedKey::Escape) => self.dismissed = true,
                _ => {}
            }
            return;
        }
        match key {
            Key::Named(NamedKey::ArrowUp) => self.selected = self.selected.saturating_sub(1),
            Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::Tab) => {
                self.selected = (self.selected + 1).min(self.last());
            }
            Key::Named(NamedKey::Home) => self.selected = 0,
            Key::Named(NamedKey::End) => self.selected = self.last(),
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                self.activate(self.selected)
            }
            Key::Named(NamedKey::Escape) => self.chosen = Some(update_flow::Action::Cancel),
            _ => {}
        }
    }

    fn hit(&self, x: usize, y: usize) -> Option<usize> {
        let inside =
            |rect: &Rect| x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h;
        self.hits
            .options
            .iter()
            .position(|rect| rect.as_ref().is_some_and(inside))
            .or_else(|| {
                self.hits
                    .toggle
                    .as_ref()
                    .filter(|r| inside(r))
                    .map(|_| TOGGLE)
            })
    }

    /// Rows for render status and accessibility: the choices, then the toggle.
    pub(super) fn rows(&self) -> Vec<String> {
        if let Some(running) = &self.running {
            let mut rows = running.meter.lines();
            rows.push(running.meter.bar().1);
            match &running.failure {
                Some(error) => {
                    rows.extend(error.lines().map(str::to_string));
                    rows.extend(
                        FAILED_OPTIONS
                            .iter()
                            .map(|(label, hint)| format!("{label}: {hint}")),
                    );
                }
                None => {
                    rows.push(running.meter.note().into());
                    rows.push(WORKING.into());
                }
            }
            return rows;
        }
        let mut rows: Vec<String> = OPTIONS
            .iter()
            .map(|(label, hint, _)| format!("{label}: {hint}"))
            .collect();
        if let Some(toggle) = self.toggle_label() {
            rows.push(format!("{toggle}: {}", self.summary()));
        }
        rows
    }

    /// The dialog's state for the opt-in UI change lines (see `ui_events`):
    /// positions and states only, never row text or session names.
    pub(super) fn state(&self) -> serde_json::Value {
        let (state, step) = match &self.running {
            None => ("consent", None),
            Some(running) => (
                if running.failure.is_some() {
                    "failed"
                } else {
                    "running"
                },
                Some(running.meter.position()),
            ),
        };
        serde_json::json!({
            "state": state,
            "selected": self.selected,
            "expanded": self.expanded,
            "step": step,
        })
    }

    pub(super) fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "title": self.title(),
            "version": self.version(),
            "rows": self.rows(),
            "selected": self.selected,
            "expanded": self.expanded,
            "sessions_shown": if self.expanded { self.sessions.len() } else { 0 },
            "running": self.running.as_ref().map(|running| serde_json::json!({
                "meter": running.meter.lines(),
                "bar": running.meter.bar().1,
                "failed": running.failure.is_some(),
                "error": running.failure,
            })),
            // Physical pixels of the last frame, for pointer drivers.
            "option_rects": self.hits.options.iter().map(|r| r.map(|r| [r.x, r.y, r.w, r.h])).collect::<Vec<_>>(),
            "toggle_rect": self.hits.toggle.map(|r| [r.x, r.y, r.w, r.h]),
        })
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn title_text(&self) -> &'static str {
        self.title()
    }
}

/// Open the modal in every window once the update check has a plan.
pub(super) fn open(app: &mut App, installed: &str, available: Option<&str>, sessions: &[String]) {
    for host in app.windows.values_mut() {
        host.space_panel = None;
        host.context_menu = None;
        host.restart_modal = Some(RestartModal::new(installed, available, sessions.to_vec()));
        host.dirty = true;
        host.window.request_redraw();
    }
}

/// Turn the open dialog in every window into the progress of the update.
pub(super) fn show_progress(app: &mut App, meter: &update_meter::Meter) {
    for host in app.windows.values_mut() {
        if let Some(modal) = host.restart_modal.as_mut() {
            modal.running = Some(Running {
                meter: meter.clone(),
                failure: None,
            });
            modal.selected = 0;
            host.dirty = true;
            host.window.request_redraw();
        }
    }
}

/// Hold the meter at the failed step, with the error and what to do next.
pub(super) fn fail(app: &mut App, meter: &update_meter::Meter, error: &str) {
    for host in app.windows.values_mut() {
        if let Some(modal) = host.restart_modal.as_mut() {
            modal.running = Some(Running {
                meter: meter.clone(),
                failure: Some(error.into()),
            });
            modal.selected = 0;
            host.dirty = true;
            host.window.request_redraw();
        }
    }
}

/// Whether any window still shows the dialog, for flows that fall back to
/// the panel when it was closed.
pub(super) fn is_open(app: &App) -> bool {
    app.windows
        .values()
        .any(|host| host.restart_modal.is_some())
}

/// Close the modal in every window.
pub(super) fn close(app: &mut App) {
    for host in app.windows.values_mut() {
        if host.restart_modal.take().is_some() {
            host.dirty = true;
            host.window.request_redraw();
        }
    }
}

pub(super) fn take_action(host: &mut HostState) -> Option<update_flow::Action> {
    host.restart_modal.as_mut()?.chosen.take()
}

pub(super) fn dispatch_key(host: &mut HostState, key: &Key, repeat: bool) {
    if let Some(modal) = host.restart_modal.as_mut() {
        if !(repeat && matches!(key, Key::Named(NamedKey::Enter | NamedKey::Escape))) {
            modal.key(key);
        }
        if modal.dismissed {
            host.restart_modal = None;
        }
    }
    host.dirty = true;
    host.window.request_redraw();
}

/// The modal consumes pointer and IME input while it is up.
pub(super) fn handle_pointer(host: &mut HostState, event: &WindowEvent) -> bool {
    if host.restart_modal.is_none() {
        return false;
    }
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            host.pointer_px = Some((position.x, position.y));
            host.cursor_cell = None;
            if let Some(row) = hover_button(host) {
                host.restart_modal.as_mut().unwrap().selected = row;
                host.dirty = true;
            }
            sync_chrome_hover(host);
        }
        WindowEvent::CursorLeft { .. } => host.pointer_px = None,
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } => {
            if let Some(row) = hover_button(host) {
                let modal = host.restart_modal.as_mut().unwrap();
                modal.selected = row;
                modal.activate(row);
                if modal.dismissed {
                    host.restart_modal = None;
                }
                host.suppress_left_release = true;
                host.dirty = true;
            }
        }
        WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } | WindowEvent::Ime(_) => {}
        _ => return false,
    }
    host.window.request_redraw();
    true
}

/// The choice or toggle under the pointer, for selection and hover cursors.
pub(super) fn hover_button(host: &HostState) -> Option<usize> {
    let modal = host.restart_modal.as_ref()?;
    let (x, y) = host.pointer_px?;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    modal.hit(x as usize, y as usize)
}

/// Split `text` into lines no wider than `max` pixels.
fn wrap(face: Face, px: f32, text: &str, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if !line.is_empty() && graphite::text_width(face, px, &candidate) > max {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Lay names out left to right with `gap` pixels between them, wrapping at
/// `max`. Returns each line's names with their x offsets.
fn flow(face: Face, px: f32, names: &[String], gap: f32, max: f32) -> Vec<Vec<(f32, String)>> {
    let mut lines: Vec<Vec<(f32, String)>> = vec![];
    let mut x = 0.0;
    for name in names {
        let w = graphite::text_width(face, px, name);
        if lines.is_empty() || (x > 0.0 && x + w > max) {
            lines.push(vec![]);
            x = 0.0;
        }
        lines.last_mut().unwrap().push((x, name.clone()));
        x += w + gap;
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn text(
    buffer: &mut [u32],
    stride: usize,
    x: usize,
    center_y: usize,
    face: Face,
    px: f32,
    value: &str,
    color: [u8; 3],
    clip: (usize, usize),
) {
    graphite::draw_text(
        buffer,
        stride,
        x as f32,
        center_y as f32,
        face,
        px,
        value,
        color,
        clip.0,
        clip.1,
    );
}

pub(super) fn paint(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    if host.restart_modal.is_none() {
        return;
    }
    if host
        .restart_modal
        .as_ref()
        .is_some_and(|modal| modal.running.is_some())
    {
        paint_progress(host, buffer, width, height);
        return;
    }
    let chrome = host.mux.geom().chrome;
    let tok = graphite::theme_tokens(&host.theme);
    let accent = graphite::accent(&tok, focus_border_rgb(host.focus_border));
    let surface = host_overlay_surface(host);
    let modal = host.restart_modal.as_mut().unwrap();
    let s = |v: f32| chrome.px(v).max(1);
    let title_px = s(18.0) as f32;
    let body_px = s(13.5) as f32;
    let label_px = s(14.0) as f32;
    let small_px = s(12.0) as f32;
    let pad = s(24.0);
    let margin = s(16.0);
    let panel_w = width
        .saturating_sub(2 * margin)
        .min(s(740.0))
        .max(s(320.0).min(width));
    let content_w = panel_w.saturating_sub(2 * pad);
    let line_h = s(22.0);
    let row_h = s(40.0);
    let explanation = wrap(Face::Regular, body_px, EXPLANATION, content_w as f32);
    let names = if modal.expanded {
        flow(
            Face::Mono,
            body_px,
            &modal.sessions,
            s(14.0) as f32,
            content_w as f32,
        )
    } else {
        vec![]
    };
    // Title, explanation, divider, options, divider, footer, names, key hints.
    let top_h = pad + s(26.0) + s(8.0) + explanation.len() * line_h + pad;
    let options_h = s(8.0) + OPTIONS.len() * (row_h + s(2.0)) + s(8.0);
    let mut footer_h = pad + line_h;
    if !names.is_empty() {
        footer_h += s(8.0) + names.len() * line_h;
    }
    footer_h += s(14.0) + line_h + s(16.0);
    let panel_h = (top_h + options_h + footer_h).min(height.saturating_sub(2 * margin));
    let x = width.saturating_sub(panel_w) / 2;
    let y = height.saturating_sub(panel_h) / 2;
    let right = x + panel_w;
    let bottom = y + panel_h;
    let clip = (x + pad, right.saturating_sub(pad));
    graphite_overlays::modal_panel(
        buffer,
        width,
        Rect::new(x, y, panel_w, panel_h),
        &tok,
        surface,
    );

    let mut cy = y + pad + s(13.0);
    text(
        buffer,
        width,
        x + pad,
        cy,
        Face::SemiBold,
        title_px,
        modal.title(),
        tok.text_strong,
        clip,
    );
    let title_w = graphite::text_width(Face::SemiBold, title_px, modal.title()).ceil() as usize;
    text(
        buffer,
        width,
        x + pad + title_w + s(14.0),
        cy,
        Face::Mono,
        body_px,
        &modal.version(),
        tok.muted,
        clip,
    );
    cy += s(13.0) + s(8.0) + line_h / 2;
    for line in &explanation {
        text(
            buffer,
            width,
            x + pad,
            cy,
            Face::Regular,
            body_px,
            line,
            tok.muted,
            clip,
        );
        cy += line_h;
    }
    let mut ry = y + top_h;
    graphite::fill_round_rect(
        buffer,
        width,
        Rect::new(x + 1, ry, panel_w.saturating_sub(2), 1),
        0.0,
        tok.hairline,
        255,
    );
    ry += s(8.0);
    let mut hits = Hits::default();
    for (index, (label, hint, _)) in OPTIONS.iter().enumerate() {
        let row = Rect::new(x + s(8.0), ry, panel_w.saturating_sub(s(16.0)), row_h);
        if ry + row_h > bottom {
            break;
        }
        if modal.selected == index {
            graphite::fill_round_rect(buffer, width, row, s(6.0) as f32, tok.tab_hover, 255);
            graphite::fill_round_rect(
                buffer,
                width,
                Rect::new(row.x, row.y, s(3.0), row.h),
                s(2.0) as f32,
                accent,
                255,
            );
        }
        let mid = row.y + row.h / 2;
        text(
            buffer,
            width,
            x + pad,
            mid,
            Face::Regular,
            label_px,
            label,
            tok.text_strong,
            clip,
        );
        let hint_w = graphite::text_width(Face::Regular, body_px, hint).ceil() as usize;
        text(
            buffer,
            width,
            right.saturating_sub(pad + hint_w),
            mid,
            Face::Regular,
            body_px,
            hint,
            tok.muted,
            clip,
        );
        hits.options[index] = Some(row);
        ry += row_h + s(2.0);
    }
    let fy = y + top_h + options_h;
    if fy < bottom {
        graphite::fill_round_rect(
            buffer,
            width,
            Rect::new(x + 1, fy, panel_w.saturating_sub(2), 1),
            0.0,
            tok.hairline,
            255,
        );
    }
    let mut cy = fy + pad + line_h / 2 - s(6.0);
    if cy < bottom {
        text(
            buffer,
            width,
            x + pad,
            cy,
            Face::Regular,
            body_px,
            &modal.summary(),
            tok.muted,
            clip,
        );
    }
    if let Some(label) = modal.toggle_label() {
        let w = graphite::text_width(Face::Regular, body_px, label).ceil() as usize;
        let lx = right.saturating_sub(pad + w);
        let link = Rect::new(
            lx.saturating_sub(s(6.0)),
            cy.saturating_sub(line_h / 2),
            w + s(12.0),
            line_h,
        );
        if modal.selected == TOGGLE {
            graphite::fill_round_rect(buffer, width, link, s(5.0) as f32, tok.tab_hover, 255);
        }
        text(
            buffer,
            width,
            lx,
            cy,
            Face::Regular,
            body_px,
            label,
            accent,
            (clip.0, right),
        );
        hits.toggle = Some(link);
    }
    cy += s(8.0);
    for line in &names {
        cy += line_h;
        if cy + line_h / 2 > bottom.saturating_sub(s(30.0)) {
            break;
        }
        for (offset, name) in line {
            text(
                buffer,
                width,
                x + pad + *offset as usize,
                cy,
                Face::Mono,
                body_px,
                name,
                tok.text,
                clip,
            );
        }
    }
    let ky = bottom.saturating_sub(s(16.0) + line_h / 2);
    text(
        buffer,
        width,
        x + pad,
        ky,
        Face::Regular,
        small_px,
        KEYS,
        tok.muted,
        clip,
    );
    modal.hits = hits;
}

/// The dialog while the update runs: the step, its bar and the note; after a
/// failure, the held step, the error and the two choices.
fn paint_progress(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    let chrome = host.mux.geom().chrome;
    let tok = graphite::theme_tokens(&host.theme);
    let accent = graphite::accent(&tok, focus_border_rgb(host.focus_border));
    let surface = host_overlay_surface(host);
    let modal = host.restart_modal.as_mut().unwrap();
    let Some(running) = modal.running.as_ref() else {
        return;
    };
    let s = |v: f32| chrome.px(v).max(1);
    let title_px = s(18.0) as f32;
    let body_px = s(13.5) as f32;
    let label_px = s(14.0) as f32;
    let small_px = s(12.0) as f32;
    let pad = s(24.0);
    let margin = s(16.0);
    let panel_w = width
        .saturating_sub(2 * margin)
        .min(s(740.0))
        .max(s(320.0).min(width));
    let content_w = panel_w.saturating_sub(2 * pad);
    let line_h = s(22.0);
    let row_h = s(40.0);
    let lines = running.meter.lines();
    let (bar, label) = running.meter.bar();
    let note = wrap(
        Face::Regular,
        body_px,
        running.meter.note(),
        content_w as f32,
    );
    let error: Vec<String> = running
        .failure
        .as_deref()
        .map(|error| {
            error
                .lines()
                .flat_map(|line| wrap(Face::Regular, body_px, line, content_w as f32))
                .take(8)
                .collect()
        })
        .unwrap_or_default();
    let failed = running.failure.is_some();
    // Title, step, detail, bar, note or error, choices, key hints.
    let mut panel_h = pad + s(26.0) + s(14.0) + 3 * line_h + s(10.0);
    panel_h += if failed {
        error.len() * line_h + s(16.0) + FAILED_OPTIONS.len() * (row_h + s(2.0)) + s(8.0)
    } else {
        note.len() * line_h
    };
    panel_h += s(14.0) + line_h + s(16.0);
    let panel_h = panel_h.min(height.saturating_sub(2 * margin));
    let x = width.saturating_sub(panel_w) / 2;
    let y = height.saturating_sub(panel_h) / 2;
    let right = x + panel_w;
    let bottom = y + panel_h;
    let clip = (x + pad, right.saturating_sub(pad));
    graphite_overlays::modal_panel(
        buffer,
        width,
        Rect::new(x, y, panel_w, panel_h),
        &tok,
        surface,
    );
    let mut cy = y + pad + s(13.0);
    text(
        buffer,
        width,
        x + pad,
        cy,
        Face::SemiBold,
        title_px,
        modal.title(),
        tok.text_strong,
        clip,
    );
    let title_w = graphite::text_width(Face::SemiBold, title_px, modal.title()).ceil() as usize;
    text(
        buffer,
        width,
        x + pad + title_w + s(14.0),
        cy,
        Face::Mono,
        body_px,
        &modal.version(),
        tok.muted,
        clip,
    );
    cy += s(13.0) + s(14.0) + line_h / 2;
    text(
        buffer,
        width,
        x + pad,
        cy,
        Face::Regular,
        label_px,
        &lines[0],
        tok.text_strong,
        clip,
    );
    cy += line_h;
    text(
        buffer,
        width,
        x + pad,
        cy,
        Face::Regular,
        body_px,
        &lines[1],
        tok.muted,
        clip,
    );
    cy += line_h;
    let label_w = graphite::text_width(Face::Regular, body_px, &label).ceil() as usize;
    let track_w = content_w.saturating_sub(if label_w > 0 { label_w + s(12.0) } else { 0 });
    let track_h = s(6.0).max(3);
    let track = Rect::new(x + pad, cy.saturating_sub(track_h / 2), track_w, track_h);
    let radius = track_h as f32 / 2.0;
    graphite::fill_round_rect(buffer, width, track, radius, tok.tab_hover, 255);
    let (start, length) = update_meter::meter_fill(track_w, bar);
    let fill = if failed { tok.attention } else { accent };
    graphite::fill_round_rect(
        buffer,
        width,
        Rect::new(track.x + start, track.y, length, track_h),
        radius,
        fill,
        255,
    );
    if label_w > 0 {
        text(
            buffer,
            width,
            right.saturating_sub(pad + label_w),
            cy,
            Face::Regular,
            body_px,
            &label,
            tok.text,
            clip,
        );
    }
    cy += line_h + s(10.0);
    let mut hits = Hits::default();
    if failed {
        for line in &error {
            text(
                buffer,
                width,
                x + pad,
                cy,
                Face::Regular,
                body_px,
                line,
                tok.text,
                clip,
            );
            cy += line_h;
        }
        let mut ry = cy.saturating_sub(line_h / 2) + s(16.0);
        graphite::fill_round_rect(
            buffer,
            width,
            Rect::new(
                x + 1,
                ry.saturating_sub(s(8.0)),
                panel_w.saturating_sub(2),
                1,
            ),
            0.0,
            tok.hairline,
            255,
        );
        for (index, (option, hint)) in FAILED_OPTIONS.iter().enumerate() {
            let row = Rect::new(x + s(8.0), ry, panel_w.saturating_sub(s(16.0)), row_h);
            if ry + row_h > bottom {
                break;
            }
            if modal.selected == index {
                graphite::fill_round_rect(buffer, width, row, s(6.0) as f32, tok.tab_hover, 255);
                graphite::fill_round_rect(
                    buffer,
                    width,
                    Rect::new(row.x, row.y, s(3.0), row.h),
                    s(2.0) as f32,
                    accent,
                    255,
                );
            }
            let mid = row.y + row.h / 2;
            text(
                buffer,
                width,
                x + pad,
                mid,
                Face::Regular,
                label_px,
                option,
                tok.text_strong,
                clip,
            );
            let hint_w = graphite::text_width(Face::Regular, body_px, hint).ceil() as usize;
            text(
                buffer,
                width,
                right.saturating_sub(pad + hint_w),
                mid,
                Face::Regular,
                body_px,
                hint,
                tok.muted,
                clip,
            );
            hits.options[index] = Some(row);
            ry += row_h + s(2.0);
        }
    } else {
        for line in &note {
            text(
                buffer,
                width,
                x + pad,
                cy,
                Face::Regular,
                body_px,
                line,
                tok.muted,
                clip,
            );
            cy += line_h;
        }
    }
    let ky = bottom.saturating_sub(s(16.0) + line_h / 2);
    let keys = if failed { KEYS } else { WORKING };
    text(
        buffer,
        width,
        x + pad,
        ky,
        Face::Regular,
        small_px,
        keys,
        tok.muted,
        clip,
    );
    modal.hits = hits;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modal(sessions: usize) -> RestartModal {
        RestartModal::new(
            "0.3.69",
            Some("0.3.70"),
            (0..sessions).map(|i| format!("session-{i}")).collect(),
        )
    }

    #[test]
    fn the_version_change_and_the_three_choices_are_listed() {
        let m = modal(44);
        assert_eq!(m.title(), "Restart to update");
        assert_eq!(m.version(), "0.3.69 → 0.3.70");
        assert_eq!(
            m.rows(),
            [
                "Save and restart: Recreate sessions afterwards",
                "Restart without saving: Sessions are discarded",
                "Cancel: Nothing changes now",
                "Show: 44 sessions · names, directories, launch commands, pane layouts",
            ]
        );
    }

    #[test]
    fn a_restart_without_a_new_release_names_only_the_installed_version() {
        let m = RestartModal::new("0.3.69", None, vec!["a".into()]);
        assert_eq!(m.title(), "Restart components");
        assert_eq!(m.version(), "0.3.69");
        assert_eq!(
            m.summary(),
            "1 session · names, directories, launch commands, pane layouts"
        );
    }

    #[test]
    fn sessions_stay_hidden_until_the_toggle_is_used_and_it_folds_back() {
        let mut m = modal(3);
        assert!(!m.expanded);
        for _ in 0..3 {
            m.key(&Key::Named(NamedKey::ArrowDown));
        }
        assert_eq!(m.selected, TOGGLE);
        m.key(&Key::Named(NamedKey::Enter));
        assert!(m.expanded);
        assert!(m.chosen.is_none(), "the toggle chooses nothing");
        assert_eq!(
            m.rows()[3],
            "Hide: 3 sessions · names, directories, launch commands, pane layouts"
        );
        m.key(&Key::Named(NamedKey::Space));
        assert!(!m.expanded);
    }

    #[test]
    fn enter_chooses_the_selected_option_and_escape_cancels() {
        let mut m = modal(2);
        m.key(&Key::Named(NamedKey::Enter));
        assert!(matches!(
            m.chosen,
            Some(update_flow::Action::SaveAndContinue)
        ));
        let mut m = modal(2);
        m.key(&Key::Named(NamedKey::ArrowDown));
        m.key(&Key::Named(NamedKey::Enter));
        assert!(matches!(m.chosen, Some(update_flow::Action::Continue)));
        let mut m = modal(2);
        m.key(&Key::Named(NamedKey::Escape));
        assert!(matches!(m.chosen, Some(update_flow::Action::Cancel)));
    }

    #[test]
    fn selection_stops_at_the_ends_and_skips_the_toggle_without_sessions() {
        let mut m = modal(0);
        m.key(&Key::Named(NamedKey::ArrowUp));
        assert_eq!(m.selected, 0);
        for _ in 0..10 {
            m.key(&Key::Named(NamedKey::ArrowDown));
        }
        assert_eq!(m.selected, 2, "no toggle when nothing runs");
        assert_eq!(m.summary(), "No sessions are running");
        assert_eq!(m.rows().len(), 3);
    }

    #[test]
    fn clicks_land_on_the_row_or_the_toggle_they_hit() {
        let mut m = modal(5);
        m.hits.options[1] = Some(Rect::new(10, 100, 200, 40));
        m.hits.toggle = Some(Rect::new(300, 300, 50, 20));
        assert_eq!(m.hit(20, 120), Some(1));
        assert_eq!(m.hit(310, 310), Some(TOGGLE));
        assert_eq!(m.hit(5, 5), None);
        m.activate(TOGGLE);
        assert!(m.expanded, "a click on Show expands the list");
    }

    #[test]
    fn long_text_wraps_and_names_flow_across_lines() {
        let lines = wrap(Face::Regular, 13.0, EXPLANATION, 300.0);
        assert!(lines.len() > 1);
        assert!(lines
            .iter()
            .all(|line| graphite::text_width(Face::Regular, 13.0, line) <= 300.0));
        assert_eq!(lines.join(" "), EXPLANATION);
        let names: Vec<String> = (0..40).map(|i| format!("lunatui-{i}")).collect();
        let rows = flow(Face::Mono, 13.0, &names, 14.0, 400.0);
        assert!(rows.len() > 1);
        assert_eq!(
            rows.iter().map(Vec::len).sum::<usize>(),
            40,
            "every name shown"
        );
        for row in &rows {
            let (x, name) = row.last().unwrap();
            assert!(x + graphite::text_width(Face::Mono, 13.0, name) <= 400.0);
        }
    }

    fn running(m: &mut RestartModal, failure: Option<&str>) {
        let mut meter = update_meter::Meter::new(true);
        meter.apply(
            &prismattyc_mux::release_update::progress::Progress::Verify {
                item: "pmuxd".into(),
                index: 2,
                count: 6,
            },
        );
        if failure.is_some() {
            meter.fail();
        }
        m.running = Some(Running {
            meter,
            failure: failure.map(Into::into),
        });
        m.selected = 0;
    }

    #[test]
    fn a_running_update_shows_its_step_and_ignores_keys() {
        let mut m = modal(3);
        running(&mut m, None);
        assert_eq!(m.title(), "Updating");
        let rows = m.rows();
        assert_eq!(rows[0], "Step 2 of 4: Verify");
        assert_eq!(rows[1], "Verifying 2 of 6: pmuxd");
        assert_eq!(rows[2], "16%");
        assert!(rows.contains(&WORKING.to_string()));
        for key in [
            NamedKey::Enter,
            NamedKey::Escape,
            NamedKey::ArrowDown,
            NamedKey::Space,
        ] {
            m.key(&Key::Named(key));
        }
        assert!(
            m.chosen.is_none() && !m.dismissed,
            "nothing to choose while it runs"
        );
        m.activate(0);
        assert!(m.chosen.is_none());
    }

    #[test]
    fn a_failed_update_holds_the_step_and_offers_check_again_or_close() {
        let mut m = modal(3);
        running(
            &mut m,
            Some("download failed for pmuxd\ninstalled version unchanged"),
        );
        assert_eq!(m.title(), "Update did not finish");
        let rows = m.rows();
        assert_eq!(rows[0], "Failed at step 2 of 4: Verify");
        assert!(
            rows.contains(&"download failed for pmuxd".to_string()),
            "{rows:?}"
        );
        assert_eq!(
            &rows[rows.len() - 2..],
            [
                "Check again: Run the update check again",
                "Close: Keep the installed version"
            ]
        );
        m.key(&Key::Named(NamedKey::Enter));
        assert!(matches!(m.chosen, Some(update_flow::Action::Start)));

        let mut m = modal(3);
        running(&mut m, Some("x"));
        m.key(&Key::Named(NamedKey::ArrowDown));
        m.key(&Key::Named(NamedKey::Enter));
        assert!(m.dismissed && m.chosen.is_none(), "Close only closes");

        let mut m = modal(3);
        running(&mut m, Some("x"));
        m.key(&Key::Named(NamedKey::Escape));
        assert!(m.dismissed);
    }

    #[test]
    fn a_click_on_a_failed_update_choice_acts_on_it() {
        let mut m = modal(3);
        running(&mut m, Some("x"));
        m.hits.options[0] = Some(Rect::new(10, 100, 200, 40));
        m.hits.options[1] = Some(Rect::new(10, 142, 200, 40));
        assert_eq!(m.hit(20, 150), Some(1));
        m.activate(1);
        assert!(m.dismissed);
        let status = m.status();
        assert_eq!(status["running"]["failed"], true);
        assert_eq!(
            status["running"]["meter"][0],
            "Failed at step 2 of 4: Verify"
        );
    }
}
