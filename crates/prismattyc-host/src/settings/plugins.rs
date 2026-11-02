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
    Manifest, Observed, Reply, Request, Status,
};

use crate::palette::PaletteRow;

fn capability(name: &str) -> &str {
    match name {
        "pmux:activity/read" => "Activity metadata",
        "pmux:view/frame" => "Terminal views",
        "pmux:tool/call" => "Commands and MCP tools",
        _ => "Unsupported capability",
    }
}

#[derive(Default)]
struct Snapshot {
    online: bool,
    catalog: Vec<CatalogEntry>,
    installed: Vec<Installed>,
    states: Vec<Status>,
    endpoints: Vec<Endpoint>,
    inspected: Option<Manifest>,
}

struct Draft {
    id: String,
    version: String,
    grants: BTreeSet<String>,
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
        }
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
                            "Choose this version".into()
                        },
                        Action::Version(version),
                    );
                }
            }
            let manifest = self
                .manifests
                .get(&(draft.id.clone(), draft.version.clone()));
            row(
                "Capabilities".into(),
                manifest
                    .map(|manifest| {
                        manifest
                            .requires
                            .keys()
                            .map(|name| capability(name).to_string())
                            .chain(
                                manifest
                                    .optional
                                    .keys()
                                    .map(|name| format!("Optional {}", capability(name))),
                            )
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| "Install this version to review its capabilities".into()),
                Action::None,
            );
            if manifest.is_some_and(|manifest| {
                manifest.requires.contains_key("pmux:activity/read")
                    || manifest.optional.contains_key("pmux:activity/read")
            }) {
                let labels = std::iter::once("local".to_string()).chain(
                    self.snapshot.iter().flat_map(|snapshot| {
                        snapshot
                            .endpoints
                            .iter()
                            .map(|endpoint| endpoint.label.clone())
                    }),
                );
                for label in labels {
                    let scope = format!("activity.read:{label}");
                    row(
                        format!("Approve activity from {label}"),
                        if draft.grants.contains(&scope) {
                            "Selected for the next Enable".into()
                        } else {
                            "Not selected".into()
                        },
                        Action::Grant(scope),
                    );
                }
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
                        "Apply this version and the checked activity permissions"
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
            row(
                "Refresh installed plug-ins".into(),
                "Read the selected daemon and verified local packages".into(),
                Action::Load(false),
            );
            row(
                "Check for available versions".into(),
                "Refresh the official package catalog".into(),
                if online {
                    Action::Load(true)
                } else {
                    Action::None
                },
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
                let detail = state
                    .map(|state| {
                        format!(
                            "{}{}",
                            if state.enabled { "Enabled" } else { "Disabled" },
                            state
                                .selected
                                .as_ref()
                                .map(|package| format!(" · {}", package.version))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "Available; disabled".into());
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
                let grants = current
                    .filter(|state| state.enabled)
                    .map(|state| state.grants.iter().cloned().collect())
                    .unwrap_or_default();
                self.draft = Some(Draft {
                    id,
                    version,
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
                    Ok(request) => self.start(Some(request), false),
                    Err(error) => self.notice = error.to_string(),
                }
            }
            Action::None | Action::Back => {}
        }
        false
    }

    fn start(&mut self, operation: Option<Request>, refresh: bool) {
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
                    self.manifests
                        .insert((manifest.id.clone(), manifest.version.clone()), manifest);
                }
                self.notice = if snapshot.online {
                    "Installation and activity permissions are separate".into()
                } else {
                    "Daemon unavailable; verified saved metadata only".into()
                };
                self.snapshot = Some(snapshot);
            }
            Err(error) => self.notice = format!("{error}. Refresh to check the saved state."),
        }
        true
    }
}

fn load(
    socket: PathBuf,
    operation: Option<Request>,
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
    let mut request = |operation| -> Result<Reply> {
        ensure!(!stopped.load(Ordering::Acquire), "Plug-in panel closed");
        if let Some(client) = &mut client {
            client.request(operation)
        } else {
            native::inspect_offline(&socket, operation)
        }
    };
    if let Some(operation) = operation {
        ensure!(online, "Plug-in changes require a running daemon");
        request(operation)?;
    }
    let Reply::List { installed, states } = request(Request::List)? else {
        bail!("Unexpected installed plug-in reply");
    };
    let Reply::Catalog { packages: catalog } = request(Request::Discover { refresh })? else {
        bail!("Unexpected plug-in catalog reply");
    };
    let Reply::Endpoints { endpoints } = request(Request::Endpoints)? else {
        bail!("Unexpected endpoint reply");
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
    Ok(Snapshot {
        online,
        catalog,
        installed,
        states,
        endpoints,
        inspected,
    })
}
