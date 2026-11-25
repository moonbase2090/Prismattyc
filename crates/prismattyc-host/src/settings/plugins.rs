// SPDX-License-Identifier: MIT
//! Native plug-in management. The daemon owns every durable change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc, Mutex,
};

use anyhow::{bail, ensure, Context, Result};
use prismattyc_mux::plugins::{
    self as native, AdminAction, CatalogEntry, Client, ConnectionCloser, Endpoint, Installed,
    Manifest, Observed, PendingPlugin, Reply, Request, Status,
};

use crate::palette::PaletteRow;

#[derive(Default)]
struct Snapshot {
    online: bool,
    catalog: Vec<CatalogEntry>,
    installed: Vec<Installed>,
    states: Vec<Status>,
    endpoints: Vec<Endpoint>,
    inspected: Option<Manifest>,
    /// What an update run did, in words, for the notice line.
    summary: Option<String>,
    /// Official plug-ins the installed release changes at the next daemon restart.
    restart: Vec<PendingPlugin>,
    /// Why that could not be read, when it could not.
    restart_unknown: Option<String>,
}

struct Draft {
    id: String,
    version: String,
    grants: BTreeSet<String>,
    /// The manifest's required permissions have been pre-selected once.
    seeded: bool,
}

/// What a request to the daemon does besides reading its state.
enum Job {
    Admin(Request),
    Update {
        only: Option<String>,
        enable_off: bool,
    },
}

/// A palette command waiting for the page's first load.
#[derive(Clone, Copy)]
pub(crate) enum Command {
    Check,
    UpdateAll,
}

#[derive(Clone)]
enum Action {
    None,
    Load(bool),
    Open(String),
    Back,
    Version(String),
    Grant(String),
    Install,
    Enable,
    Disable,
    Update {
        enable_off: bool,
    },
    UpdateAll,
    /// Open Update and restart, where the daemon restart happens.
    RestartNow,
}

struct Row {
    label: String,
    detail: String,
    action: Action,
}

struct Pending {
    receive: Receiver<std::result::Result<Snapshot, String>>,
    stopped: Arc<AtomicBool>,
    closer: Arc<Mutex<Option<ConnectionCloser>>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(closer) = self.closer.lock().unwrap().as_ref() {
            closer.close();
        }
    }
}

pub(super) struct Page {
    socket: Option<PathBuf>,
    wake: crate::mux::Wake,
    snapshot: Option<Snapshot>,
    manifests: BTreeMap<(String, String), Manifest>,
    draft: Option<Draft>,
    pending: Option<Pending>,
    entered: bool,
    notice: String,
    queued: Option<Command>,
    /// Restart now was chosen; the host opens Update and restart.
    restart_requested: bool,
}

impl Page {
    pub fn new() -> Self {
        Self {
            socket: None,
            wake: Arc::new(|| {}),
            snapshot: None,
            manifests: BTreeMap::new(),
            draft: None,
            pending: None,
            entered: false,
            notice: String::new(),
            queued: None,
            restart_requested: false,
        }
    }

    /// Choose Restart now, as activating its row does.
    #[cfg(all(test, target_os = "linux"))]
    pub fn request_restart(&mut self) {
        self.restart_requested = true;
    }

    /// Whether Restart now was chosen since the last call.
    pub fn take_restart_request(&mut self) -> bool {
        std::mem::take(&mut self.restart_requested)
    }

    /// Run a palette command on this page: now when it is idle, after the
    /// first load otherwise.
    pub(crate) fn command(&mut self, command: Command) {
        if self.entered && self.pending.is_none() {
            self.run(command);
        } else {
            self.queued = Some(command);
            self.enter();
        }
    }

    fn run(&mut self, command: Command) {
        match command {
            Command::Check => self.start(None, true),
            Command::UpdateAll => self.start_update(None, false),
        }
    }

    fn start_update(&mut self, only: Option<String>, enable_off: bool) {
        self.start(Some(Job::Update { only, enable_off }), false);
    }

    pub fn bind(&mut self, socket: Option<PathBuf>, wake: crate::mux::Wake) {
        self.socket = socket;
        self.wake = wake;
    }

    pub fn enter(&mut self) {
        if !self.entered {
            self.entered = true;
            self.start(None, false);
        }
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    fn items(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut row = |label: String, detail: String, action| {
            rows.push(Row {
                label,
                detail,
                action,
            })
        };
        let busy = self.pending.is_some();
        let online = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.online);
        if let Some(draft) = &self.draft {
            row("Back to plug-ins".into(), draft.id.clone(), Action::Back);
            if let Some(state) = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.states.iter().find(|state| state.id == draft.id))
            {
                let observed = match state.observed {
                    Observed::Stopped if state.enabled => "Enabled; starts when used",
                    Observed::Stopped => "Disabled",
                    Observed::Starting => "Starting",
                    Observed::Running => "Running",
                    Observed::Failed => "Failed",
                    Observed::Incompatible => "Incompatible with this release",
                    Observed::DaemonUnavailable => "Daemon unavailable; saved state shown",
                };
                row(
                    observed.into(),
                    state.failure.clone().unwrap_or_else(|| {
                        state
                            .selected
                            .as_ref()
                            .map(|package| format!("Selected version {}", package.version))
                            .unwrap_or_else(|| "No version selected".into())
                    }),
                    Action::None,
                );
            } else {
                row(
                    "Disabled".into(),
                    "Installing a package never enables it".into(),
                    Action::None,
                );
            }
            let state = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.states.iter().find(|state| state.id == draft.id));
            let update = self.update_for(&draft.id);
            let in_use = state
                .and_then(|state| state.selected.as_ref())
                .map(|package| package.version.as_str());
            row(
                "Version".into(),
                match (in_use, update.as_deref()) {
                    (Some(have), Some(newest)) => format!("Installed {have}; newest {newest}"),
                    (Some(have), None) => format!("Installed {have}; this is the newest"),
                    (None, Some(newest)) => format!("Not installed; newest {newest}"),
                    (None, None) => "Not installed".into(),
                },
                Action::None,
            );
            if let Some(newest) = update.as_deref() {
                let on = state.is_some_and(|state| state.enabled);
                row(
                    format!("Update to {newest}"),
                    if on {
                        "Installs it and keeps the permissions already granted"
                    } else {
                        "Installs it; the plug-in stays off, as you left it"
                    }
                    .into(),
                    if online && !busy {
                        Action::Update { enable_off: false }
                    } else {
                        Action::None
                    },
                );
                if !on {
                    row(
                        format!("Update to {newest} and enable"),
                        "Installs it and turns it on with its recommended permissions".into(),
                        if online && !busy {
                            Action::Update { enable_off: true }
                        } else {
                            Action::None
                        },
                    );
                }
            }
            if let Some(snapshot) = &self.snapshot {
                let versions = snapshot
                    .catalog
                    .iter()
                    .filter(|entry| entry.id == draft.id)
                    .map(|entry| entry.version.clone())
                    .chain(
                        snapshot
                            .installed
                            .iter()
                            .filter(|entry| entry.id == draft.id)
                            .map(|entry| entry.version.clone()),
                    )
                    .collect::<BTreeSet<_>>();
                for version in versions {
                    row(
                        format!("Version {version}"),
                        if version == draft.version {
                            "Chosen for review".into()
                        } else {
                            "Choose another version".into()
                        },
                        Action::Version(version),
                    );
                }
            }
            let manifest = self
                .manifests
                .get(&(draft.id.clone(), draft.version.clone()));
            let wants = |name: &str| {
                manifest.is_some_and(|manifest| {
                    manifest.requires.contains_key(name) || manifest.optional.contains_key(name)
                })
            };
            let required = manifest.map(Manifest::required_grants).unwrap_or_default();
            let on = |scope: &str| {
                if draft.grants.contains(scope) {
                    "On"
                } else {
                    "Off"
                }
            };
            let tag = |scope: &str| {
                if required.iter().any(|grant| grant == scope) {
                    " (recommended)"
                } else {
                    ""
                }
            };
            row(
                "Permissions".into(),
                if manifest.is_some() {
                    "Press Enter on a row to turn it on or off; Enable applies them".into()
                } else {
                    "Install this version to review its permissions".into()
                },
                Action::None,
            );
            if wants("pmux:activity/read") {
                for (scope, label, what) in std::iter::once((
                    "activity.read:local".to_string(),
                    "Read activity on this computer".to_string(),
                    "see which agent sessions exist and whether they need you",
                ))
                .chain(self.snapshot.iter().flat_map(|snapshot| {
                    snapshot.endpoints.iter().map(|endpoint| {
                        (
                            format!("activity.read:{}", endpoint.label),
                            format!("Read activity from {}", endpoint.label),
                            "see that computer's agent sessions",
                        )
                    })
                })) {
                    row(
                        label,
                        format!("{}{} · {what}", on(&scope), tag(&scope)),
                        Action::Grant(scope),
                    );
                }
            }
            if wants("pmux:activity/manage") {
                let scope = "activity.manage:local".to_string();
                row(
                    "Clean up stopped sessions on this computer".into(),
                    format!(
                        "{}{} · close sessions whose agent has exited; never one with a live process",
                        on(&scope),
                        tag(&scope)
                    ),
                    Action::Grant(scope),
                );
            }
            let installed = self.selected_installed();
            if installed.is_none() {
                row(
                    format!("Install version {}", draft.version),
                    if online {
                        "Package stays disabled"
                    } else {
                        "Requires a running daemon"
                    }
                    .into(),
                    if online && !busy {
                        Action::Install
                    } else {
                        Action::None
                    },
                );
            } else {
                row(
                    format!("Enable version {}", draft.version),
                    if !online {
                        "Requires a running daemon"
                    } else if manifest.is_some_and(|manifest| manifest.compatibility().is_err()) {
                        "Incompatible with this release"
                    } else {
                        "Apply this version and the permissions turned on above"
                    }
                    .into(),
                    if online
                        && !busy
                        && manifest.is_some_and(|manifest| manifest.compatibility().is_ok())
                    {
                        Action::Enable
                    } else {
                        Action::None
                    },
                );
            }
            row(
                "Disable plug-in".into(),
                "Stop its workers and revoke access".into(),
                if online && !busy {
                    Action::Disable
                } else {
                    Action::None
                },
            );
            row(
                "Activity collection".into(),
                "Controlled separately in Multiplexer settings".into(),
                Action::None,
            );
        } else {
            let waiting = self.snapshot.as_ref().map_or(Vec::new(), |snapshot| {
                native::updates_available(&snapshot.catalog, &snapshot.states)
            });
            if let Some(snapshot) = &self.snapshot {
                if !snapshot.restart.is_empty() {
                    row(
                        "Restart to finish plug-in updates".into(),
                        restart_text(&snapshot.restart),
                        Action::None,
                    );
                    row(
                        "Restart now".into(),
                        "Opens Update and restart; Enter lists sessions and saves them before restarting"
                            .into(),
                        Action::RestartNow,
                    );
                } else if let Some(reason) = &snapshot.restart_unknown {
                    row(
                        "Restart status unknown".into(),
                        format!("Could not check the installed release: {reason}"),
                        Action::None,
                    );
                }
            }
            row(
                "Check for plug-in updates".into(),
                "Refresh the official package catalog and list what is newer".into(),
                if online && !busy {
                    Action::Load(true)
                } else {
                    Action::None
                },
            );
            row(
                "Update and enable".into(),
                check_summary(&waiting),
                if online && !busy && !waiting.is_empty() {
                    Action::UpdateAll
                } else {
                    Action::None
                },
            );
            row(
                "Refresh installed plug-ins".into(),
                "Read the selected daemon and verified local packages".into(),
                Action::Load(false),
            );
            let ids = self
                .snapshot
                .iter()
                .flat_map(|snapshot| {
                    snapshot
                        .catalog
                        .iter()
                        .map(|entry| entry.id.clone())
                        .chain(snapshot.installed.iter().map(|entry| entry.id.clone()))
                        .chain(snapshot.states.iter().map(|entry| entry.id.clone()))
                })
                .collect::<BTreeSet<_>>();
            if ids.is_empty() {
                row(
                    if busy {
                        "Loading plug-ins"
                    } else if self.snapshot.is_none() {
                        "Plug-ins have not loaded"
                    } else {
                        "No plug-ins available"
                    }
                    .into(),
                    "Use Refresh to read verified package metadata".into(),
                    Action::None,
                );
            }
            for id in ids {
                let state = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.states.iter().find(|state| state.id == id));
                let update = self.update_for(&id);
                let detail = format!(
                    "{} · {}",
                    installed_text(state),
                    status_text(state, update.as_deref())
                );
                row(id.clone(), detail, Action::Open(id));
            }
        }
        row(
            "Daemon target".into(),
            self.socket
                .as_ref()
                .map(|socket| socket.display().to_string())
                .unwrap_or_else(|| "Unavailable".into()),
            Action::None,
        );
        rows
    }

    pub fn rows(&self) -> Vec<PaletteRow> {
        self.items()
            .into_iter()
            .map(|row| PaletteRow::plain(row.label, row.detail, String::new()))
            .collect()
    }

    fn selected_installed(&self) -> Option<Installed> {
        let draft = self.draft.as_ref()?;
        self.snapshot
            .as_ref()?
            .installed
            .iter()
            .find(|package| package.id == draft.id && package.version == draft.version)
            .cloned()
    }

    /// Return true when navigation should move focus to the first row.
    pub fn activate(&mut self, index: usize) -> bool {
        let Some(action) = self.items().get(index).map(|row| row.action.clone()) else {
            return false;
        };
        if matches!(action, Action::Back) {
            self.draft = None;
            return true;
        }
        if matches!(action, Action::RestartNow) {
            self.restart_requested = true;
            return true;
        }
        if self.pending.is_some() {
            return false;
        }
        match action {
            Action::Open(id) => {
                let Some(snapshot) = &self.snapshot else {
                    return false;
                };
                let current = snapshot.states.iter().find(|state| state.id == id);
                let version = current
                    .and_then(|state| state.selected.as_ref())
                    .map(|package| package.version.clone())
                    .or_else(|| {
                        snapshot
                            .installed
                            .iter()
                            .find(|package| package.id == id)
                            .map(|package| package.version.clone())
                    })
                    .or_else(|| {
                        snapshot
                            .catalog
                            .iter()
                            .find(|package| package.id == id)
                            .map(|package| package.version.clone())
                    });
                let Some(version) = version else {
                    return false;
                };
                let grants: BTreeSet<String> = current
                    .filter(|state| state.enabled)
                    .map(|state| state.grants.iter().cloned().collect())
                    .unwrap_or_default();
                self.draft = Some(Draft {
                    id,
                    version,
                    seeded: !grants.is_empty(),
                    grants,
                });
                self.start(None, false);
                return true;
            }
            Action::Version(version) => {
                if let Some(draft) = &mut self.draft {
                    if draft.version != version {
                        draft.version = version;
                        draft.grants.clear();
                        draft.seeded = false;
                        self.start(None, false);
                    }
                }
            }
            Action::Grant(scope) => {
                if let Some(draft) = &mut self.draft {
                    if !draft.grants.remove(&scope) {
                        draft.grants.insert(scope);
                    }
                }
            }
            Action::Load(refresh) => self.start(None, refresh),
            Action::Update { enable_off } => {
                if let Some(id) = self.draft.as_ref().map(|draft| draft.id.clone()) {
                    self.start_update(Some(id), enable_off);
                }
            }
            Action::UpdateAll => self.start_update(None, false),
            Action::Install | Action::Enable | Action::Disable => {
                let Some(draft) = &self.draft else {
                    return false;
                };
                let action = match action {
                    Action::Install => AdminAction::Install {
                        id: draft.id.clone(),
                        version: Some(draft.version.clone()),
                    },
                    Action::Enable => {
                        let Some(package) = self.selected_installed() else {
                            return false;
                        };
                        AdminAction::Enable {
                            package,
                            grants: draft.grants.iter().cloned().collect(),
                        }
                    }
                    _ => AdminAction::Disable {
                        id: draft.id.clone(),
                    },
                };
                match native::admin_request(action) {
                    Ok(request) => self.start(Some(Job::Admin(request)), false),
                    Err(error) => self.notice = error.to_string(),
                }
            }
            Action::None | Action::Back | Action::RestartNow => {}
        }
        false
    }

    fn start(&mut self, operation: Option<Job>, refresh: bool) {
        if self.pending.is_some() {
            return;
        }
        let Some(socket) = self.socket.clone() else {
            self.notice = "No daemon target is available".into();
            return;
        };
        let target = self
            .draft
            .as_ref()
            .map(|draft| (draft.id.clone(), draft.version.clone()));
        let stopped = Arc::new(AtomicBool::new(false));
        let closer = Arc::new(Mutex::new(None));
        let cancel = stopped.clone();
        let close = closer.clone();
        let wake = self.wake.clone();
        let (send, receive) = mpsc::channel();
        self.pending = Some(Pending {
            receive,
            stopped,
            closer,
        });
        self.notice = "Loading plug-ins…".into();
        std::thread::spawn(move || {
            let result = load(socket, operation, refresh, target, &cancel, &close)
                .map_err(|error| format!("{error:#}"));
            let _ = send.send(result);
            wake();
        });
    }

    pub fn poll(&mut self) -> bool {
        let result = self
            .pending
            .as_ref()
            .and_then(|pending| match pending.receive.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                    "Plug-in request stopped; refresh to check its outcome".into(),
                )),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        let Some(result) = result else {
            return false;
        };
        self.pending = None;
        match result {
            Ok(mut snapshot) => {
                if let Some(manifest) = snapshot.inspected.take() {
                    if let Some(draft) = self.draft.as_mut().filter(|draft| {
                        !draft.seeded
                            && draft.id == manifest.id
                            && draft.version == manifest.version
                    }) {
                        // Nothing granted yet: start from what the manifest requires.
                        draft.seeded = true;
                        draft.grants.extend(manifest.required_grants());
                    }
                    self.manifests
                        .insert((manifest.id.clone(), manifest.version.clone()), manifest);
                }
                self.notice = if let Some(summary) = snapshot.summary.take() {
                    summary
                } else if snapshot.online {
                    "Installation and activity permissions are separate".into()
                } else {
                    "Daemon unavailable; verified saved metadata only".into()
                };
                self.snapshot = Some(snapshot);
                if let Some(command) = self.queued.take() {
                    self.run(command);
                }
            }
            Err(error) => self.notice = format!("{error}. Refresh to check the saved state."),
        }
        true
    }
}

fn load(
    socket: PathBuf,
    operation: Option<Job>,
    refresh: bool,
    target: Option<(String, String)>,
    stopped: &AtomicBool,
    closer: &Mutex<Option<ConnectionCloser>>,
) -> Result<Snapshot> {
    let mut client = Client::connect(&socket).ok();
    if let Some(client) = &client {
        *closer.lock().unwrap() = Some(client.close_handle()?);
    }
    ensure!(!stopped.load(Ordering::Acquire), "Plug-in panel closed");
    let online = client.is_some();
    let mut summary = None;
    if let Some(job) = operation {
        ensure!(online, "Plug-in changes require a running daemon");
        ensure!(!stopped.load(Ordering::Acquire), "Plug-in panel closed");
        let client = client.as_mut().expect("online");
        match job {
            Job::Admin(operation) => {
                client.request(operation)?;
            }
            Job::Update { only, enable_off } => {
                let outcomes = native::update_plugins(client, only.as_deref(), enable_off)?;
                summary = Some(summarize(&outcomes));
            }
        }
    }
    let mut request = |operation| -> Result<Reply> {
        ensure!(!stopped.load(Ordering::Acquire), "Plug-in panel closed");
        if let Some(client) = &mut client {
            client.request(operation)
        } else {
            native::inspect_offline(&socket, operation)
        }
    };
    let Reply::List { installed, states } = request(Request::List)? else {
        bail!("Unexpected installed plug-in reply");
    };
    let Reply::Catalog { packages: catalog } = request(Request::Discover { refresh })? else {
        bail!("Unexpected plug-in catalog reply");
    };
    let Reply::Endpoints { endpoints } = request(Request::Endpoints)? else {
        bail!("Unexpected endpoint reply");
    };
    // An older daemon does not know this request; the page still loads.
    let (restart, restart_unknown) = match request(Request::RestartPending) {
        Ok(Reply::RestartPending { plugins, unchecked }) => (plugins, unchecked),
        Ok(_) => (Vec::new(), Some("unexpected daemon reply".into())),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let inspected = if let Some((id, version)) = target.filter(|(id, version)| {
        installed
            .iter()
            .any(|package| &package.id == id && &package.version == version)
    }) {
        let Reply::Inspection { manifest, .. } = request(Request::Inspect {
            id,
            version: Some(version),
        })?
        else {
            bail!("Unexpected manifest reply");
        };
        Some(manifest.context("Installed plug-in has no manifest")?)
    } else {
        None
    };
    if summary.is_none() && refresh && online {
        summary = Some(check_summary(&native::updates_available(&catalog, &states)));
    }
    Ok(Snapshot {
        online,
        catalog,
        installed,
        states,
        endpoints,
        inspected,
        summary,
        restart,
        restart_unknown,
    })
}

/// The plug-ins a restart changes, in words: "org.x 1.0.0 → 1.1.0 · org.y (new) 2.0.0".
fn restart_text(pending: &[PendingPlugin]) -> String {
    pending
        .iter()
        .map(|plugin| match &plugin.from {
            Some(from) => format!("{} {from} → {}", plugin.id, plugin.to),
            None => format!("{} (new) {}", plugin.id, plugin.to),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Page {
    /// The newest version offered for `id` when it is newer than the one in use.
    fn update_for(&self, id: &str) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        native::updates_available(&snapshot.catalog, &snapshot.states)
            .into_iter()
            .find(|(known, _)| known == id)
            .map(|(_, version)| version)
    }
}

fn installed_text(state: Option<&Status>) -> String {
    state.and_then(|state| state.selected.as_ref()).map_or_else(
        || "Not installed".to_owned(),
        |package| format!("{} installed", package.version),
    )
}

/// One plug-in's state in words, for the list.
fn status_text(state: Option<&Status>, update: Option<&str>) -> String {
    let Some(state) = state else {
        return "Available".into();
    };
    match state.observed {
        Observed::Failed => {
            return format!(
                "Failed: {}",
                state.failure.as_deref().unwrap_or("open it for details")
            )
        }
        Observed::Incompatible => return "Incompatible with this release".into(),
        Observed::DaemonUnavailable => return "Daemon unavailable; saved state shown".into(),
        _ => {}
    }
    match (state.enabled, update) {
        (true, Some(newest)) => format!("Update available: {newest}"),
        (true, None) => "Enabled".into(),
        (false, Some(newest)) => format!("Disabled · update available: {newest}"),
        (false, None) => "Disabled".into(),
    }
}

fn check_summary(waiting: &[(String, String)]) -> String {
    if waiting.is_empty() {
        return "Plug-ins are up to date".into();
    }
    format!(
        "{} update{} available: {}",
        waiting.len(),
        if waiting.len() == 1 { "" } else { "s" },
        waiting
            .iter()
            .map(|(id, version)| format!("{id} {version}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// What an update run did, one clause per plug-in that changed or failed.
fn summarize(outcomes: &[native::Outcome]) -> String {
    let clauses = outcomes
        .iter()
        .filter_map(|outcome| {
            let to = outcome.to.as_deref().unwrap_or("");
            Some(match &outcome.result {
                native::UpdateResult::Current => return None,
                native::UpdateResult::Updated => format!(
                    "{} {} to {to}",
                    outcome.id,
                    outcome.from.as_deref().unwrap_or("?")
                ),
                native::UpdateResult::InstalledStaysOff => {
                    format!("{} {to} installed, still off", outcome.id)
                }
                native::UpdateResult::InstalledAndEnabled => {
                    format!("{} {to} installed and enabled", outcome.id)
                }
                native::UpdateResult::Failed(reason) => {
                    format!("{} not updated: {reason}", outcome.id)
                }
            })
        })
        .collect::<Vec<_>>();
    if clauses.is_empty() {
        "Plug-ins are up to date".into()
    } else {
        clauses.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(enabled: bool, observed: Observed, failure: Option<&str>) -> Status {
        Status {
            id: "org.example.reader".into(),
            selected: Some(prismattyc_mux::plugins::Installed {
                id: "org.example.reader".into(),
                version: "1.2.0".into(),
                digest: "0".repeat(64),
            }),
            enabled,
            grants: Vec::new(),
            observed,
            failure: failure.map(str::to_owned),
        }
    }

    #[test]
    fn a_list_row_says_what_is_installed_and_what_state_it_is_in() {
        let on = state(true, Observed::Running, None);
        assert_eq!(installed_text(Some(&on)), "1.2.0 installed");
        assert_eq!(installed_text(None), "Not installed");
        assert_eq!(status_text(Some(&on), None), "Enabled");
        assert_eq!(
            status_text(Some(&on), Some("1.3.0")),
            "Update available: 1.3.0"
        );
        let off = state(false, Observed::Stopped, None);
        assert_eq!(status_text(Some(&off), None), "Disabled");
        assert_eq!(
            status_text(Some(&off), Some("1.3.0")),
            "Disabled · update available: 1.3.0"
        );
        assert_eq!(status_text(None, None), "Available");
    }

    #[test]
    fn a_failed_or_incompatible_plugin_is_named_before_its_update() {
        let failed = state(true, Observed::Failed, Some("worker exited"));
        assert_eq!(
            status_text(Some(&failed), Some("1.3.0")),
            "Failed: worker exited"
        );
        let incompatible = state(true, Observed::Incompatible, None);
        assert_eq!(
            status_text(Some(&incompatible), None),
            "Incompatible with this release"
        );
    }

    #[test]
    fn the_check_notice_names_each_plugin_with_a_newer_version() {
        assert_eq!(check_summary(&[]), "Plug-ins are up to date");
        assert_eq!(
            check_summary(&[("a".into(), "1.1.0".into())]),
            "1 update available: a 1.1.0"
        );
        assert_eq!(
            check_summary(&[("a".into(), "1.1.0".into()), ("b".into(), "2.0.0".into())]),
            "2 updates available: a 1.1.0, b 2.0.0"
        );
    }

    #[test]
    fn an_update_run_names_what_changed_and_what_failed() {
        let outcome = |id: &str, from: &str, to: Option<&str>, result| native::Outcome {
            id: id.into(),
            from: Some(from.into()),
            to: to.map(str::to_owned),
            result,
        };
        assert_eq!(
            summarize(&[outcome("a", "1.0.0", None, native::UpdateResult::Current)]),
            "Plug-ins are up to date"
        );
        assert_eq!(
            summarize(&[
                outcome("a", "1.0.0", Some("1.1.0"), native::UpdateResult::Updated),
                outcome("b", "2.0.0", Some("2.1.0"), native::UpdateResult::InstalledStaysOff),
                outcome(
                    "c",
                    "3.0.0",
                    Some("3.1.0"),
                    native::UpdateResult::Failed("does not fit this release".into())
                ),
                outcome("d", "4.0.0", None, native::UpdateResult::Current),
            ]),
            "a 1.0.0 to 1.1.0; b 2.1.0 installed, still off; c not updated: does not fit this release"
        );
    }

    fn loaded(restart: Vec<PendingPlugin>, restart_unknown: Option<&str>) -> Page {
        let mut page = Page::new();
        page.snapshot = Some(Snapshot {
            online: true,
            restart,
            restart_unknown: restart_unknown.map(str::to_owned),
            ..Snapshot::default()
        });
        page
    }

    fn labels(page: &Page) -> Vec<String> {
        page.items().into_iter().map(|row| row.label).collect()
    }

    #[test]
    fn a_pending_restart_lists_each_plugin_and_offers_restart_now() {
        let mut page = loaded(
            vec![
                PendingPlugin {
                    id: "org.moonbase.activity-monitor".into(),
                    from: Some("1.0.0".into()),
                    to: "1.1.0".into(),
                },
                PendingPlugin {
                    id: "org.moonbase.activity-connector".into(),
                    from: None,
                    to: "2.0.0".into(),
                },
            ],
            None,
        );
        let items = page.items();
        let notice = items
            .iter()
            .position(|row| row.label == "Restart to finish plug-in updates")
            .expect("restart notice row");
        assert_eq!(
            items[notice].detail,
            "org.moonbase.activity-monitor 1.0.0 → 1.1.0 · org.moonbase.activity-connector (new) 2.0.0"
        );
        assert_eq!(items[notice + 1].label, "Restart now");
        assert!(!page.take_restart_request());
        // Restart now asks the host to open Update and restart, once.
        assert!(page.activate(notice + 1));
        assert!(page.take_restart_request());
        assert!(!page.take_restart_request());
        // The notice row itself does nothing.
        assert!(!page.activate(notice));
        assert!(!page.take_restart_request());
    }

    #[test]
    fn nothing_pending_shows_no_restart_rows_and_an_unreadable_release_says_why() {
        let quiet = labels(&loaded(Vec::new(), None));
        assert!(!quiet.iter().any(|label| label.starts_with("Restart")));
        let unknown = loaded(
            Vec::new(),
            Some("the installed daemon reported an unreadable version"),
        );
        let row = unknown
            .items()
            .into_iter()
            .find(|row| row.label == "Restart status unknown")
            .expect("unknown row");
        assert_eq!(
            row.detail,
            "Could not check the installed release: the installed daemon reported an unreadable version"
        );
        assert!(!labels(&unknown).contains(&"Restart now".to_owned()));
    }
}
