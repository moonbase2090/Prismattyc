//! Search existing terminals across Space views. Navigation never starts a session.
use super::*;
#[derive(Clone)]
pub(super) struct Entry {
    pub label: String,
    pub display_label: String,
    pub detail: String,
    owner: Option<String>,
    local: Option<(PaneId, u32)>,
    session: Option<String>,
    process: Option<u32>,
    remote: Option<u64>,
}
fn owner_name(owner: Option<&str>) -> String {
    let dir = spaces_dir();
    prismattyc_mux::list_spaces(&dir)
        .unwrap_or_default()
        .into_iter()
        .find(|entry| {
            load_space(&dir, &entry.name)
                .ok()
                .is_some_and(|space| space.id.as_deref() == owner)
        })
        .map(|entry| entry.name)
        .unwrap_or_else(|| "This window".into())
}
pub(super) fn open(host: &mut HostState) {
    open_mode(host, false);
}
pub(super) fn messages(host: &mut HostState) {
    open_mode(host, true);
}
fn open_mode(host: &mut HostState, messages: bool) {
    let mut entries = Vec::new();
    let mut add = |owner: &Option<String>, mux: &mux::MuxRuntime| {
        for (pane, title, cwd, pid) in mux.local_terminal_rows() {
            if !mux.is_retained_local_terminal(pane)
                || !mux.pane(pane).is_some_and(|p| p.child_alive)
            {
                continue;
            }
            let Some(pid) = pid else {
                continue;
            };
            let directory = cwd.map(|p| p.display().to_string()).unwrap_or_default();
            entries.push(Entry {
                label: format!(
                    "{} / {} — {}",
                    owner_name(owner.as_deref()),
                    title,
                    directory
                ),
                display_label: title,
                detail: format!(
                    "{} · {} · Blank terminal",
                    Path::new(&directory)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    owner_name(owner.as_deref())
                ),
                owner: owner.clone(),
                local: Some((pane, pid)),
                session: None,
                process: None,
                remote: None,
            });
        }
    };
    if !messages {
        add(&host.mux.space_id, &host.mux);
        for (owner, view) in &host.local_views.parked {
            add(owner, &view.mux);
        }
    }
    if let Some(snapshot) = attach_log::live_snapshot() {
        for session in &snapshot.sessions {
            for pane in session
                .windows
                .iter()
                .flat_map(|w| &w.panes)
                .filter(|p| p.child_pid.is_some())
            {
                let mail = pane.mail.as_ref().map_or(0, |m| m.depth);
                if messages && mail == 0 && pane.pane_write.is_none() {
                    continue;
                }
                let receipt = pane
                    .pane_write
                    .as_ref()
                    .map(|r| {
                        format!(
                            " · {} queued {}/{} bytes · execution unknown",
                            if r.complete { "Complete" } else { "Partial" },
                            r.nbytes,
                            r.total_bytes
                        )
                    })
                    .unwrap_or_default();
                let directory = pane
                    .child_pid
                    .and_then(prismattyc_mux::procinfo::cwd_of)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                entries.push(Entry {
                    label: format!(
                        "{} / {} / pane {} — {} · {} pending mail{}",
                        owner_name(session.space_id.as_deref()),
                        session.name,
                        pane.id,
                        directory,
                        mail,
                        receipt
                    ),
                    display_label: format!("{} / pane {}", session.name, pane.id),
                    detail: format!(
                        "{mail} pending mail{receipt} · {}",
                        owner_name(session.space_id.as_deref())
                    ),
                    owner: session.space_id.clone(),
                    local: None,
                    session: Some(session.id.to_string()),
                    process: pane.child_pid,
                    remote: Some(pane.id),
                });
            }
        }
    }
    let mut counts = HashMap::new();
    for entry in &mut entries {
        let count = counts.entry(entry.label.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            entry.label.push_str(&format!(" ({count})"));
            entry.display_label.push_str(&format!(" ({count})"));
        }
    }
    host.palette = None;
    host.space_panel = None;
    host.context_menu = None;
    host.terminal_messages = messages;
    host.terminal_targets = Some(entries);
    host.space_picker = Some(SpacePicker::new(SpacePickerKind::Open));
    host.space_rail.leave();
    host.palette_layout = None;
    host.dirty = true;
    host.window.request_redraw();
}
pub(super) fn rows(host: &HostState, kind: SpacePickerKind) -> Vec<SpacePickerRow> {
    match &host.terminal_targets {
        Some(entries) => entries
            .iter()
            .map(|entry| SpacePickerRow {
                name: entry.label.clone(),
                sessions: 0,
                saved_at_unix: 0,
            })
            .collect(),
        None => space_picker_rows(kind, host.space_rail.current.as_deref()),
    }
}
pub(super) fn activate(host: &mut HostState, label: &str) -> bool {
    let Some(entries) = host.terminal_targets.take() else {
        return false;
    };
    let Some(entry) = entries.into_iter().find(|entry| entry.label == label) else {
        return true;
    };
    if let Err(error) = navigate(host, &entry) {
        rail_toast(host, &format!("Terminal unavailable: {error}"));
    }
    true
}
fn navigate(host: &mut HostState, entry: &Entry) -> Result<()> {
    // Recheck the exact live target before moving focus or changing views.
    if let Some((pane, pid)) = entry.local {
        let mux = if host.mux.space_id == entry.owner {
            &host.mux
        } else {
            &host
                .local_views
                .parked
                .get(&entry.owner)
                .context("Space view was closed")?
                .mux
        };
        anyhow::ensure!(
            mux.is_retained_local_terminal(pane)
                && mux.pane(pane).is_some_and(|p| p.child_alive)
                && mux.pane(pane).and_then(|p| p.child_pid()) == Some(pid),
            "terminal was closed or moved"
        );
    }
    if let Some(id) = &entry.session {
        let snapshot = attach_log::live_snapshot().context("daemon unavailable")?;
        anyhow::ensure!(
            snapshot.sessions.iter().any(|s| s.id.to_string() == *id
                && s.space_id == entry.owner
                && s.windows
                    .iter()
                    .flat_map(|w| &w.panes)
                    .any(|p| Some(p.id) == entry.remote && p.child_pid == entry.process)),
            "session was closed or moved"
        );
    }
    local_views::activate_owner(host, entry.owner.clone())?;
    if let Some(session) = &entry.session {
        if !host
            .mux
            .tab_panes()
            .iter()
            .flat_map(|(_, panes)| panes)
            .any(|pane| {
                host.mux.attach_session_of(*pane) == Some(session)
                    && host.mux.remote_pane_id(*pane) == entry.remote
            })
        {
            let names = attach_log::session_names();
            let name = names.get(session).context("session disappeared")?.clone();
            let program = find_mux_bin().to_string_lossy().into_owned();
            host.mux.new_tab(
                &program,
                &[
                    "attach".into(),
                    "--session-id".into(),
                    session.clone(),
                    "--host-pane-id".into(),
                    entry.remote.context("missing pane")?.to_string(),
                    entry.process.context("missing process")?.to_string(),
                ],
            )?;
            let pane = host.mux.focused_id();
            host.mux.mark_attach_session(pane, session.clone(), name);
            sync_attach_pane_sessions(host);
        }
    }

    let pane = entry
        .local
        .map(|(pane, _)| pane)
        .or_else(|| {
            host.mux
                .tab_panes()
                .into_iter()
                .flat_map(|(_, panes)| panes)
                .find(|pane| {
                    host.mux.attach_session_of(*pane) == entry.session.as_deref()
                        && host.mux.remote_pane_id(*pane) == entry.remote
                })
        })
        .context("terminal is no longer in this view")?;
    let window = host.mux.pane_window(pane).context("tab closed")?;
    let index = host
        .mux
        .window_ids()
        .iter()
        .position(|id| *id == window)
        .context("tab closed")?;
    host.mux.select_tab(index)?;
    host.mux.focus(pane);
    App::refit_geom(host, host.window.inner_size(), Some("terminal switcher"));
    persist_attach_layout_from_live(host);
    host.dirty = true;
    Ok(())
}
