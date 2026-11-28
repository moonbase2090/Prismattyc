//! One startup decision before the registered host consumes its saved cache.

use super::*;

/// How long the restore choice waits once the user can see it.
const RESTORE_COUNTDOWN: Duration = Duration::from_secs(3);

pub(super) struct RestorePrompt {
    layout: attach_tabs::AttachTabsFile,
    pub(super) selected: usize,
    automatic_at: Option<Instant>,
    startup: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// Esc arrived while the daemon was still starting. Apply it when that
    /// startup finishes, so a restore cannot put the sessions back afterwards.
    declined: bool,
    /// The countdown was armed while the splash covered this choice.
    /// Start it when the splash is gone and startup has finished.
    held_for_splash: bool,
}

pub(super) fn cache_matches_space(
    layout: &attach_tabs::AttachTabsFile,
    dir: &std::path::Path,
) -> bool {
    let Some(name) = layout.space.as_deref() else {
        return true;
    };
    let Ok(space) = load_space(dir, name) else {
        return false;
    };
    layout
        .space_id
        .as_deref()
        .is_none_or(|cached_id| space.id.as_deref() == Some(cached_id))
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
                declined: false,
                held_for_splash: false,
            })
    }

    pub(super) fn arm(&mut self, socket: PathBuf, pmux: PathBuf, wake: mux::Wake) {
        self.automatic_at = Some(Instant::now() + RESTORE_COUNTDOWN);
        let (sender, receiver) = std::sync::mpsc::channel();
        self.startup = Some(receiver);
        std::thread::spawn(move || {
            let result = prismattyc_process::spawn(
                prismattyc_mux::platform::hidden_capture_command(pmux)
                    .arg("--socket")
                    .arg(socket)
                    .arg("up"),
            )
            .and_then(std::process::Child::wait_with_output)
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

    /// A choice made while the daemon is still starting waits for it. Esc
    /// selects Start fresh and says so, so the key visibly took effect.
    fn defer(&mut self, accept: bool, now: Instant) {
        if accept {
            self.declined = false;
            self.automatic_at = Some(now);
        } else {
            // The daemon may still be restoring this Space. Detach only after
            // that startup returns, or it would put the sessions back.
            self.declined = true;
            self.automatic_at = None;
            self.selected = 1;
        }
    }

    /// The two choices, with what each will do right now.
    fn rows(&self) -> [PaletteRow; 2] {
        let waiting = self.startup.is_some();
        [
            PaletteRow::plain(
                "Restore workspace".into(),
                if self.automatic_at.is_some() && !self.declined {
                    "Restoring automatically. Esc starts fresh.".into()
                } else {
                    "Reconnect saved sessions, tabs, panes, and Spaces.".into()
                },
                "R".into(),
            ),
            PaletteRow::plain(
                "Start fresh".into(),
                if self.declined && waiting {
                    "Starting fresh once the pmux server has started…".into()
                } else {
                    "Keep this fresh window.".into()
                },
                "Esc".into(),
            ),
        ]
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
        self.held_for_splash = false;
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
    // The splash is painted above this choice and owns the pointer until
    // the user dismisses it.
    if host.splash.is_some() || host.restore_prompt.is_none() {
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
    if let Some(prompt) = host.restore_prompt.as_mut().filter(|p| p.startup.is_some()) {
        prompt.defer(accept, Instant::now());
        host.dirty = true;
        host.window.request_redraw();
        return;
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
                let space = file.space.clone();
                host.attach_layout = Some(file);
                host.layout_dirty = false;
                // The tiles are back; put the Space's floating pane back too.
                if let Some(space) = space.as_deref() {
                    crate::restore_open_space_float(host, space);
                }
            }
            Err(error) => {
                eprintln!("prismattyc-host: could not restore last space: {error:#}");
                rail_error_toast(host, &format!(" could not restore last space: {error:#} "));
            }
        }
    } else {
        let space_name = prompt.layout.space.clone();
        host.local_views.fresh = true;
        set_current_space(host, None);
        if let Some(path) = host.attach_layout_path.clone() {
            host.force_attach_write = true;
            if let Err(error) = enqueue_attach_tabs(host, &path, Default::default()) {
                rail_error_toast(host, &format!(" Could not save fresh workspace: {error} "));
            }
        }
        if let Some(name) = space_name {
            host.start_fresh_release = release_saved_sessions(&name, host.ui_wake.clone());
        }
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
    poll_at(host, Instant::now());
}

/// [`poll`] at a given time, so tests drive the countdown without the clock.
pub(super) fn poll_at(host: &mut HostState, now: Instant) {
    enum After {
        Quiet,
        Decline,
        Failed(String),
        FailedThenDecline(String),
    }
    let after = {
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
            let declined = prompt.declined;
            prompt.startup = None;
            match result {
                Err(error) if declined => After::FailedThenDecline(error),
                Err(error) => {
                    prompt.automatic_at = None;
                    prompt.held_for_splash = false;
                    After::Failed(error)
                }
                Ok(()) if declined => After::Decline,
                Ok(()) => After::Quiet,
            }
        } else {
            After::Quiet
        }
    };
    match after {
        After::Failed(error) => {
            rail_error_toast(host, &format!(" Could not start workspace: {error} "));
            return;
        }
        After::FailedThenDecline(error) => {
            rail_error_toast(host, &format!(" Could not start workspace: {error} "));
            finish(host, false);
            return;
        }
        After::Decline => {
            finish(host, false);
            return;
        }
        After::Quiet => {}
    }
    let step = host.restore_prompt.as_ref().map(|prompt| {
        countdown_step(
            host.splash.is_some(),
            prompt.held_for_splash,
            prompt.startup.is_some(),
            prompt.automatic_at,
            now,
        )
    });
    match step {
        Some(CountdownStep::Hold) => {
            if let Some(prompt) = host.restore_prompt.as_mut() {
                prompt.held_for_splash = true;
            }
        }
        Some(CountdownStep::Rearm) => {
            if let Some(prompt) = host.restore_prompt.as_mut() {
                prompt.automatic_at = Some(now + RESTORE_COUNTDOWN);
                prompt.held_for_splash = false;
            }
        }
        Some(CountdownStep::Restore) => finish(host, true),
        Some(CountdownStep::Wait) | None => {}
    }
}

/// The splash covers the choice, so a key cannot cancel the countdown and
/// the deadline must not restore the workspace until the splash is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CountdownStep {
    Hold,
    Rearm,
    Restore,
    Wait,
}

fn countdown_step(
    splash_up: bool,
    held_for_splash: bool,
    startup_pending: bool,
    automatic_at: Option<Instant>,
    now: Instant,
) -> CountdownStep {
    if automatic_at.is_none() {
        return CountdownStep::Wait;
    }
    if splash_up {
        return CountdownStep::Hold;
    }
    if held_for_splash && !startup_pending {
        return CountdownStep::Rearm;
    }
    if !startup_pending && automatic_at.is_some_and(|at| now >= at) {
        CountdownStep::Restore
    } else {
        CountdownStep::Wait
    }
}

/// Start fresh releasing a saved Space's sessions: a worker thread, and the
/// sessions it could not release (`name: reason`) once it is done.
pub(super) struct Release {
    space: String,
    done: std::sync::mpsc::Receiver<Vec<String>>,
}

/// Drop live Space ownership for the saved sessions, off the window thread.
/// Processes keep running and stay in the attach list. `space remove`
/// without `--kill` is the existing release; it does not destroy the session.
fn release_saved_sessions(space_name: &str, wake: mux::Wake) -> Option<Release> {
    let socket = host_mux_socket().filter(|socket| socket.exists())?;
    let space = load_space(&spaces_dir(), space_name).ok()?;
    let names: Vec<String> = space.sessions.into_iter().map(|s| s.name).collect();
    spawn_release(find_mux_bin(), socket, space_name.to_owned(), names, wake)
}

/// Run [`release_sessions`] on a worker thread and return at once.
fn spawn_release(
    pmux: PathBuf,
    socket: PathBuf,
    space: String,
    names: Vec<String>,
    wake: mux::Wake,
) -> Option<Release> {
    let (sender, done) = std::sync::mpsc::channel();
    let worker_space = space.clone();
    std::thread::Builder::new()
        .name("start-fresh-release".into())
        .spawn(move || {
            let _ = sender.send(release_sessions(&pmux, &socket, &worker_space, &names));
            wake();
        })
        .ok()?;
    Some(Release { space, done })
}

/// Release each session from the Space; returns `name: reason` for each
/// one that could not be released.
fn release_sessions(
    pmux: &std::path::Path,
    socket: &std::path::Path,
    space: &str,
    names: &[String],
) -> Vec<String> {
    let mut failures = Vec::new();
    for name in names {
        let output = prismattyc_process::spawn(
            prismattyc_mux::platform::hidden_capture_command(pmux)
                .arg("--socket")
                .arg(socket)
                .arg("space")
                .arg("remove")
                .arg(space)
                .arg("--session")
                .arg(name),
        )
        .and_then(std::process::Child::wait_with_output);
        let reason = match output {
            Ok(output) if output.status.success() => continue,
            Ok(output) => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                if detail.is_empty() {
                    format!("pmux exited with {}", output.status)
                } else {
                    detail
                }
            }
            Err(error) => error.to_string(),
        };
        eprintln!("prismattyc-host: could not detach {name}: {reason}");
        failures.push(format!("{name}: {reason}"));
    }
    failures
}

/// The error the app shows when Start fresh could not release sessions.
fn release_failure_text(space: &str, failures: &[String]) -> Option<String> {
    let first = failures.first()?;
    let more = match failures.len() {
        1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    Some(format!(
        " Start fresh could not release {} from {space}: {first}{more}. They still belong to {space}. ",
        if failures.len() == 1 { "a session" } else { "sessions" },
    ))
}

/// Show the result of Start fresh's release once its worker is done.
pub(super) fn poll_release(host: &mut HostState) {
    let Some(release) = host.start_fresh_release.as_ref() else {
        return;
    };
    let failures = match release.done.try_recv() {
        Ok(failures) => failures,
        Err(std::sync::mpsc::TryRecvError::Empty) => return,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            vec!["the release worker stopped".to_owned()]
        }
    };
    let space = release.space.clone();
    host.start_fresh_release = None;
    if let Some(text) = release_failure_text(&space, &failures) {
        rail_error_toast(host, &text);
        host.dirty = true;
        host.window.request_redraw();
    }
}

pub(super) fn sync_login_registration() {
    let Some(socket) = host_mux_socket() else {
        return;
    };
    let pmux = find_mux_bin();
    std::thread::spawn(move || {
        match prismattyc_process::spawn(
            prismattyc_mux::platform::hidden_capture_command(pmux)
                .arg("--socket")
                .arg(socket)
                .args(["login", "sync"]),
        )
        .and_then(std::process::Child::wait_with_output)
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

/// The open choice for render-status: its selection and each row's detail.
pub(super) fn status(host: &HostState) -> serde_json::Value {
    let Some(prompt) = host.restore_prompt.as_ref() else {
        return serde_json::Value::Null;
    };
    let rows: Vec<_> = prompt
        .rows()
        .iter()
        .map(|row| serde_json::json!({"label": row.name, "detail": row.describe}))
        .collect();
    serde_json::json!({
        "selected": prompt.selected,
        "rows": rows,
        "starting": prompt.startup.is_some(),
        "start_fresh_queued": prompt.declined,
    })
}

pub(super) fn paint(host: &mut HostState, buffer: &mut [u32], width: usize, height: usize) {
    let Some(prompt) = host.restore_prompt.as_ref() else {
        return;
    };
    let rows = prompt.rows();
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

#[cfg(all(test, target_os = "linux"))]
pub(super) fn test_expire_countdown(host: &mut HostState) {
    let prompt = host.restore_prompt.as_mut().expect("restore prompt");
    prompt.startup = None;
    prompt.declined = false;
    prompt.held_for_splash = false;
    prompt.automatic_at = Some(Instant::now() - Duration::from_secs(1));
}

#[cfg(all(test, target_os = "linux"))]
pub(super) fn test_countdown_deadline(host: &HostState) -> Option<Instant> {
    host.restore_prompt.as_ref()?.automatic_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn real_window_choices() {
        crate::render_window_tests::restore_in_private_window();
    }

    #[test]
    fn splash_holds_an_expired_countdown() {
        let now = Instant::now();
        let past = Some(now - Duration::from_secs(1));
        assert_eq!(
            countdown_step(true, false, false, past, now),
            CountdownStep::Hold
        );
    }

    #[test]
    fn dismissing_the_splash_rearms_the_countdown() {
        let now = Instant::now();
        let past = Some(now - Duration::from_secs(1));
        assert_eq!(
            countdown_step(false, true, false, past, now),
            CountdownStep::Rearm
        );
        assert_eq!(
            countdown_step(false, true, true, past, now),
            CountdownStep::Wait
        );
    }

    #[test]
    fn a_visible_expired_countdown_restores() {
        let now = Instant::now();
        let past = Some(now - Duration::from_secs(1));
        assert_eq!(
            countdown_step(false, false, false, past, now),
            CountdownStep::Restore
        );
        assert_eq!(
            countdown_step(false, false, false, Some(now + RESTORE_COUNTDOWN), now),
            CountdownStep::Wait
        );
        assert_eq!(
            countdown_step(false, false, false, None, now),
            CountdownStep::Wait
        );
    }

    fn prompt() -> RestorePrompt {
        RestorePrompt::new(attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "saved".into(),
                sessions: vec!["gone".into()],
                layout: None,
                fixed_name: None,
            }],
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn esc_during_startup_selects_start_fresh_and_says_it_waits() {
        let mut prompt = prompt();
        let (_sender, receiver) = std::sync::mpsc::channel();
        prompt.startup = Some(receiver);
        prompt.automatic_at = Some(Instant::now());
        assert_eq!(prompt.rows()[1].describe, "Keep this fresh window.");
        prompt.defer(false, Instant::now());
        assert!(prompt.declined);
        assert_eq!(prompt.selected, 1, "Esc moves the selection to Start fresh");
        let rows = prompt.rows();
        assert_eq!(
            rows[1].describe,
            "Starting fresh once the pmux server has started…"
        );
        assert_eq!(
            rows[0].describe,
            "Reconnect saved sessions, tabs, panes, and Spaces."
        );
        // Changing the mind to Restore while still starting clears it.
        prompt.defer(true, Instant::now());
        assert!(!prompt.declined);
        assert_eq!(prompt.rows()[1].describe, "Keep this fresh window.");
        assert_eq!(
            prompt.rows()[0].describe,
            "Restoring automatically. Esc starts fresh."
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_release_names_the_session_and_why() {
        let dir = std::env::temp_dir().join(format!("prismattyc-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls");
        let pmux = dir.join("pmux");
        crate::test_support::write_script(
            &pmux,
            &format!(
                "#!/bin/sh\necho \"$*\" >> {}\nfor a in \"$@\"; do last=$a; done\n\
                 if [ \"$last\" = beta ]; then echo 'session beta is not in work' >&2; exit 1; fi\n",
                log.display()
            ),
            "700",
        );
        let failures = release_sessions(
            &pmux,
            &dir.join("pmux.sock"),
            "work",
            &["alpha".into(), "beta".into()],
        );
        assert_eq!(failures, ["beta: session beta is not in work"]);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains("space remove work --session alpha")
                && calls.contains("space remove work --session beta"),
            "{calls}"
        );
        assert!(!calls.contains("--kill"), "a release never ends a session");
        let text = release_failure_text("work", &failures).unwrap();
        assert!(
            text.contains(
                "could not release a session from work: beta: session beta is not in work"
            ),
            "{text}"
        );
        assert_eq!(release_failure_text("work", &[]), None);
        let many = release_failure_text("work", &["a: x".into(), "b: y".into()]).unwrap();
        assert!(
            many.contains("sessions from work: a: x (and 1 more)"),
            "{many}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn the_release_runs_off_the_calling_thread() {
        let dir =
            std::env::temp_dir().join(format!("prismattyc-release-thread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go");
        let pmux = dir.join("pmux");
        // Blocks until the test lets it go (at most about 30 s, so a release
        // run on this thread fails the check below instead of hanging).
        crate::test_support::write_script(
            &pmux,
            &format!(
                "#!/bin/sh\nn=0\nwhile [ ! -e {} ] && [ $n -lt 600 ]; do sleep 0.05; n=$((n+1)); done\necho 'server busy' >&2\nexit 1\n",
                go.display()
            ),
            "700",
        );
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = woke.clone();
        let wake: mux::Wake = std::sync::Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let release = spawn_release(
            pmux,
            dir.join("pmux.sock"),
            "work".into(),
            vec!["alpha".into()],
            wake,
        )
        .expect("worker");
        // pmux cannot finish before `go` exists, so this call returned first.
        assert!(matches!(
            release.done.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        std::fs::write(&go, b"").unwrap();
        let failures = release
            .done
            .recv_timeout(Duration::from_secs(30))
            .expect("the worker reports");
        assert_eq!(failures, ["alpha: server busy"]);
        assert!(
            woke.load(std::sync::atomic::Ordering::SeqCst),
            "the window is woken"
        );
        assert_eq!(release.space, "work");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_cache_does_not_ask() {
        assert!(RestorePrompt::new(Default::default()).is_none());
        assert!(RestorePrompt::new(attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "empty".into(),
                sessions: vec![],
                layout: None,
                fixed_name: None,
            }],
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn cache_for_missing_or_replaced_space_is_not_restored() {
        let dir = std::env::temp_dir().join(format!(
            "restore-prompt-space-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let layout = attach_tabs::AttachTabsFile {
            space: Some("default".into()),
            space_id: Some("old-owner".into()),
            ..Default::default()
        };
        assert!(!cache_matches_space(&layout, &dir));

        let space = prismattyc_mux::SavedSpace {
            version: prismattyc_mux::SAVED_SPACE_VERSION,
            id: Some("new-owner".into()),
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![prismattyc_mux::stub_space_session("default")],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        prismattyc_mux::save_space(&dir, "default", &space).unwrap();
        assert!(!cache_matches_space(&layout, &dir));
        let mut current = layout;
        current.space_id = prismattyc_mux::load_space(&dir, "default").unwrap().id;
        assert!(cache_matches_space(&current, &dir));
        let _ = std::fs::remove_dir_all(dir);
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
