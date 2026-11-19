// SPDX-License-Identifier: MIT
//! The Update and restart confirmation: a modal with the version change, the
//! three choices, and the running sessions folded behind Show and Hide.
use super::*;
use graphite::Rect;

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

pub(super) struct RestartModal {
    /// The installed version, and the release that replaces it when one does.
    installed: String,
    available: Option<String>,
    sessions: Vec<String>,
    expanded: bool,
    selected: usize,
    chosen: Option<update_flow::Action>,
    hits: Hits,
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
        }
    }

    fn title(&self) -> &'static str {
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
        if index == TOGGLE {
            self.expanded = !self.expanded;
        } else if let Some((_, _, action)) = OPTIONS.get(index) {
            self.chosen = Some(*action);
        }
    }

    fn key(&mut self, key: &Key) {
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
        let mut rows: Vec<String> = OPTIONS
            .iter()
            .map(|(label, hint, _)| format!("{label}: {hint}"))
            .collect();
        if let Some(toggle) = self.toggle_label() {
            rows.push(format!("{toggle}: {}", self.summary()));
        }
        rows
    }

    pub(super) fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "title": self.title(),
            "version": self.version(),
            "rows": self.rows(),
            "selected": self.selected,
            "expanded": self.expanded,
            "sessions_shown": if self.expanded { self.sessions.len() } else { 0 },
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

/// Draw the dialog: lunatui widgets render it into a cell grid (see
/// [`restart_modal_lunatui`]) and the grid is painted centred over the window.
pub(super) fn paint(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    if host.restart_modal.is_none() {
        return;
    }
    let tok = graphite::theme_tokens(&host.theme);
    let palette = restart_modal_lunatui::Palette {
        panel: tok.bar,
        border: tok.hairline,
        text: tok.text,
        strong: tok.text_strong,
        muted: tok.muted,
        accent: graphite::accent(&tok, focus_border_rgb(host.focus_border)),
        selected: tok.tab_hover,
    };
    let (cw, ch) = (host.font.cell_w.max(1), host.font.cell_h.max(1));
    let columns = u16::try_from((width / cw).saturating_sub(4).min(84)).unwrap_or(84);
    let modal = host.restart_modal.as_mut().unwrap();
    let options: Vec<(&str, &str)> = OPTIONS
        .iter()
        .map(|(label, hint, _)| (*label, *hint))
        .collect();
    let version = modal.version();
    let summary = modal.summary();
    let view = restart_modal_lunatui::View {
        title: modal.title(),
        version: &version,
        explanation: EXPLANATION,
        options: &options,
        selected: modal.selected,
        summary: &summary,
        toggle: modal.toggle_label(),
        expanded: modal.expanded,
        sessions: &modal.sessions,
        keys: KEYS,
    };
    let layout = restart_modal_lunatui::layout(&view, columns, palette);
    let grid_w = usize::from(layout.grid.width()) * cw;
    let grid_h = usize::from(layout.grid.height()) * ch;
    let x = width.saturating_sub(grid_w) / 2;
    let y = height.saturating_sub(grid_h) / 2;
    restart_modal_lunatui::paint(&layout, buffer, width, x, y, &host.font, palette);
    let rect = |cells| {
        let [x, y, w, h] = restart_modal_lunatui::pixels(cells, x, y, (cw, ch));
        Rect::new(x, y, w, h)
    };
    let mut hits = Hits::default();
    for (index, cells) in layout.options.iter().enumerate().take(3) {
        hits.options[index] = Some(rect(*cells));
    }
    hits.toggle = layout.toggle.map(rect);
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
}
