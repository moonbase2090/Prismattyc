//! Cooperative host restart. Blank/local PTYs require the old process to stay.
use super::*;
use prismattyc_mux::component_restart as requests;
#[cfg(unix)]
use prismattyc_mux::platform::Exec;

#[derive(serde::Serialize, serde::Deserialize)]
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
    let _ = requests::register(&socket, "host");
    let Some(request) = requests::take(&socket, "host") else {
        return;
    };
    if let Err(error) = perform(app, &request) {
        let message = format!("Restart deferred: {error:#}");
        let _ = requests::respond(&socket, &request, "deferred", &message);
        for host in app.windows.values_mut() {
            rail_toast(host, &message);
        }
    }
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
        prismattyc_mux::attach_tabs::save(&path, &records)?;
        let size = host.window.inner_size();
        views.push(WindowState {
            path,
            width: size.width,
            height: size.height,
            position: host.window.outer_position().ok().map(|p| (p.x, p.y)),
            focused: host.window_focused,
        });
    }
    let resume = Resume {
        request: request.clone(),
        views,
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
        let error = command.exec();
        Err(error).context("start replacement host")
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .spawn()
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
        app.cli.attach_sessions = layout
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.sessions.iter().map(|session| AttachTarget {
                    session: session.clone(),
                    title: tab.title.clone(),
                })
            })
            .collect();
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
