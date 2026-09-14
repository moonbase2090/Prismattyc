//! Optional team details in the existing keyboard-accessible Space menu.

use super::*;
use prismattyc_mux::space_team::TeamDetails;
use prismattyc_mux::space_template::TemplatePreview;
use std::sync::mpsc::{self, Receiver};

#[derive(Clone)]
enum Choice {
    Maintenance,
    Command(Vec<String>),
    None,
    Settings,
    Preference(String, String),
    Back,
    Session(String),
    View(String),
    Reopen(String),
    Attention {
        session: String,
        pane: u64,
        revision: u64,
        resolve: bool,
    },
    Role(String),
    LinkLabel,
    LinkTarget(String),
    Link(String),
    SaveTemplate,
    Templates,
    Template(String),
    Create {
        template: String,
        name: String,
        launch: bool,
    },
    Result,
    Retry,
}

#[derive(Clone)]
enum Page {
    Maintenance,
    Settings,
    Team,
    Session(String),
    Templates,
    Preview(String, String),
    Text(String),
}

#[derive(Clone)]
struct Input {
    label: String,
    value: String,
    select_all: bool,
    choice: Choice,
}
struct Row {
    label: String,
    detail: String,
    choice: Choice,
}
enum Payload {
    Details(TeamDetails),
    Templates(Vec<String>),
    Preview(TemplatePreview),
    Text(String),
    Created(String),
}

pub(super) struct Panel {
    space: String,
    maintenance: bool,
    owner: Option<String>,
    details: Option<TeamDetails>,
    page: Page,
    rows: Vec<Row>,
    input: Option<Input>,
    submitted: Option<Input>,
    pending: Option<Receiver<Result<Payload, String>>>,
    pub scroll: usize,
    columns: usize,
}

impl Panel {
    fn row(&mut self, label: impl Into<String>, detail: impl Into<String>, choice: Choice) {
        let label = label.into();
        let detail = detail.into();
        if matches!(choice, Choice::None) {
            let text = if detail.is_empty() {
                label
            } else {
                format!("{label}: {detail}")
            };
            for line in wrap_text(&text, self.columns.max(12)) {
                self.rows.push(Row {
                    label: line,
                    detail: String::new(),
                    choice: Choice::None,
                });
            }
        } else if space_rail::name_cells(&detail) > self.columns.saturating_sub(36).max(12) {
            self.rows.push(Row {
                label,
                detail: String::new(),
                choice,
            });
            for line in wrap_text(&detail, self.columns.max(12)) {
                self.rows.push(Row {
                    label: line,
                    detail: String::new(),
                    choice: Choice::None,
                });
            }
        } else {
            self.rows.push(Row {
                label,
                detail,
                choice,
            });
        }
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        match self.page.clone() {
            Page::Maintenance => {
                self.row(
                    "Update source",
                    "Moonbase2090/Prismattyc releases, starting at 0.2.0",
                    Choice::None,
                );
                for (label, detail, args) in [
                    (
                        "Check for updates",
                        "Compare installed and available release versions",
                        vec!["update", "--check"],
                    ),
                    (
                        "Install latest release",
                        "Verify and install all components; running sessions continue",
                        vec!["update"],
                    ),
                    (
                        "Roll back update",
                        "Restore the previous installed version",
                        vec!["update", "--rollback"],
                    ),
                    (
                        "Installed and running versions",
                        "See which components need a restart",
                        vec!["versions"],
                    ),
                    (
                        "Restart",
                        "Restart safe components; defer active PTY owners",
                        vec!["restart"],
                    ),
                    (
                        "Restart host",
                        "Restore windows; defer while blank terminals are open",
                        vec!["restart", "--host"],
                    ),
                    (
                        "Restart MCP adapters",
                        "Keep the connection; never replay in-flight operations",
                        vec!["restart", "--mcp"],
                    ),
                    (
                        "Restart daemon",
                        "Restart only if no sessions exist",
                        vec!["restart", "--daemon"],
                    ),
                ] {
                    self.row(
                        label,
                        detail,
                        Choice::Command(args.into_iter().map(str::to_owned).collect()),
                    );
                }
            }
            Page::Settings => {
                let config = config::load(&config::config_path()).unwrap_or_default();
                for side in ["bottom", "left", "top", "right"] {
                    self.row(
                        format!("Rail: {side}"),
                        if config.space_rail().as_str() == side {
                            "Selected"
                        } else {
                            "Move the rail to this edge"
                        },
                        Choice::Preference("space_rail".into(), side.into()),
                    );
                }
                let restore = config.restore_blank_terminals.unwrap_or(false);
                self.row(
                    format!("Restore blanks: {}", if restore { "on" } else { "off" }),
                    "Recreate fresh shells, working directories, and split layouts on restore",
                    Choice::Preference("restore_blank_terminals".into(), (!restore).to_string()),
                );
                let enabled = config.space_autosave.unwrap_or(false);
                self.row(
                    format!("Autosave: {}", if enabled { "on" } else { "off" }),
                    "Save changed layouts after two idle seconds",
                    Choice::Preference("space_autosave".into(), (!enabled).to_string()),
                );
                for (value, label, detail) in [
                    (
                        "blank",
                        "Session names: blank",
                        "Open a local shell without a popup or session",
                    ),
                    (
                        "ask",
                        "Session names: ask",
                        "Show the naming popup for new panes and tabs",
                    ),
                    (
                        "auto",
                        "Session names: automatic",
                        "Use suggested names without a popup",
                    ),
                ] {
                    self.row(
                        label,
                        if config.session_naming.as_deref().unwrap_or("ask") == value {
                            "Selected"
                        } else {
                            detail
                        },
                        Choice::Preference("session_naming".into(), value.into()),
                    );
                }
                for (value, label, detail) in [
                    ("ask", "Startup: ask", "Choose each time"),
                    (
                        "restore",
                        "Startup: restore",
                        "Reconnect live sessions; leave stopped sessions stopped",
                    ),
                    ("fresh", "Startup: fresh", "Open a fresh window"),
                ] {
                    self.row(
                        label,
                        if config.space_startup.as_deref().unwrap_or("ask") == value {
                            "Selected"
                        } else {
                            detail
                        },
                        Choice::Preference("space_startup".into(), value.into()),
                    );
                }
            }
            Page::Team => {
                if let Some(details) = self.details.clone() {
                    self.row(
                        format!(
                            "{} · {}",
                            waiting_label(details.sessions_needing_input),
                            count_label(details.letters, "letter")
                        ),
                        if details.source == "live daemon snapshot" {
                            "Up to date"
                        } else {
                            "Offline — last known state"
                        },
                        Choice::None,
                    );
                    for session in details.sessions {
                        let reason = session
                            .attention
                            .iter()
                            .map(|r| r.message.as_str())
                            .collect::<Vec<_>>()
                            .join("; ");
                        self.row(
                            session.name.clone(),
                            format!(
                                "{}{} · {}{}{}",
                                session
                                    .role
                                    .as_ref()
                                    .map(|r| format!("{r} · "))
                                    .unwrap_or_default(),
                                session.state,
                                count_label(session.letters, "letter"),
                                if reason.is_empty() { "" } else { " · " },
                                reason
                            ),
                            Choice::Session(session.name),
                        );
                    }
                    for (label, target) in details.links {
                        self.row(label, target.clone(), Choice::Link(target));
                    }
                }
                self.row(
                    "Last open result",
                    "Read details and recovery actions",
                    Choice::Result,
                );
                self.row(
                    "Add context link",
                    "Repository, design, worktree, or restart note",
                    Choice::LinkLabel,
                );
                self.row(
                    "Save team template",
                    "Save this definition without starting work",
                    Choice::SaveTemplate,
                );
                self.row(
                    "Team templates",
                    "Preview an independent team before creating it",
                    Choice::Templates,
                );
                self.row(
                    "Spaces settings…",
                    "Position, autosave, and startup",
                    Choice::Settings,
                );
            }
            Page::Session(name) => {
                if let Some(session) = self
                    .details
                    .as_ref()
                    .and_then(|d| d.sessions.iter().find(|s| s.name == name))
                    .cloned()
                {
                    if session.state == "running" {
                        self.row(
                            "View session",
                            "Focus its current pane; leave mail unchanged",
                            Choice::View(name.clone()),
                        );
                    } else if session.state == "stopped" {
                        self.row(
                            "Reopen session",
                            "Reopen only this saved session",
                            Choice::Reopen(name.clone()),
                        );
                    } else {
                        self.row("Session unavailable", session.state, Choice::None);
                    }
                    self.row(
                        "Set role",
                        session.role.unwrap_or_default(),
                        Choice::Role(name.clone()),
                    );
                    for request in session.attention {
                        let now = prismattyc_mux::host_render_status::unix_ms();
                        self.row(
                            request.message.clone(),
                            format!(
                                "{} · {}",
                                "Request",
                                if request.needs_input(now) {
                                    "needs input"
                                } else {
                                    "snoozed"
                                }
                            ),
                            Choice::None,
                        );
                        self.row(
                            "Resolve request",
                            "Mail delivery stays unchanged",
                            Choice::Attention {
                                session: name.clone(),
                                pane: request.pane_id,
                                revision: request.revision,
                                resolve: true,
                            },
                        );
                        self.row(
                            "Snooze for 10 minutes",
                            "Keep the request; pause its reminder",
                            Choice::Attention {
                                session: name.clone(),
                                pane: request.pane_id,
                                revision: request.revision,
                                resolve: false,
                            },
                        );
                    }
                }
                self.row("Back to team", "", Choice::Back);
            }
            Page::Text(text) => {
                for line in text.lines() {
                    self.row(line, "", Choice::None);
                }
                if self.maintenance {
                    self.row("Back to update and restart", "", Choice::Maintenance);
                } else {
                    self.row("Back to team", "", Choice::Back);
                }
            }
            Page::Templates | Page::Preview(..) => {}
        }
    }
}

pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty()
            && space_rail::name_cells(&line) + 1 + space_rail::name_cells(word) > width
        {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        for ch in word.chars() {
            if space_rail::name_cells(&line) + space_rail::name_cells(&ch.to_string()) > width {
                lines.push(std::mem::take(&mut line));
            }
            line.push(ch);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn count_label(n: u32, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
fn waiting_label(n: usize) -> String {
    match n {
        0 => "No sessions need you".into(),
        1 => "1 session needs you".into(),
        _ => format!("{n} sessions need you"),
    }
}

pub(super) fn maintenance(host: &mut HostState) {
    settings(host);
    let panel = host.space_panel.as_mut().unwrap();
    panel.maintenance = true;
    panel.page = Page::Maintenance;
    panel.rebuild();
    let first = panel
        .rows
        .iter()
        .position(|r| matches!(r.choice, Choice::Command(_)))
        .unwrap_or(0);
    if let Some(menu) = host.context_menu.as_mut() {
        menu.selected = first;
    }
}

pub(super) fn settings(host: &mut HostState) {
    open(host, host.space_rail.current.clone().unwrap_or_default());
    let panel = host.space_panel.as_mut().unwrap();
    panel.pending = None;
    panel.page = Page::Settings;
    panel.rebuild();
}

fn command(host: &mut HostState, args: Vec<String>, kind: &'static str) {
    let Some(panel) = host.space_panel.as_mut() else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    panel.pending = Some(rx);
    panel.rows.clear();
    panel.row("Loading…", "Your sessions keep running", Choice::None);
    let bin = pmux_bin();
    let socket = host_mux_socket();
    std::thread::spawn(move || {
        let result = (|| -> Result<Payload, String> {
            let mut cmd = std::process::Command::new(bin);
            if let Some(socket) = socket {
                cmd.arg("--socket").arg(socket);
            }
            let output = cmd
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().into());
            }
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            match kind {
                "maintenance" => Ok(Payload::Text(maintenance_result(&text))),
                "details" => serde_json::from_str(&text)
                    .map(Payload::Details)
                    .map_err(|e| e.to_string()),
                "preview" => serde_json::from_str(&text)
                    .map(Payload::Preview)
                    .map_err(|e| e.to_string()),
                "templates" => Ok(Payload::Templates(
                    text.lines().map(str::to_string).collect(),
                )),
                "created" => Ok(Payload::Created(text)),
                _ => Ok(Payload::Text(text)),
            }
        })();
        let _ = tx.send(result);
    });
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn open(host: &mut HostState, name: String) {
    let owner = load_space(&spaces_dir(), &name).ok().and_then(|s| s.id);
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::SpaceChip));
    host.context_menu_target = Some(ContextMenuTarget::SpaceChip(0));
    host.palette = None;
    host.space_picker = None;
    host.space_panel = Some(Panel {
        space: name.clone(),
        maintenance: false,
        owner,
        details: None,
        page: Page::Team,
        rows: vec![],
        input: None,
        submitted: None,
        pending: None,
        scroll: 0,
        columns: (host.window.inner_size().width as usize / host.font.cell_w.max(1))
            .saturating_sub(10),
    });
    command(
        host,
        vec!["space".into(), "details".into(), name, "--json".into()],
        "details",
    );
}

pub(super) fn rows(host: &HostState) -> Option<(String, Vec<PaletteRow>)> {
    let panel = host.space_panel.as_ref()?;
    let header = if let Some(input) = &panel.input {
        format!("{} — {}", panel.space, input.label)
    } else {
        match &panel.page {
            Page::Maintenance => "UPDATE AND RESTART".into(),
            Page::Text(_) if panel.maintenance => "UPDATE AND RESTART RESULT".into(),
            Page::Settings => "SPACES SETTINGS".into(),
            Page::Session(name) => format!("{} — {name}", panel.space),
            Page::Templates => "TEAM TEMPLATES".into(),
            Page::Preview(_, name) => format!("PREVIEW {name}"),
            _ => format!("{} — TEAM DETAILS", panel.space),
        }
    };
    let rows = if let Some(input) = &panel.input {
        vec![PaletteRow::plain(
            input.value.clone(),
            "Enter continue · Escape cancel · Ctrl+A select all".into(),
            String::new(),
        )]
    } else {
        panel
            .rows
            .iter()
            .map(|r| {
                let mut row = PaletteRow::plain(r.label.clone(), r.detail.clone(), String::new());
                row.full_width = matches!(r.choice, Choice::None);
                row
            })
            .collect()
    };
    Some((header, rows))
}

pub(super) fn status(host: &HostState) -> Option<serde_json::Value> {
    let (header, rows) = rows(host)?;
    Some(
        serde_json::json!({"header":header,"rows":rows.iter().map(|row| format!("{}: {}", row.name, row.describe)).collect::<Vec<_>>(),
        "loading":host.space_panel.as_ref()?.pending.is_some()}),
    )
}

pub(super) fn poll(host: &mut HostState) {
    focus_pending(host);
    let Some(panel) = host.space_panel.as_mut() else {
        return;
    };
    let result = panel.pending.as_ref().and_then(|rx| match rx.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Disconnected) => Some(Err("team helper stopped".into())),
        Err(mpsc::TryRecvError::Empty) => None,
    });
    let Some(result) = result else {
        return;
    };
    panel.pending = None;
    panel.scroll = 0;
    let mut open_created = None;
    match result {
        Ok(Payload::Details(details)) => {
            if panel.owner.is_some() && panel.owner != details.space_id {
                panel.page =
                    Page::Text("This Space was replaced. Open its current details again.".into());
            } else {
                panel.details = Some(details);
            }
            panel.rebuild();
        }
        Ok(Payload::Templates(names)) => {
            panel.rows.clear();
            for name in names {
                panel.row(
                    name.clone(),
                    "Choose a new Space name, then preview",
                    Choice::Template(name),
                );
            }
            panel.row("Back to team", "", Choice::Back);
        }
        Ok(Payload::Preview(preview)) => {
            panel.rows.clear();
            for launch in &preview.launches {
                panel.row(launch.session.clone(), "", Choice::None);
                panel.row(
                    "Directory",
                    launch
                        .cwd
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "default directory".into()),
                    Choice::None,
                );
                panel.row(
                    "Program",
                    launch.program.as_deref().unwrap_or("default shell"),
                    Choice::None,
                );
                panel.row(
                    "Command",
                    launch.command.as_deref().unwrap_or("none"),
                    Choice::None,
                );
            }
            for notice in &preview.notices {
                panel.row("Note", notice, Choice::None);
            }
            for conflict in &preview.conflicts {
                panel.row("Conflict", conflict, Choice::None);
            }
            if preview.conflicts.is_empty() {
                if let Page::Preview(template, name) = panel.page.clone() {
                    panel.row(
                        "Create shells only",
                        "Do not start saved commands",
                        Choice::Create {
                            template: template.clone(),
                            name: name.clone(),
                            launch: false,
                        },
                    );
                    panel.row(
                        "Create and launch",
                        "Run the commands shown above in independent sessions",
                        Choice::Create {
                            template,
                            name,
                            launch: true,
                        },
                    );
                }
            }
            if let Page::Preview(template, _) = panel.page.clone() {
                panel.row(
                    "Change Space name",
                    "Return to the name field",
                    Choice::Template(template),
                );
            }
            panel.row("Back to team", "", Choice::Back);
        }
        Ok(Payload::Created(text)) => {
            if let Page::Preview(_, name) = &panel.page {
                open_created = Some(name.clone());
            }
            panel.page = Page::Text(text);
            panel.rebuild();
        }
        Ok(Payload::Text(text)) => {
            panel.submitted = None;
            panel.page = Page::Text(text);
            panel.rebuild();
        }
        Err(text) => {
            if let Some(mut input) = panel.submitted.take() {
                input.label = format!("{} — {text}", input.label);
                panel.input = Some(input);
            } else {
                panel.page = Page::Text(text);
                panel.rebuild();
            }
        }
    }
    if let Some(menu) = &mut host.context_menu {
        menu.selected = 0;
    }
    host.dirty = true;
    host.window.request_redraw();
    if let Some(name) = open_created {
        close_context_menu(host);
        open_space_from_host(host, &name, SpaceOpenMode::Switch);
    }
}

pub(super) fn activate(host: &mut HostState, index: usize) {
    let Some(panel) = host.space_panel.as_mut() else {
        return;
    };
    let choice = panel
        .rows
        .get(index)
        .map(|r| r.choice.clone())
        .unwrap_or(Choice::None);
    let name = panel.space.clone();
    match choice {
        Choice::Maintenance => {
            panel.page = Page::Maintenance;
            panel.rebuild();
        }
        Choice::Command(args) => {
            command(host, args, "maintenance");
        }
        Choice::None => return,
        Choice::Settings => {
            panel.page = Page::Settings;
            panel.rebuild();
        }
        Choice::Preference(key, value) => {
            let item = if key == "space_autosave" || key == "restore_blank_terminals" {
                toml_edit::value(value == "true")
            } else {
                toml_edit::value(value)
            };
            match config::save_preference(&config::config_path(), &key, item) {
                Ok(()) => panel.rebuild(),
                Err(error) => {
                    panel.page = Page::Text(format!("Could not save preference: {error}"));
                    panel.rebuild();
                }
            }
        }
        Choice::Back => {
            panel.page = Page::Team;
            command(
                host,
                vec!["space".into(), "details".into(), name, "--json".into()],
                "details",
            );
        }
        Choice::Session(session) => {
            panel.page = Page::Session(session);
            panel.rebuild();
        }
        Choice::Role(session) => {
            panel.input = Some(Input {
                label: "Role".into(),
                value: String::new(),
                select_all: true,
                choice: Choice::Role(session),
            })
        }
        Choice::LinkLabel => {
            panel.input = Some(Input {
                label: "Link label".into(),
                value: String::new(),
                select_all: true,
                choice: Choice::LinkLabel,
            })
        }
        Choice::SaveTemplate => {
            panel.input = Some(Input {
                label: "Template name".into(),
                value: name,
                select_all: true,
                choice: Choice::SaveTemplate,
            })
        }
        Choice::Template(template) => {
            panel.input = Some(Input {
                label: "New Space name".into(),
                value: format!("{template}-team"),
                select_all: true,
                choice: Choice::Template(template),
            })
        }
        Choice::Templates => {
            panel.page = Page::Templates;
            command(
                host,
                vec!["space".into(), "template".into(), "ls".into()],
                "templates",
            );
        }
        Choice::Attention {
            session,
            pane,
            revision,
            resolve,
        } => command(
            host,
            vec![
                "space".into(),
                "attention".into(),
                if resolve { "resolve" } else { "snooze" }.into(),
                name,
                session,
                pane.to_string(),
                revision.to_string(),
            ],
            "text",
        ),
        Choice::Reopen(session) => command(
            host,
            vec![
                "session".into(),
                "reopen".into(),
                session,
                "--space".into(),
                name,
            ],
            "text",
        ),
        Choice::Link(target) => {
            if !hyperlink::spawn_open(&target) {
                panel.page = Page::Text(format!("Could not open {target}"));
                panel.rebuild();
            }
        }
        Choice::Result => {
            let text = host
                .last_space_open
                .as_ref()
                .filter(|r| r.name == name)
                .and_then(|r| serde_json::to_value(r).ok())
                .map(|r| prismattyc_mux::space_team::result_text(&r));
            let Some(text) = text else {
                command(host, vec!["space".into(), "result".into(), name], "text");
                return;
            };
            panel.page = Page::Text(text);
            panel.rebuild();
            panel.row(
                "Retry opening this Space",
                "Reuse existing sessions; do not repeat their commands",
                Choice::Retry,
            );
        }
        Choice::Retry => {
            close_context_menu(host);
            host.space_opens.enqueue(&name, SpaceOpenMode::Switch);
            advance_space_opens(host);
        }
        Choice::View(session) => {
            // Refresh ownership before selecting a live local replica.
            let snapshot = attach_log::live_snapshot();
            let live = snapshot.as_ref().and_then(|s| {
                s.sessions
                    .iter()
                    .find(|s| s.name == session && s.space_id == panel.owner)
            });
            if let Some(live) = live {
                let target = host
                    .attach_pane_sessions
                    .iter()
                    .find(|(_, id)| **id == live.id.to_string())
                    .map(|(p, _)| *p);
                if let Some(target) = target {
                    let tab = host
                        .mux
                        .tab_panes()
                        .iter()
                        .position(|(_, panes)| panes.contains(&target));
                    close_context_menu(host);
                    if let Some(tab) = tab {
                        let _ = host.mux.seed_tab_and_focus(tab, Some(target));
                    }
                    mark_layout_dirty(host);
                } else if let Some(owner) = panel.owner.clone() {
                    host.space_team_focus = Some((owner, live.id, Instant::now()));
                    close_context_menu(host);
                    host.space_opens.enqueue(&name, SpaceOpenMode::Switch);
                    advance_space_opens(host);
                }
            } else {
                panel.page =
                    Page::Text("Session moved or stopped. Refresh the team details.".into());
                panel.rebuild();
            }
        }
        Choice::Create {
            template,
            name,
            launch,
        } => {
            let mut args = vec![
                "space".into(),
                "template".into(),
                "create".into(),
                template,
                name,
                "--prepare-only".into(),
            ];
            if launch {
                args.push("--launch".into());
            }
            command(host, args, "created");
        }
        Choice::LinkTarget(_) => return,
    }
    if let Some(menu) = &mut host.context_menu {
        menu.selected = 0;
    }
    host.dirty = true;
    host.window.request_redraw();
}

pub(super) fn input_key(host: &mut HostState, event: &winit::event::KeyEvent) -> bool {
    let Some(panel) = host.space_panel.as_mut() else {
        return false;
    };
    let Some(input) = panel.input.as_mut() else {
        return false;
    };
    match &event.logical_key {
        Key::Named(NamedKey::Escape) => {
            panel.input = None;
        }
        Key::Named(NamedKey::Backspace) => {
            if input.select_all {
                input.value.clear();
                input.select_all = false;
            } else {
                input.value.pop();
            }
        }
        Key::Character(text) if host.modifiers.control_key() && text.eq_ignore_ascii_case("a") => {
            input.select_all = true
        }
        Key::Character(text) if host.modifiers.control_key() && text.eq_ignore_ascii_case("v") => {
            if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                if input.select_all {
                    input.value.clear();
                    input.select_all = false;
                }
                for ch in text.chars().filter(|c| !c.is_control()) {
                    if input.value.len() + ch.len_utf8() > 4096 {
                        break;
                    }
                    input.value.push(ch);
                }
            }
        }
        Key::Character(text)
            if !host.modifiers.control_key()
                && !host.modifiers.alt_key()
                && !host.modifiers.super_key() =>
        {
            if input.select_all {
                input.value.clear();
                input.select_all = false;
            }
            if input.value.len() + text.len() <= 4096 && !text.chars().any(char::is_control) {
                input.value.push_str(text);
            }
        }
        Key::Named(NamedKey::Enter) => {
            if panel.pending.is_some() {
                return true;
            }
            panel.submitted = panel.input.clone();
            let Input { value, choice, .. } = panel.input.take().unwrap();
            let name = panel.space.clone();
            match choice {
                Choice::Role(session) => command(
                    host,
                    vec!["space".into(), "role".into(), name, session, value],
                    "text",
                ),
                Choice::LinkLabel => {
                    panel.input = Some(Input {
                        label: "HTTP(S) link or absolute path".into(),
                        value: String::new(),
                        select_all: true,
                        choice: Choice::LinkTarget(value),
                    })
                }
                Choice::LinkTarget(label) => command(
                    host,
                    vec!["space".into(), "link".into(), name, label, value],
                    "text",
                ),
                Choice::SaveTemplate => command(
                    host,
                    vec![
                        "space".into(),
                        "template".into(),
                        "save".into(),
                        value,
                        "--space".into(),
                        name,
                    ],
                    "text",
                ),
                Choice::Template(template) => {
                    panel.page = Page::Preview(template.clone(), value.clone());
                    command(
                        host,
                        vec![
                            "space".into(),
                            "template".into(),
                            "preview".into(),
                            template,
                            value,
                            "--json".into(),
                        ],
                        "preview",
                    );
                }
                _ => {}
            }
        }
        _ => {}
    }
    host.dirty = true;
    host.window.request_redraw();
    true
}

#[derive(Default)]
pub(super) struct AttentionFeed {
    pending: Option<Receiver<Vec<prismattyc_mux::team_attention::AttentionRequest>>>,
    last: Option<Instant>,
    requests: Vec<prismattyc_mux::team_attention::AttentionRequest>,
}

pub(super) fn attention_requests(
    host: &mut HostState,
) -> Vec<prismattyc_mux::team_attention::AttentionRequest> {
    let feed = &mut host.team_attention_feed;
    if let Some(rx) = &feed.pending {
        match rx.try_recv() {
            Ok(requests) => {
                feed.requests = requests;
                feed.pending = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                feed.requests.clear();
                feed.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    if feed.pending.is_none()
        && feed
            .last
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
    {
        feed.last = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        feed.pending = Some(rx);
        let socket = host_mux_socket();
        std::thread::spawn(move || {
            let requests = socket
                .and_then(|s| prismattyc_mux::space_team::TeamClient::connect(&s).ok())
                .and_then(|mut c| c.requests().ok())
                .unwrap_or_default();
            let _ = tx.send(requests);
        });
    }
    feed.requests.clone()
}

/// Wait for the requested view, then recheck ownership before focusing it.
fn focus_pending(host: &mut HostState) {
    let Some((owner, session, started)) = host.space_team_focus.clone() else {
        return;
    };
    if started.elapsed() > Duration::from_secs(15) {
        host.space_team_focus = None;
        rail_toast(host, " Session unavailable; refresh its team details ");
        return;
    }
    if host.space_opens.busy() || host.mux.space_id.as_ref() != Some(&owner) {
        return;
    }
    let Some(snapshot) = attach_log::live_snapshot() else {
        return;
    };
    if !snapshot
        .sessions
        .iter()
        .any(|s| s.id == session && s.space_id.as_ref() == Some(&owner))
    {
        host.space_team_focus = None;
        rail_toast(host, " Session moved or stopped; refresh its team details ");
        return;
    }
    let target = host
        .attach_pane_sessions
        .iter()
        .find(|(_, id)| **id == session.to_string())
        .map(|(p, _)| *p);
    if let Some(target) = target {
        if let Some(tab) = host
            .mux
            .tab_panes()
            .iter()
            .position(|(_, panes)| panes.contains(&target))
        {
            let _ = host.mux.seed_tab_and_focus(tab, Some(target));
            host.space_team_focus = None;
            mark_layout_dirty(host);
        }
    }
}

fn maintenance_result(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_owned();
    };
    let mut lines = Vec::new();
    if let Some(components) = value["components"].as_array() {
        for component in components {
            let result = component.get("response").unwrap_or(component);
            lines.push(format!(
                "{}: {}. {}",
                component["component"].as_str().unwrap_or("Component"),
                result["status"].as_str().unwrap_or("unknown"),
                result["detail"].as_str().unwrap_or("")
            ));
        }
    } else if let Some(installed) = value["installed"].as_array() {
        for component in installed {
            lines.push(format!(
                "Installed {}: {}",
                component["component"].as_str().unwrap_or("component"),
                component["version"].as_str().unwrap_or("unavailable")
            ));
        }
        lines.push(format!(
            "Running daemon: {}",
            value["daemon"]["version"].as_str().unwrap_or("unknown")
        ));
        if let Some(running) = value["running"].as_array() {
            for component in running {
                lines.push(format!(
                    "Running {} (PID {}): {}",
                    component["component"].as_str().unwrap_or("component"),
                    component["pid"],
                    component["version"].as_str().unwrap_or("unknown")
                ));
            }
        }
    } else {
        lines.push(format!(
            "{}: {}",
            value["status"].as_str().unwrap_or("Result"),
            value["version"].as_str().unwrap_or("")
        ));
    }
    lines.join("\n")
}
