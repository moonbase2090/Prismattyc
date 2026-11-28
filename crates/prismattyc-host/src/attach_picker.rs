//! Attach running session: bring a live pmux session into a Space as a tab.
//!
//! The picker lists every live session, grouped by where it is now. Enter
//! opens it here: a session no Space owns joins this Space, one owned by
//! another Space moves here after a confirmation that names that Space, and
//! one already open in this window is focused. Shift+Enter opens the session
//! in a new Space instead.
use super::*;
use prismattyc_mux::Snapshot;

/// Where a live session is, seen from the window that opened the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    /// No saved Space owns it.
    Loose,
    /// The target Space owns it, but no tab here shows it.
    Here,
    /// Another saved Space owns it. Enter moves it after a confirmation.
    Other,
    /// A tab in this window already shows it.
    Open,
}

impl Group {
    pub(super) fn header(self) -> &'static str {
        match self {
            Group::Loose => "NOT IN A SPACE",
            Group::Here => "IN THIS SPACE",
            Group::Other => "IN OTHER SPACES",
            Group::Open => "ALREADY OPEN",
        }
    }

    /// What Enter does, shown at the right of the row.
    pub(super) fn chord(self) -> &'static str {
        match self {
            Group::Loose | Group::Here => "Open here",
            Group::Other => "Move here",
            Group::Open => "Focus",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    /// Unique search key: the name plus what kind of session it is.
    pub label: String,
    pub name: String,
    pub session: u64,
    pub group: Group,
    /// The saved Space that owns the session, when there is one.
    pub owner: Option<String>,
    pub describe: String,
}

pub(super) struct Picker {
    pub entries: Vec<Entry>,
    /// The Space the session joins. `None` when the window is in no Space.
    pub space: Option<String>,
}

/// Group and describe every live session. Pure, so the rules are tested.
///
/// `open` holds the session ids shown in this window. `here` is the target
/// Space's name and id. `saved` lists the saved Spaces by name.
pub(super) fn entries(
    snapshot: &Snapshot,
    open: &std::collections::HashSet<String>,
    here: Option<(&str, &str)>,
    saved: &[(String, Option<String>)],
) -> Vec<Entry> {
    let mut entries: Vec<Entry> = snapshot
        .sessions
        .iter()
        .map(|session| {
            let owner_name = session.space_id.as_deref().and_then(|owner| {
                saved
                    .iter()
                    .find(|(_, id)| id.as_deref() == Some(owner))
                    .map(|(name, _)| name.clone())
            });
            let group = if open.contains(&session.id.to_string()) {
                Group::Open
            } else {
                match (session.space_id.as_deref(), &owner_name) {
                    (Some(owner), _) if here.is_some_and(|(_, id)| id == owner) => Group::Here,
                    (Some(_), Some(_)) => Group::Other,
                    // No owner, or an owner with no saved file: free to join.
                    _ => Group::Loose,
                }
            };
            let kind = if session.agent_id.is_some() {
                "agent seat"
            } else {
                "shell"
            };
            let panes: Vec<_> = session.windows.iter().flat_map(|w| &w.panes).collect();
            let status = if panes.iter().any(|pane| pane.attention.is_some()) {
                "needs you".to_string()
            } else if panes.len() == 1 {
                "1 pane".to_string()
            } else {
                format!("{} panes", panes.len())
            };
            let place = match group {
                Group::Open => "open in this window".to_string(),
                Group::Loose => "not in a Space".to_string(),
                Group::Here | Group::Other => {
                    format!("in {}", owner_name.as_deref().unwrap_or("a Space"))
                }
            };
            Entry {
                label: format!("{} · {kind}", session.name),
                name: session.name.clone(),
                session: session.id,
                group,
                owner: owner_name,
                describe: format!("{kind} · {status} · {place}"),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
    let mut seen = std::collections::HashMap::new();
    for entry in &mut entries {
        let count = seen.entry(entry.label.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            entry.label.push_str(&format!(" ({count})"));
        }
    }
    entries
}

fn saved_spaces() -> Vec<(String, Option<String>)> {
    let dir = spaces_dir();
    prismattyc_mux::list_spaces(&dir)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let id = load_space(&dir, &entry.name)
                .ok()
                .and_then(|space| space.id);
            (entry.name, id)
        })
        .collect()
}

/// Open the picker. `space` is the Space a picked session joins; `None`
/// means the window's current Space.
pub(super) fn open(host: &mut HostState, space: Option<String>) {
    let Some(snapshot) = attach_log::live_snapshot() else {
        rail_error_toast(host, " the pmux server is not running ");
        return;
    };
    let space = space.or_else(|| host.space_rail.current.clone());
    let saved = saved_spaces();
    let here_id = space.as_deref().and_then(|name| {
        saved
            .iter()
            .find(|(saved_name, _)| saved_name == name)
            .and_then(|(_, id)| id.clone())
    });
    let open: std::collections::HashSet<String> =
        host.attach_pane_sessions.values().cloned().collect();
    let entries = entries(
        &snapshot,
        &open,
        space.as_deref().zip(here_id.as_deref()),
        &saved,
    );
    host.palette = None;
    host.space_panel = None;
    host.context_menu = None;
    host.terminal_targets = None;
    host.attach_picker = Some(Picker { entries, space });
    host.space_picker = Some(SpacePicker::new(SpacePickerKind::Attach));
    host.space_rail.leave();
    host.palette_layout = None;
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn rows(host: &HostState) -> Vec<SpacePickerRow> {
    host.attach_picker
        .as_ref()
        .map(|picker| {
            picker
                .entries
                .iter()
                .map(|entry| SpacePickerRow {
                    name: entry.label.clone(),
                    sessions: 0,
                    saved_at_unix: 0,
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn entry<'a>(host: &'a HostState, label: &str) -> Option<&'a Entry> {
    host.attach_picker
        .as_ref()?
        .entries
        .iter()
        .find(|entry| entry.label == label)
}

/// The open picker as render-status reports it: rows in display order with
/// their group, the selected row, the row awaiting a move confirmation, and
/// the target Space. `None` when the picker is closed.
pub(super) fn status(host: &HostState) -> Option<serde_json::Value> {
    let picker = host
        .space_picker
        .as_ref()
        .filter(|picker| picker.kind == SpacePickerKind::Attach)?;
    let attach = host.attach_picker.as_ref()?;
    let rows = rows(host);
    let ranked = picker.ranked(&rows);
    let listed: Vec<_> = ranked
        .iter()
        .filter_map(|row| entry(host, &row.name))
        .map(|entry| {
            serde_json::json!({
                "name": entry.name,
                "session": entry.session,
                "group": entry.group.header(),
                "action": entry.group.chord(),
                "describe": entry.describe,
            })
        })
        .collect();
    let selected = ranked
        .get(picker.selected)
        .and_then(|row| entry(host, &row.name))
        .map(|entry| entry.name.clone());
    let confirm = picker
        .confirm
        .as_deref()
        .and_then(|label| entry(host, label))
        .map(|entry| confirm_text(entry, attach.space.as_deref()));
    Some(serde_json::json!({
        "space": attach.space,
        "rows": listed,
        "selected": selected,
        "confirm": confirm,
    }))
}

/// What the key that chose a row asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum How {
    Enter,
    Confirmed,
    NewSpace,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Done,
    /// Ask before moving: the text names the Space the session leaves.
    Confirm(String),
}

/// The confirmation shown before a session leaves its Space.
pub(super) fn confirm_text(entry: &Entry, space: Option<&str>) -> String {
    format!(
        "{} leaves {} and joins {} · Enter to move · Esc",
        entry.name,
        entry.owner.as_deref().unwrap_or("its Space"),
        space.unwrap_or("this window")
    )
}

/// Decide the step for a chosen entry. Pure, so the rules are tested.
pub(super) fn step(entry: &Entry, how: How) -> Step {
    match (entry.group, how) {
        (Group::Open, _) => Step::Focus,
        (_, How::NewSpace) => Step::NewSpace,
        (Group::Other, How::Enter) => Step::Confirm,
        (Group::Other, How::Confirmed) => Step::Move,
        (Group::Loose, _) => Step::Add,
        (Group::Here, _) => Step::OpenTab,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Focus,
    Confirm,
    /// Join the target Space from no Space.
    Add,
    /// Leave another Space and join the target Space.
    Move,
    /// Already owned by the target Space: open a tab for it.
    OpenTab,
    NewSpace,
}

pub(super) fn activate(host: &mut HostState, label: &str, how: How) -> Result<Outcome> {
    let picker = host.attach_picker.as_ref().context("picker closed")?;
    let entry = picker
        .entries
        .iter()
        .find(|entry| entry.label == label)
        .cloned()
        .context("session is no longer listed")?;
    let space = picker.space.clone();
    let session = entry.session.to_string();
    match step(&entry, how) {
        Step::Focus => focus(host, &session)?,
        Step::Confirm => return Ok(Outcome::Confirm(confirm_text(&entry, space.as_deref()))),
        Step::NewSpace => {
            let name = unique_space_name(&entry.name);
            run_pmux(&["space", "create", &name, "--no-attach"])?;
            let verb = if entry.owner.is_some() { "move" } else { "add" };
            run_pmux(&["space", verb, &name, "--session", &entry.name])?;
            open_space_from_host(host, &name, SpaceOpenMode::Switch);
            rail_toast(
                host,
                &format!(" {} opened in the new Space {name} ", entry.name),
            );
        }
        Step::Add | Step::Move | Step::OpenTab => {
            let Some(space) = space else {
                // The window is in no Space: a tab here, nothing to join.
                open_tab(host, &entry)?;
                return Ok(Outcome::Done);
            };
            match step(&entry, how) {
                Step::Add => run_pmux(&["space", "add", &space, "--session", &entry.name])?,
                Step::Move => run_pmux(&["space", "move", &space, "--session", &entry.name])?,
                _ => {}
            }
            if host.space_rail.current.as_deref() == Some(space.as_str()) {
                if step(&entry, how) == Step::OpenTab {
                    open_tab(host, &entry)?;
                } else {
                    // The Space view opens a tab for its new session on the
                    // next refresh; focus it when it appears.
                    host.last_space_refresh = None;
                    if let Some(owner) = host.mux.space_id.clone() {
                        host.space_team_focus = Some((owner, entry.session, Instant::now()));
                    }
                }
                let verb = match step(&entry, how) {
                    Step::Move => format!(
                        "moved here from {}",
                        entry.owner.as_deref().unwrap_or("its Space")
                    ),
                    Step::Add => format!("joined {space}"),
                    _ => "opened".into(),
                };
                rail_toast(host, &format!(" {} {verb} · new tab ", entry.name));
            } else {
                open_space_from_host(host, &space, SpaceOpenMode::Switch);
                rail_toast(host, &format!(" {} joined {space} ", entry.name));
            }
        }
    }
    Ok(Outcome::Done)
}

fn focus(host: &mut HostState, session: &str) -> Result<()> {
    let pane = host
        .attach_pane_sessions
        .iter()
        .find(|(_, id)| id.as_str() == session)
        .map(|(pane, _)| *pane)
        .context("the session's tab was closed")?;
    let window = host.mux.pane_window(pane).context("tab closed")?;
    let index = host
        .mux
        .window_ids()
        .iter()
        .position(|id| *id == window)
        .context("tab closed")?;
    host.mux.select_tab(index)?;
    host.mux.focus(pane);
    App::refit_geom(host, host.window.inner_size(), Some("attach picker focus"));
    persist_attach_layout_from_live(host);
    host.dirty = true;
    Ok(())
}

fn open_tab(host: &mut HostState, entry: &Entry) -> Result<()> {
    let program = find_mux_bin().to_string_lossy().into_owned();
    host.mux.new_tab(
        &program,
        &[
            "attach".into(),
            "--session-id".into(),
            entry.session.to_string(),
        ],
    )?;
    let pane = host.mux.focused_id();
    host.mux
        .mark_attach_session(pane, entry.session.to_string(), entry.name.clone());
    sync_attach_pane_sessions(host);
    App::refit_geom(host, host.window.inner_size(), Some("attach picker tab"));
    persist_attach_layout_from_live(host);
    host.dirty = true;
    Ok(())
}

fn unique_space_name(base: &str) -> String {
    let taken: std::collections::HashSet<String> =
        saved_spaces().into_iter().map(|(name, _)| name).collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !taken.contains(name))
        .expect("an unused name")
}

fn run_pmux(args: &[&str]) -> Result<()> {
    let mut command = pmux_capture_command();
    if let Some(socket) = host_mux_socket() {
        command.arg("--socket").arg(socket);
    }
    command.args(args).stdin(std::process::Stdio::null());
    let output = prismattyc_process::spawn(&mut command)
        .and_then(std::process::Child::wait_with_output)
        .with_context(|| format!("run pmux {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::SessionSnapshot;

    fn session(id: u64, name: &str, owner: Option<&str>, agent: bool) -> SessionSnapshot {
        SessionSnapshot {
            id,
            name: name.into(),
            agent_id: agent.then(|| "seat".into()),
            space_id: owner.map(str::to_string),
            windows: vec![],
        }
    }

    fn sample() -> Vec<Entry> {
        let snapshot = Snapshot {
            sequence: 1,
            sessions: vec![
                session(1, "logs", Some("id-work"), false),
                session(2, "api-agent", None, true),
                session(3, "build-agent", Some("id-release"), true),
                session(4, "notes", Some("id-work"), false),
                session(5, "orphan", Some("id-gone"), false),
            ],
        };
        let open = ["1".to_string()].into_iter().collect();
        let saved = vec![
            ("work".to_string(), Some("id-work".to_string())),
            ("release".to_string(), Some("id-release".to_string())),
        ];
        entries(&snapshot, &open, Some(("work", "id-work")), &saved)
    }

    #[test]
    fn sessions_group_by_where_they_are_and_sort_loose_first() {
        let entries = sample();
        let groups: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.group)).collect();
        assert_eq!(
            groups,
            vec![
                ("api-agent", Group::Loose),
                // An owner with no saved file is free to join, like space add.
                ("orphan", Group::Loose),
                ("notes", Group::Here),
                ("build-agent", Group::Other),
                ("logs", Group::Open),
            ]
        );
        let build = &entries[3];
        assert_eq!(build.owner.as_deref(), Some("release"));
        assert_eq!(build.describe, "agent seat · 0 panes · in release");
        assert_eq!(entries[0].describe, "agent seat · 0 panes · not in a Space");
        assert_eq!(entries[4].describe, "shell · 0 panes · open in this window");
    }

    #[test]
    fn enter_moves_only_after_a_confirmation_that_names_the_old_space() {
        let entries = sample();
        let build = entries.iter().find(|e| e.name == "build-agent").unwrap();
        assert_eq!(step(build, How::Enter), Step::Confirm);
        assert_eq!(step(build, How::Confirmed), Step::Move);
        assert_eq!(
            confirm_text(build, Some("work")),
            "build-agent leaves release and joins work · Enter to move · Esc"
        );
        let api = entries.iter().find(|e| e.name == "api-agent").unwrap();
        assert_eq!(step(api, How::Enter), Step::Add);
        let notes = entries.iter().find(|e| e.name == "notes").unwrap();
        assert_eq!(step(notes, How::Enter), Step::OpenTab);
        let logs = entries.iter().find(|e| e.name == "logs").unwrap();
        assert_eq!(step(logs, How::Enter), Step::Focus);
        assert_eq!(step(logs, How::NewSpace), Step::Focus);
        assert_eq!(step(api, How::NewSpace), Step::NewSpace);
        assert_eq!(step(build, How::NewSpace), Step::NewSpace);
    }

    #[test]
    fn rows_say_what_enter_does() {
        assert_eq!(Group::Loose.chord(), "Open here");
        assert_eq!(Group::Here.chord(), "Open here");
        assert_eq!(Group::Other.chord(), "Move here");
        assert_eq!(Group::Open.chord(), "Focus");
    }
}
