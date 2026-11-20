//! Cooperative host restart. Blank/local PTYs require the old process to stay.
use super::*;
use prismattyc_mux::component_restart as requests;
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(super) struct WindowState {
    path: PathBuf,
    width: u32,
    height: u32,
    position: Option<(i32, i32)>,
    focused: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Resume {
    pub request: requests::Request,
    pub views: Vec<WindowState>,
    #[serde(default)]
    pub notice: Option<String>,
}

#[cfg(windows)]
pub(super) fn receive() -> Result<Option<Resume>> {
    use std::io::Read;
    let Some(raw) = std::env::var_os("PMUX_HOST_RESTART") else {
        return Ok(None);
    };
    std::env::remove_var("PMUX_HOST_RESTART");
    let resume = serde_json::from_str::<Resume>(&raw.to_string_lossy())?;
    let mut pid = [0u8; 4];
    std::io::stdin()
        .read_exact(&mut pid)
        .context("read replacement identity")?;
    anyhow::ensure!(
        u32::from_le_bytes(pid) == std::process::id(),
        "restart replacement identity mismatch"
    );
    Ok(Some(resume))
}

pub(super) fn poll(app: &mut App) {
    let now = Instant::now();
    if app
        .last_component_poll
        .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
    {
        return;
    }
    app.last_component_poll = Some(now);
    let Some(socket) = host_mux_socket() else {
        return;
    };
    if let Err(error) = app
        .file_writer
        .handle()
        .component_heartbeat(socket.clone(), "host")
    {
        eprintln!("prismattyc-host: could not write component heartbeat: {error}");
    }
    let Some(request) = requests::take(&socket, "host") else {
        return;
    };
    if let Err(error) = perform(app, &request) {
        #[cfg(target_os = "macos")]
        let update = app
            .update_restart_notice
            .take()
            .map(|notice| format!("{notice} Restart deferred."))
            .unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        let update = String::new();
        let message = format!("{update} Restart deferred: {error:#}");
        let _ = requests::respond(&socket, &request, "deferred", &message);
        for host in app.windows.values_mut() {
            rail_error_toast(host, &message);
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn schedule_after_update(app: &mut App, notice: String) -> Result<()> {
    app.update_restart_notice = Some(notice);
    let socket = host_mux_socket().context("mux socket path")?;
    app.file_writer
        .handle()
        .wait_for_component_heartbeat(&socket, "host");
    requests::register(&socket, "host")?;
    requests::request(&socket, "host", std::process::id())?;
    Ok(())
}

fn perform(app: &mut App, request: &requests::Request) -> Result<()> {
    anyhow::ensure!(!app.windows.is_empty(), "no host window to restore");
    let mut views = Vec::new();
    for host in app.windows.values_mut() {
        anyhow::ensure!(
            !local_views::has_local(&host.mux) && host.local_views.parked.is_empty(),
            "this window owns blank terminals; close those terminals before restarting the host"
        );
        anyhow::ensure!(
            !host.space_opens.blocks_persist() && host.restore_prompt.is_none(),
            "finish the current Space operation first"
        );
        anyhow::ensure!(host.cache_writer, "window does not own a restorable view");
        // The legacy view cache stores session targets, which reopen their
        // first pane. Preserve exact secondary-pane views by deferring.
        let snapshot = attach_log::live_snapshot().context("daemon unavailable")?;
        for (_, panes) in host.mux.tab_panes() {
            for pane in panes {
                let first = host
                    .mux
                    .attach_session_of(pane)
                    .and_then(|id| snapshot.sessions.iter().find(|s| s.id.to_string() == id))
                    .and_then(|s| s.windows.first())
                    .and_then(|w| w.panes.first())
                    .map(|p| p.id);
                anyhow::ensure!(host.mux.remote_pane_id(pane) == first,
                    "this window has an exact secondary-pane view; close that view before restarting the host");
            }
        }
        views.push(save_window_view(host)?);
    }
    #[cfg(target_os = "macos")]
    let notice = app.update_restart_notice.take();
    #[cfg(not(target_os = "macos"))]
    let notice = None;
    let resume = Resume {
        request: request.clone(),
        views,
        notice,
    };
    #[cfg(unix)]
    let executable = prismattyc_mux::release_update::installed_binary("prismattyc-host")
        .or_else(|| std::env::current_exe().ok())
        .context("host executable")?;
    #[cfg(windows)]
    let executable = prismattyc_mux::release_update::replacement_binary("prismattyc-host")?;
    let mut command = std::process::Command::new(executable);
    command
        .args(std::env::args_os().skip(1))
        .env("PMUX_HOST_RESTART", serde_json::to_string(&resume)?);
    #[cfg(unix)]
    {
        let error = prismattyc_process::exec(&mut command);
        Err(error).context("start replacement host")
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        // The replacement claims the single-instance mutex. Release it first
        // or the child focuses this window and exits.
        super::instance::release();
        let mut child = prismattyc_process::spawn(command.stdin(std::process::Stdio::piped()))
            .context("start replacement host")?;
        let mut input = child.stdin.take().context("replacement input pipe")?;
        if let Err(error) = input.write_all(&child.id().to_le_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("send replacement identity");
        }
        drop(input);
        std::process::exit(0);
    }
}

/// Attach targets for a saved view, one per session in saved order.
///
/// A target's title becomes the stable name the restored pane is marked with,
/// and regroup matches the saved sessions against live panes by that name. It
/// must be the session's own name. A tab's title would give every session in
/// a split tab the same name, so regroup would see the real names as detached
/// and attach those sessions a second time in an extra tab. The tab title
/// stays on the saved tab record, which restore applies on its own.
/// Write this window's tabs to its view file and record its size, position
/// and focus, so a relaunched app can open the same window again.
pub(super) fn save_window_view(host: &mut HostState) -> Result<WindowState> {
    sync_attach_pane_sessions(host);
    let path = host
        .attach_layout_path
        .clone()
        .context("window has no saved view path")?;
    let records = attach_records_from_live(host);
    anyhow::ensure!(
        records.tabs.iter().any(|tab| !tab.sessions.is_empty()),
        "window has no mux sessions to restore"
    );
    host.file_writer.wait_for_attach_tabs(&path);
    prismattyc_mux::attach_tabs::save(&path, &records)?;
    let size = host.window.inner_size();
    Ok(WindowState {
        path,
        width: size.width,
        height: size.height,
        position: host.window.outer_position().ok().map(|p| (p.x, p.y)),
        focused: host.window_focused,
    })
}

/// Open each saved window on its saved tabs, at its saved size and place.
/// Returns how many windows opened; a view that no longer loads is skipped.
pub(super) fn reopen(app: &mut App, event_loop: &ActiveEventLoop, views: &[WindowState]) -> usize {
    let mut focused = None;
    let mut opened = 0;
    for view in views {
        let Some(layout) = attach_tabs::load(&view.path) else {
            continue;
        };
        app.cli.attach_sessions = restore_targets(&layout);
        app.restart_view = Some(view.path.clone());
        let Ok(id) = app.open_window(event_loop, false) else {
            continue;
        };
        opened += 1;
        let window = &app.windows[&id].window;
        let _ = window.request_inner_size(PhysicalSize::new(view.width, view.height));
        if let Some((x, y)) = view.position {
            window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        if view.focused {
            focused = Some(id);
        }
    }
    app.restart_view = None;
    if let Some(id) = focused {
        app.windows[&id].window.focus_window();
    }
    opened
}

fn restore_targets(layout: &attach_tabs::AttachTabsFile) -> Vec<AttachTarget> {
    layout
        .tabs
        .iter()
        .flat_map(|tab| {
            tab.sessions.iter().map(|session| AttachTarget {
                session: session.clone(),
                title: layout
                    .session_names
                    .get(session)
                    .cloned()
                    .unwrap_or_else(|| tab.title.clone()),
            })
        })
        .collect()
}

pub(super) fn resume(app: &mut App, event_loop: &ActiveEventLoop) -> bool {
    #[cfg(unix)]
    let resume = {
        let Some(raw) = std::env::var_os("PMUX_HOST_RESTART") else {
            return false;
        };
        std::env::remove_var("PMUX_HOST_RESTART");
        let Ok(resume) = serde_json::from_str::<Resume>(&raw.to_string_lossy()) else {
            return false;
        };
        if resume.request.pid != std::process::id() {
            return false;
        }
        resume
    };
    #[cfg(windows)]
    let Some(resume) = app.restart_resume.take() else {
        return false;
    };
    let Some(socket) = host_mux_socket() else {
        return false;
    };
    let mut focused = None;
    let notice = resume.notice.clone();
    for view in resume.views {
        let path = view.path;
        let Some(layout) = attach_tabs::load(&path) else {
            let _ = requests::respond(
                &socket,
                &resume.request,
                "failed",
                "could not load a saved window",
            );
            app.restart_view = None;
            if app.windows.is_empty() {
                event_loop.exit();
            }
            return true;
        };
        app.cli.attach_sessions = restore_targets(&layout);
        app.restart_view = Some(path);
        match app.open_window(event_loop, false) {
            Ok(id) => {
                let window = &app.windows[&id].window;
                let _ = window.request_inner_size(PhysicalSize::new(view.width, view.height));
                if let Some((x, y)) = view.position {
                    window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                }
                if view.focused {
                    focused = Some(id);
                }
            }
            Err(error) => {
                let _ =
                    requests::respond(&socket, &resume.request, "failed", &format!("{error:#}"));
                app.restart_view = None;
                if app.windows.is_empty() {
                    event_loop.exit();
                }
                return true;
            }
        }
    }
    app.restart_view = None;
    if let Some(notice) = notice {
        for host in app.windows.values_mut() {
            rail_toast(host, &notice);
        }
    }
    if let Some(id) = focused {
        app.windows[&id].window.focus_window();
    }
    let _ = requests::register(&socket, "host");
    let _ = requests::respond(
        &socket,
        &resume.request,
        "restarted",
        "windows restored; mux sessions kept running",
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::attach_tabs::{AttachTabRecord, AttachTabsFile};

    fn tab(title: &str, sessions: &[&str]) -> AttachTabRecord {
        AttachTabRecord {
            title: title.into(),
            fixed_name: None,
            sessions: sessions.iter().map(|s| (*s).to_string()).collect(),
            layout: None,
        }
    }

    #[test]
    fn restored_sessions_keep_their_own_names_in_a_split_tab() {
        let layout = AttachTabsFile {
            tabs: vec![tab("alpha-1", &["2", "4"]), tab("alpha-2", &["3"])],
            session_names: [("2", "alpha-1"), ("3", "alpha-2"), ("4", "alpha-3")]
                .into_iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
            ..AttachTabsFile::default()
        };
        let targets: Vec<(String, String)> = restore_targets(&layout)
            .into_iter()
            .map(|t| (t.session, t.title))
            .collect();
        assert_eq!(
            targets,
            [("2", "alpha-1"), ("4", "alpha-3"), ("3", "alpha-2")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn a_session_without_a_saved_name_falls_back_to_its_tab_title() {
        let layout = AttachTabsFile {
            tabs: vec![tab("alpha-1", &["2"])],
            ..AttachTabsFile::default()
        };
        assert_eq!(restore_targets(&layout)[0].title, "alpha-1");
    }
}
