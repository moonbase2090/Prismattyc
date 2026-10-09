//! One startup decision before the registered host consumes its saved cache.

use super::*;

pub(super) struct RestorePrompt {
    layout: attach_tabs::AttachTabsFile,
    pub(super) selected: usize,
    automatic_at: Option<Instant>,
    startup: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl RestorePrompt {
    pub(super) fn new(layout: attach_tabs::AttachTabsFile) -> Option<Self> {
        layout
            .tabs
            .iter()
            .any(|tab| !tab.sessions.is_empty())
            .then_some(Self {
                layout,
                selected: 0,
                automatic_at: None,
                startup: None,
            })
    }

    pub(super) fn arm(&mut self, socket: PathBuf, pmux: PathBuf, wake: mux::Wake) {
        self.automatic_at = Some(Instant::now() + Duration::from_secs(3));
        let (sender, receiver) = std::sync::mpsc::channel();
        self.startup = Some(receiver);
        std::thread::spawn(move || {
            let result = prismattyc_mux::platform::hidden_command(pmux)
                .arg("--socket")
                .arg(socket)
                .arg("up")
                .output()
                .map_err(|error| error.to_string())
                .and_then(|output| {
                    if output.status.success() {
                        Ok(())
                    } else {
                        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
                    }
                });
            let _ = sender.send(result);
            wake();
        });
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.startup
            .is_none()
            .then_some(self.automatic_at)
            .flatten()
    }

    fn key(&mut self, key: &Key) -> Option<bool> {
        // Any deliberate navigation cancels the countdown.
        self.automatic_at = None;
        match key {
            Key::Named(NamedKey::Enter) => Some(self.selected == 0),
            Key::Named(NamedKey::Escape) => Some(false),
            Key::Named(
                NamedKey::ArrowUp
                | NamedKey::ArrowDown
                | NamedKey::Tab
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight,
            ) => {
                self.selected = 1 - self.selected;
                None
            }
            Key::Character(value)
                if value.eq_ignore_ascii_case("r") || value.eq_ignore_ascii_case("y") =>
            {
                Some(true)
            }
            Key::Character(value) if value.eq_ignore_ascii_case("n") => Some(false),
            _ => None,
        }
    }
}

pub(super) fn dispatch_key(host: &mut HostState, key: &Key, repeat: bool) {
    if repeat {
        return;
    }
    let decision = host
        .restore_prompt
        .as_mut()
        .and_then(|prompt| prompt.key(key));
    if let Some(accept) = decision {
        finish(host, accept);
    }
    host.dirty = true;
    host.window.request_redraw();
}

/// A modal choice consumes pointer and IME input before guest dispatch.
pub(super) fn handle_pointer(host: &mut HostState, event: &WindowEvent) -> bool {
    if host.restore_prompt.is_none() {
        return false;
    }
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            host.pointer_px = Some((position.x, position.y));
            host.cursor_cell = None;
            if let Some(row) = hit(host) {
                host.restore_prompt.as_mut().unwrap().selected = row;
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
            if let Some(row) = hit(host) {
                finish(host, row == 0);
                host.suppress_left_release = true;
            }
        }
        WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } | WindowEvent::Ime(_) => {}
        _ => return false,
    }
    host.window.request_redraw();
    true
}

fn hit(host: &HostState) -> Option<usize> {
    let (x, y) = host.pointer_px?;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    palette_hit(host.palette_layout.as_ref()?, x as usize, y as usize).filter(|row| *row < 2)
}

/// Button row under the pointer for hover cursors. Read-only: selection
/// follows the pointer in [`handle_pointer`]; this only reports the row.
pub(super) fn hover_button(host: &HostState) -> Option<usize> {
    host.restore_prompt.as_ref()?;
    hit(host)
}

pub(super) fn finish(host: &mut HostState, accept: bool) {
    if accept {
        if let Some(prompt) = host.restore_prompt.as_mut().filter(|p| p.startup.is_some()) {
            prompt.automatic_at = Some(Instant::now());
            return;
        }
    }
    let Some(prompt) = host.restore_prompt.take() else {
        return;
    };
    host.palette_layout = None;
    host.splash = None;
    if accept {
        let result = (|| -> Result<_> {
            let space = prompt
                .layout
                .space
                .as_deref()
                .map(|name| {
                    load_space(&spaces_dir(), name)
                        .with_context(|| format!("saved Space {name:?} is unavailable"))
                })
                .transpose()?;
            // No daemon means stopped seats. Restoring the layout must not
            // launch saved commands; Enter starts the daemon and one seat.
            let snapshot = attach_log::live_snapshot().unwrap_or(prismattyc_mux::Snapshot {
                sequence: 0,
                sessions: Vec::new(),
            });
            let (file, stopped) =
                space_view::restore_layout(&prompt.layout, space.as_ref(), &snapshot)?;
            let names = snapshot
                .sessions
                .iter()
                .filter(|s| {
                    file.space_id
                        .as_ref()
                        .is_none_or(|owner| s.space_id.as_ref() == Some(owner))
                })
                .map(|s| (s.id.to_string(), s.name.clone()))
                .collect();
            adopt_space_owner(host, file.space_id.clone());
            let before_bindings = host.attach_pane_sessions.clone();
            let applied = regroup::apply_with_placeholders(
                &mut host.mux,
                &mut host.attach_pane_sessions,
                &file,
                &find_mux_bin().to_string_lossy(),
                &names,
                &stopped,
            );
            crate::note_attach_bindings(host, &before_bindings);
            applied?;
            Ok(file)
        })();
        match result {
            Ok(file) => {
                set_current_space(host, file.space.clone());
                host.attach_layout = Some(file);
                host.layout_dirty = false;
            }
            Err(error) => {
                eprintln!("prismattyc-host: could not restore last space: {error:#}");
                rail_error_toast(host, &format!(" could not restore last space: {error:#} "));
            }
        }
    } else {
        host.local_views.fresh = true;
        set_current_space(host, None);
    }
    host.dirty = true;
    host.window
        .set_title(&window_title(&host.mux, show_tab_strip(host)));
    App::refit_geom(
        host,
        host.window.inner_size(),
        Some("startup restore choice"),
    );
    host.window.request_redraw();
}

pub(super) fn poll(host: &mut HostState) {
    let Some(prompt) = host.restore_prompt.as_mut() else {
        return;
    };
    let result = prompt.startup.as_ref().and_then(|rx| match rx.try_recv() {
        Ok(result) => Some(result),
        Err(std::sync::mpsc::TryRecvError::Empty) => None,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Some(Err("workspace startup worker stopped".into()))
        }
    });
    if let Some(result) = result {
        prompt.startup = None;
        if let Err(error) = result {
            prompt.automatic_at = None;
            rail_error_toast(host, &format!(" Could not start workspace: {error} "));
            return;
        }
    }
    if prompt.deadline().is_some_and(|at| Instant::now() >= at) {
        finish(host, true);
    }
}

pub(super) fn sync_login_registration() {
    let Some(socket) = host_mux_socket() else {
        return;
    };
    let pmux = find_mux_bin();
    std::thread::spawn(move || {
        match prismattyc_mux::platform::hidden_command(pmux)
            .arg("--socket")
            .arg(socket)
            .args(["login", "sync"])
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "prismattyc-host: login registration failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => eprintln!("prismattyc-host: login registration failed: {error}"),
        }
    });
}

pub(super) fn paint(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    let Some(prompt) = host.restore_prompt.as_ref() else {
        return;
    };
    let rows = [
        PaletteRow::plain(
            "Restore workspace".into(),
            if prompt.automatic_at.is_some() {
                "Restoring automatically. Esc starts fresh.".into()
            } else {
                "Reconnect saved sessions, tabs, panes, and Spaces.".into()
            },
            "R".into(),
        ),
        PaletteRow::plain(
            "Start fresh".into(),
            "Keep this fresh window.".into(),
            "Esc".into(),
        ),
    ];
    let sections = [PaletteSection {
        header: "Restore workspace",
        subtitle: prompt.layout.space.as_deref().unwrap_or("Previous window"),
        rows: &rows,
    }];
    let frame = PaletteFrame {
        layout_mode: PaletteLayoutMode::FixedHeight,
        query: None,
        query_focused: false,
        chips: None,
        sections: &sections,
        selected: prompt.selected,
        scroll: 0,
        detail: None,
        footer: "Enter choose · Esc start fresh · Startup preference: Spaces settings",
    };
    host.palette_layout = paint_palette_overlay(
        &host.font,
        &host.theme,
        host.mux.geom().chrome,
        focus_border_rgb(host.focus_border),
        &frame,
        host_overlay_surface(host),
        buffer,
        width,
        height,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn real_window_choices() {
        crate::render_window_tests::restore_in_private_window();
    }

    fn prompt() -> RestorePrompt {
        RestorePrompt::new(attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "saved".into(),
                sessions: vec!["gone".into()],
                layout: None,
            }],
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn empty_cache_does_not_ask() {
        assert!(RestorePrompt::new(Default::default()).is_none());
        assert!(RestorePrompt::new(attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "empty".into(),
                sessions: vec![],
                layout: None,
            }],
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn restore_choice_keys() {
        let mut prompt = prompt();
        assert_eq!(prompt.key(&Key::Named(NamedKey::Enter)), Some(true));
        for key in [
            NamedKey::ArrowDown,
            NamedKey::ArrowUp,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Tab,
        ] {
            assert_eq!(prompt.key(&Key::Named(key)), None);
            assert_eq!(prompt.key(&Key::Named(NamedKey::Enter)), Some(false));
            assert_eq!(prompt.key(&Key::Named(key)), None);
            assert_eq!(prompt.key(&Key::Named(NamedKey::Enter)), Some(true));
        }
        for key in ["r", "R", "y", "Y"] {
            assert_eq!(prompt.key(&Key::Character(key.into())), Some(true));
        }
        for key in ["n", "N"] {
            assert_eq!(prompt.key(&Key::Character(key.into())), Some(false));
        }
        assert_eq!(prompt.key(&Key::Named(NamedKey::Escape)), Some(false));
        assert_eq!(prompt.key(&Key::Character("z".into())), None);
    }
}
