// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use pmux_plugin_api::host::pmux::{activity::read, plugin::lifecycle, tool::call, view::frame};
use pmux_plugin_api::{validate_viewport, ValidatedFrame, API_VERSION, CONTRACTS, MAX_FRAME_BYTES};
use pmux_plugin_runtime::ActivityProvider;
use prismattyc_core::activity::{ActivityCursor, ActivityPage};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use super::catalog::Catalog;
use super::endpoint::Endpoint;
use super::package::{
    digest, nonce, private_directory, read_private, valid_digest, Package, Store,
};
use super::protocol::{AdminAction, Observed, Reply, Request, Status, ToolExport};
use super::remote::Remote;
use super::worker::{ActiveWorker, Cancellation, PendingWorker};
use super::{Contribution, Installed, Manifest};
use crate::ControlPlane;

const MAX_WORKERS: usize = 16;

/// Constructed without touching disk. Disabled-by-default daemons need no new
/// storage or executable until the first explicit plug-in request.
pub(crate) struct Service {
    socket: PathBuf,
    plane: Weak<Mutex<ControlPlane>>,
    manager: OnceLock<Arc<Manager>>,
    initializing: Mutex<()>,
    stopped: AtomicBool,
}

impl Service {
    pub(crate) fn new(socket: PathBuf, plane: &Arc<Mutex<ControlPlane>>) -> Self {
        Self {
            socket,
            plane: Arc::downgrade(plane),
            manager: OnceLock::new(),
            initializing: Mutex::new(()),
            stopped: AtomicBool::new(false),
        }
    }

    pub(crate) fn handle(&self, connection: &str, request: Request) -> Result<Reply> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "plug-in service is stopping"
        );
        if self.manager.get().is_none() {
            let _initializing = self.initializing.lock().unwrap();
            if self.manager.get().is_none() {
                let manager = Arc::new(Manager::new(&self.socket, self.plane.clone())?);
                let _ = self.manager.set(manager);
            }
        }
        if self.stopped.load(Ordering::Acquire) {
            self.stop();
            bail!("plug-in service is stopping");
        }
        self.manager.get().unwrap().handle(connection, request)
    }

    pub(crate) fn close(&self, connection: &str) {
        if let Some(manager) = self.manager.get() {
            manager.close_connection(connection);
        }
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(manager) = self.manager.get() {
            manager.stopped.store(true, Ordering::Release);
            let workers = std::mem::take(
                &mut manager
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .workers,
            );
            for worker in workers.into_values() {
                worker.cancel.cancel();
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Desired {
    Disabled {
        selected: Option<Installed>,
    },
    Enabled {
        selected: Installed,
        grants: BTreeSet<String>,
    },
}

impl Desired {
    fn selected(&self) -> Option<&Installed> {
        match self {
            Self::Disabled { selected } => selected.as_ref(),
            Self::Enabled { selected, .. } => Some(selected),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completed {
    id: String,
    action: String,
    reply: Reply,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u16,
    selections: BTreeMap<String, Desired>,
    completed: VecDeque<Completed>,
    #[serde(default)]
    endpoints: BTreeMap<String, Endpoint>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: 1,
            selections: BTreeMap::new(),
            completed: VecDeque::new(),
            endpoints: BTreeMap::new(),
        }
    }
}

struct Slot {
    generation: String,
    tool: Option<String>,
    attempts: VecDeque<Instant>,
    failure: Option<String>,
    package: Option<Arc<Package>>,
    incompatible: bool,
}

impl Slot {
    fn new() -> Result<Self> {
        Ok(Self {
            generation: nonce()?,
            tool: None,
            attempts: VecDeque::new(),
            failure: None,
            package: None,
            incompatible: false,
        })
    }
}

enum Phase {
    Starting,
    Running(Arc<ActiveWorker>),
}

struct Worker {
    id: String,
    plugin: String,
    generation: String,
    cancel: Cancellation,
    phase: Phase,
    endpoints: BTreeMap<u64, (usize, String, Access)>,
    rate_window: Instant,
    reads: u16,
    last_reads: BTreeMap<(String, u64), Instant>,
}

#[derive(Clone)]
enum Access {
    Local,
    Remote {
        endpoint: Endpoint,
        reader: Arc<Remote>,
    },
}

#[derive(Clone)]
struct View {
    connection: String,
    worker: String,
    reference: lifecycle::ViewReference,
}

struct Export {
    plugin: String,
    generation: String,
    contribution: String,
}

struct State {
    journal: Journal,
    slots: BTreeMap<String, Slot>,
    workers: BTreeMap<String, Worker>,
    views: BTreeMap<String, View>,
    exports: BTreeMap<(String, String), Export>,
}

struct Manager {
    _writer: std::fs::File,
    executable: PathBuf,
    journal: PathBuf,
    store: Store,
    catalog: Mutex<Arc<Catalog>>,
    administration: Mutex<()>,
    state: Mutex<State>,
    plane: Weak<Mutex<ControlPlane>>,
    stopped: AtomicBool,
    remotes: Mutex<BTreeMap<String, Weak<Remote>>>,
    active_remotes: Arc<AtomicUsize>,
}

fn load_journal(path: &Path) -> Result<Journal> {
    let journal: Journal = if path.try_exists()? {
        serde_json::from_slice(&read_private(&path, 1024 * 1024)?)?
    } else {
        Journal::default()
    };
    ensure!(
        journal.version == 1 && journal.selections.len() <= 256 && journal.completed.len() <= 128,
        "unsupported or oversized plug-in journal"
    );
    ensure!(
        journal.endpoints.len() <= 31,
        "too many configured activity endpoints"
    );
    for (label, endpoint) in &journal.endpoints {
        endpoint.validate()?;
        ensure!(
            label == &endpoint.label,
            "endpoint label does not match its key"
        );
    }
    for (id, desired) in &journal.selections {
        super::manifest::validate_plugin_id(id)?;
        if let Some(selected) = desired.selected() {
            ensure!(
                selected.id == *id && valid_digest(&selected.digest),
                "invalid saved plug-in selection"
            );
            Version::parse(&selected.version)?;
        }
        if let Desired::Enabled { grants, .. } = desired {
            validate_grants(grants, &journal.endpoints)?;
        }
    }
    Ok(journal)
}

impl Manager {
    fn new(socket: &Path, plane: Weak<Mutex<ControlPlane>>) -> Result<Self> {
        let path = socket.with_extension("plugins.json");
        private_directory(path.parent().context("plug-in journal has no parent")?)?;
        let writer = crate::private_fs::open(
            &socket.with_extension("plugins.lock"),
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true),
        )?;
        crate::platform::try_lock_exclusive(&writer)
            .context("another manager owns this plug-in journal")?;
        let journal = load_journal(&path)?;
        let slots = journal
            .selections
            .keys()
            .map(|id| Ok((id.clone(), Slot::new()?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let store = Store::new(Store::default_root()?)?;
        let catalog = Catalog::bundled()?;
        store.check_catalog(&catalog.list())?;
        Ok(Self {
            _writer: writer,
            executable: std::env::current_exe()?.canonicalize()?,
            journal: path,
            store,
            catalog: Mutex::new(Arc::new(catalog)),
            administration: Mutex::new(()),
            state: Mutex::new(State {
                journal,
                slots,
                workers: BTreeMap::new(),
                views: BTreeMap::new(),
                exports: BTreeMap::new(),
            }),
            plane,
            stopped: AtomicBool::new(false),
            remotes: Mutex::new(BTreeMap::new()),
            active_remotes: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn handle(self: &Arc<Self>, connection: &str, request: Request) -> Result<Reply> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "plug-in manager is stopping"
        );
        match request {
            Request::Endpoints => Ok(Reply::Endpoints {
                endpoints: self
                    .state
                    .lock()
                    .unwrap()
                    .journal
                    .endpoints
                    .values()
                    .cloned()
                    .collect(),
            }),
            Request::Discover { refresh } => {
                if refresh {
                    let catalog = Arc::new(Catalog::refreshed()?);
                    self.store.check_catalog(&catalog.list())?;
                    *self.catalog.lock().unwrap() = catalog;
                }
                Ok(Reply::Catalog {
                    packages: self.catalog.lock().unwrap().list(),
                })
            }
            Request::List => {
                let installed = self.store.list()?;
                let state = self.state.lock().unwrap();
                Ok(Reply::List {
                    installed,
                    states: state
                        .journal
                        .selections
                        .keys()
                        .map(|id| status(&state, id))
                        .collect(),
                })
            }
            Request::Inspect { id, version } => {
                let current = status(&self.state.lock().unwrap(), &id);
                inspection(&self.store, current, version.as_deref())
            }
            Request::Admin { retry_id, action } => self.admin(retry_id, action),
            Request::Invoke {
                id,
                contribution,
                arguments,
                min_interval_ms,
            } => {
                let worker = self.tool(&id, &contribution)?;
                self.invoke(
                    connection,
                    min_interval_ms,
                    &worker,
                    lifecycle::Message::Tool(call::Request {
                        id: number()?,
                        contribution,
                        arguments,
                    }),
                )
            }
            Request::OpenView {
                id,
                contribution,
                viewport,
                preferences,
                endpoints,
            } => self.open_view(
                connection,
                &id,
                contribution,
                viewport,
                preferences,
                endpoints,
            ),
            Request::ViewEvent { handle, event } => self.view_event(connection, &handle, event),
            Request::CloseView { handle } => {
                self.close_view(connection, &handle)?;
                Ok(Reply::Idle)
            }
            Request::Exports { allowlist } => self.exports(connection, allowlist),
            Request::CallExport {
                handle,
                arguments,
                min_interval_ms,
            } => {
                let (plugin, contribution) = {
                    let state = self.state.lock().unwrap();
                    let export = state
                        .exports
                        .get(&(connection.into(), handle))
                        .context("unknown tool handle for this connection")?;
                    ensure!(
                        state
                            .slots
                            .get(&export.plugin)
                            .is_some_and(|slot| slot.generation == export.generation),
                        "tool handle was revoked"
                    );
                    (export.plugin.clone(), export.contribution.clone())
                };
                let worker = self.tool(&plugin, &contribution)?;
                self.invoke(
                    connection,
                    min_interval_ms,
                    &worker,
                    lifecycle::Message::Tool(call::Request {
                        id: number()?,
                        contribution,
                        arguments,
                    }),
                )
            }
        }
    }

    fn admin(&self, retry_id: String, action: AdminAction) -> Result<Reply> {
        ensure!(
            retry_id.len() == 32 && retry_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid administrative retry ID"
        );
        let _administration = self
            .administration
            .try_lock()
            .map_err(|_| anyhow::anyhow!("another plug-in change is in progress; retry"))?;
        let action_digest = digest(&serde_json::to_vec(&action)?);
        if let Some(completed) = self
            .state
            .lock()
            .unwrap()
            .journal
            .completed
            .iter()
            .find(|completed| completed.id == retry_id)
        {
            ensure!(
                completed.action == action_digest,
                "administrative retry ID was reused for a different request"
            );
            return Ok(completed.reply.clone());
        }
        let mut desired = None;
        let mut endpoint_change = None;
        let mut imported = Vec::new();
        let reply = match action {
            AdminAction::ImportEndpoints { endpoints } => {
                let existing = self.state.lock().unwrap().journal.endpoints.clone();
                imported = super::endpoint_import::plan(&existing, endpoints)?;
                Reply::Endpoints {
                    endpoints: imported.clone(),
                }
            }
            AdminAction::AddEndpoint { label, source } => {
                let endpoint = Endpoint {
                    id: nonce()?,
                    label: label.clone(),
                    source,
                };
                endpoint.validate()?;
                let state = self.state.lock().unwrap();
                ensure!(!state.journal.endpoints.contains_key(&label), "endpoint label already exists; remove it and approve new grants before replacing its transport");
                ensure!(
                    state.journal.endpoints.len() < 31,
                    "too many configured endpoints"
                );
                endpoint_change = Some((label.clone(), Some(endpoint.clone())));
                Reply::EndpointChanged {
                    label,
                    endpoint: Some(endpoint),
                }
            }
            AdminAction::RemoveEndpoint { label } => {
                ensure!(
                    self.state
                        .lock()
                        .unwrap()
                        .journal
                        .endpoints
                        .contains_key(&label),
                    "unknown configured endpoint"
                );
                endpoint_change = Some((label.clone(), None));
                Reply::EndpointChanged {
                    label,
                    endpoint: None,
                }
            }
            AdminAction::Install { id, version } => {
                let catalog = self.catalog.lock().unwrap().clone();
                let package = self
                    .store
                    .install(&self.executable, catalog.get(&id, version.as_deref())?)?;
                Reply::Installed { package }
            }
            AdminAction::Enable { package, grants } => {
                let grants = grants.into_iter().collect::<BTreeSet<_>>();
                validate_grants(&grants, &self.state.lock().unwrap().journal.endpoints)?;
                let next = Desired::Enabled {
                    selected: package.clone(),
                    grants: grants.clone(),
                };
                let unchanged = self
                    .state
                    .lock()
                    .unwrap()
                    .journal
                    .selections
                    .get(&package.id)
                    == Some(&next);
                if !unchanged {
                    let loaded = self.store.load(&package)?;
                    loaded.manifest.compatibility()?;
                    let role = readiness_role(&loaded.manifest)?;
                    let context = context(&loaded.manifest, &grants, role)?;
                    // Candidate readiness has no data authority. The old selection
                    // remains current until the one durable journal transition.
                    let pending = PendingWorker::spawn(&self.executable)?;
                    let _prepared = pending.prepare(
                        loaded.component,
                        context,
                        loaded.assets,
                        loaded.schemas,
                    )?;
                }
                desired = Some((package.id.clone(), next));
                Reply::State {
                    state: Status {
                        id: package.id.clone(),
                        selected: Some(package),
                        enabled: true,
                        grants: grants.into_iter().collect(),
                        observed: Observed::Stopped,
                        failure: None,
                    },
                }
            }
            AdminAction::Disable { id } => {
                super::manifest::validate_plugin_id(&id)?;
                let selected = self
                    .state
                    .lock()
                    .unwrap()
                    .journal
                    .selections
                    .get(&id)
                    .and_then(Desired::selected)
                    .cloned();
                desired = Some((
                    id.clone(),
                    Desired::Disabled {
                        selected: selected.clone(),
                    },
                ));
                Reply::State {
                    state: Status {
                        id,
                        selected,
                        enabled: false,
                        grants: Vec::new(),
                        observed: Observed::Stopped,
                        failure: None,
                    },
                }
            }
        };
        let cancellations = {
            let mut state = self.state.lock().unwrap();
            ensure!(
                !self.stopped.load(Ordering::Acquire),
                "plug-in manager is stopping"
            );
            let mut journal = state.journal.clone();
            let changed = desired
                .as_ref()
                .is_some_and(|(id, desired)| journal.selections.get(id) != Some(desired));
            let new_slot = changed.then(Slot::new).transpose()?;
            let mut endpoint_slots = BTreeMap::new();
            for endpoint in &imported {
                journal
                    .endpoints
                    .insert(endpoint.label.clone(), endpoint.clone());
            }
            if let Some((label, endpoint)) = &endpoint_change {
                if let Some(endpoint) = endpoint {
                    journal.endpoints.insert(label.clone(), endpoint.clone());
                } else {
                    journal.endpoints.remove(label);
                    let scope = format!("activity.read:{label}");
                    for (id, selection) in &mut journal.selections {
                        if let Desired::Enabled { grants, .. } = selection {
                            if grants.remove(&scope) {
                                endpoint_slots.insert(id.clone(), Slot::new()?);
                            }
                        }
                    }
                }
            }
            if let Some((id, desired)) = &desired {
                journal.selections.insert(id.clone(), desired.clone());
            }
            ensure!(
                journal.selections.len() <= 256,
                "too many saved plug-in selections"
            );
            journal.completed.push_back(Completed {
                id: retry_id,
                action: action_digest,
                reply: reply.clone(),
            });
            while journal.completed.len() > 128 {
                journal.completed.pop_front();
            }
            let bytes = loop {
                let bytes = serde_json::to_string(&journal)?;
                if bytes.len() <= 1024 * 1024 {
                    break bytes;
                }
                ensure!(
                    journal.completed.len() > 1,
                    "plug-in journal exceeds its byte budget"
                );
                journal.completed.pop_front();
            };
            private_directory(
                self.journal
                    .parent()
                    .context("plug-in journal has no parent")?,
            )?;
            crate::config::write_file_atomic(&self.journal, &bytes)?;
            state.journal = journal;
            let mut cancellations = Vec::new();
            for (id, slot) in endpoint_slots {
                state.slots.insert(id.clone(), slot);
                cancellations.extend(revoke(&mut state, &id));
            }
            if changed {
                let (id, _) = desired.as_ref().unwrap();
                state.slots.insert(id.clone(), new_slot.unwrap());
                cancellations.extend(revoke(&mut state, id));
            }
            cancellations
        };
        for cancel in cancellations {
            cancel.cancel();
        }
        Ok(reply)
    }
}

fn validate_grants(
    grants: &BTreeSet<String>,
    endpoints: &BTreeMap<String, Endpoint>,
) -> Result<()> {
    ensure!(
        grants.iter().all(|grant| grant == "activity.read:local"
            || grant
                .strip_prefix("activity.read:")
                .is_some_and(|label| endpoints.contains_key(label))),
        "unknown or unconfigured plug-in permission scope"
    );
    Ok(())
}

fn inspection(store: &Store, current: Status, version: Option<&str>) -> Result<Reply> {
    super::manifest::validate_plugin_id(&current.id)?;
    let selected = if let Some(version) = version {
        Version::parse(version)?;
        Some(
            store
                .list()?
                .into_iter()
                .find(|package| package.id == current.id && package.version == version)
                .context("requested version is not installed")?,
        )
    } else {
        current.selected.clone()
    };
    let manifest = selected
        .as_ref()
        .map(|selected| store.load(selected).map(|package| package.manifest))
        .transpose()?;
    Ok(Reply::Inspection {
        state: current,
        manifest,
    })
}

/// Verified read-only metadata. This path cannot acquire the journal writer,
/// create storage, fetch packages, change permissions or start a worker.
pub fn inspect_offline(socket: &Path, request: Request) -> Result<Reply> {
    ensure!(
        matches!(
            request,
            Request::Discover { refresh: false }
                | Request::List
                | Request::Inspect { .. }
                | Request::Endpoints
        ),
        "plug-in mutations and refresh require a running daemon"
    );
    if matches!(request, Request::Discover { .. }) {
        let catalog = Catalog::bundled()?;
        Store::new(Store::default_root()?)?.check_catalog(&catalog.list())?;
        return Ok(Reply::Catalog {
            packages: catalog.list(),
        });
    }
    let journal = load_journal(&socket.with_extension("plugins.json"))?;
    let state = State {
        journal,
        slots: BTreeMap::new(),
        workers: BTreeMap::new(),
        views: BTreeMap::new(),
        exports: BTreeMap::new(),
    };
    let store = Store::new(Store::default_root()?)?;
    let saved = |id: &str| -> Result<Status> {
        let mut current = status(&state, id);
        if current.enabled {
            current.observed = Observed::DaemonUnavailable;
        }
        if let Some(selected) = &current.selected {
            if store.load(selected)?.manifest.compatibility().is_err() {
                current.observed = Observed::Incompatible;
                current.failure = Some("selected package is incompatible with this core".into());
            }
        }
        Ok(current)
    };
    match request {
        Request::List => Ok(Reply::List {
            installed: store.list()?,
            states: state
                .journal
                .selections
                .keys()
                .map(|id| saved(id))
                .collect::<Result<_>>()?,
        }),
        Request::Inspect { id, version } => inspection(&store, saved(&id)?, version.as_deref()),
        Request::Endpoints => Ok(Reply::Endpoints {
            endpoints: state.journal.endpoints.into_values().collect(),
        }),
        _ => unreachable!("read-only request checked above"),
    }
}

fn status(state: &State, id: &str) -> Status {
    let desired = state.journal.selections.get(id);
    let slot = state.slots.get(id);
    let enabled = matches!(desired, Some(Desired::Enabled { .. }));
    let observed = if !enabled {
        Observed::Stopped
    } else if state
        .workers
        .values()
        .any(|worker| worker.plugin == id && matches!(worker.phase, Phase::Running(_)))
    {
        Observed::Running
    } else if state.workers.values().any(|worker| worker.plugin == id) {
        Observed::Starting
    } else if slot.is_some_and(|slot| slot.incompatible) {
        Observed::Incompatible
    } else if slot.is_some_and(|slot| slot.failure.is_some()) {
        Observed::Failed
    } else {
        Observed::Stopped
    };
    Status {
        id: id.into(),
        selected: desired.and_then(Desired::selected).cloned(),
        enabled,
        grants: match desired {
            Some(Desired::Enabled { grants, .. }) => grants.iter().cloned().collect(),
            _ => Vec::new(),
        },
        observed,
        failure: slot.and_then(|slot| slot.failure.clone()),
    }
}

fn revoke(state: &mut State, id: &str) -> Vec<Cancellation> {
    let worker_ids = state
        .workers
        .values()
        .filter(|worker| worker.plugin == id)
        .map(|worker| worker.id.clone())
        .collect::<BTreeSet<_>>();
    state
        .views
        .retain(|_, view| !worker_ids.contains(&view.worker));
    state.exports.retain(|_, export| export.plugin != id);
    worker_ids
        .into_iter()
        .filter_map(|id| state.workers.remove(&id).map(|worker| worker.cancel))
        .collect()
}

fn number() -> Result<u64> {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("cannot mint plug-in ID"))?;
    Ok(u64::from_le_bytes(bytes).max(1))
}

fn readiness_role(manifest: &Manifest) -> Result<lifecycle::Role> {
    if manifest
        .contributions
        .iter()
        .any(|contribution| matches!(contribution, Contribution::Command { .. }))
    {
        return Ok(lifecycle::Role::ToolService);
    }
    let id = manifest
        .contributions
        .first()
        .context("plug-in has no contributions")?
        .id();
    Ok(lifecycle::Role::View(lifecycle::ViewReference {
        preferences: frame::Preferences {
            snapshot: true,
            motion: false,
            ..Default::default()
        },
        id: number()?,
        contribution: id.into(),
        viewport: frame::Viewport {
            columns: 80,
            rows: 24,
            pixels: None,
        },
    }))
}

fn context(
    manifest: &Manifest,
    grants: &BTreeSet<String>,
    role: lifecycle::Role,
) -> Result<lifecycle::Context> {
    let mut capabilities = Vec::new();
    for (name, version) in CONTRACTS {
        let range = manifest
            .requires
            .get(*name)
            .or_else(|| manifest.optional.get(*name));
        if let Some(range) = range {
            let available = VersionReq::parse(range)?.matches(&Version::parse(version)?)
                && (*name != "pmux:activity/read"
                    || grants
                        .iter()
                        .any(|grant| grant.starts_with("activity.read:")));
            capabilities.push(lifecycle::Capability {
                name: (*name).into(),
                version: (*version).into(),
                available,
            });
        }
    }
    Ok(lifecycle::Context {
        api_version: API_VERSION.into(),
        capabilities,
        limits: lifecycle::Limits {
            memory_bytes: 64 * 1024 * 1024,
            result_bytes: 1024 * 1024,
            frame_bytes: MAX_FRAME_BYTES as u32,
        },
        role,
    })
}

struct Selection {
    installed: Installed,
    package: Arc<Package>,
    generation: String,
    grants: BTreeSet<String>,
}

impl Manager {
    fn selection(&self, id: &str) -> Result<Selection> {
        let (installed, grants, generation, cached) = {
            let state = self.state.lock().unwrap();
            let Some(Desired::Enabled { selected, grants }) = state.journal.selections.get(id)
            else {
                bail!("plug-in is disabled; install and explicitly enable it first");
            };
            let slot = state
                .slots
                .get(id)
                .context("plug-in has no runtime generation")?;
            (
                selected.clone(),
                grants.clone(),
                slot.generation.clone(),
                slot.package.clone(),
            )
        };
        let package = if let Some(package) = cached {
            package
        } else {
            let loaded = Arc::new(self.store.load(&installed)?);
            let mut state = self.state.lock().unwrap();
            let slot = state
                .slots
                .get_mut(id)
                .context("plug-in selection was revoked")?;
            ensure!(
                slot.generation == generation,
                "plug-in selection changed during package loading"
            );
            slot.package = Some(loaded.clone());
            loaded
        };
        if let Err(error) = package.manifest.compatibility() {
            let mut state = self.state.lock().unwrap();
            if let Some(slot) = state
                .slots
                .get_mut(id)
                .filter(|slot| slot.generation == generation)
            {
                slot.incompatible = true;
                slot.failure = Some("selected package is incompatible with this core".into());
            }
            return Err(error);
        }
        Ok(Selection {
            installed,
            package,
            generation,
            grants,
        })
    }

    fn tool(self: &Arc<Self>, id: &str, contribution: &str) -> Result<String> {
        let selection = self.selection(id)?;
        ensure!(
            selection.package.manifest.contributions.iter().any(
                |entry| matches!(entry, Contribution::Command { id, .. } if id == contribution)
            ),
            "unknown command contribution"
        );
        {
            let state = self.state.lock().unwrap();
            if let Some(worker) = state.slots.get(id).and_then(|slot| slot.tool.as_ref()) {
                let entry = state
                    .workers
                    .get(worker)
                    .context("tool worker is unavailable")?;
                ensure!(
                    matches!(entry.phase, Phase::Running(_)),
                    "tool worker is starting; retry"
                );
                return Ok(worker.clone());
            }
        }
        self.start(selection, lifecycle::Role::ToolService, None, None)
    }

    fn start(
        self: &Arc<Self>,
        selection: Selection,
        role: lifecycle::Role,
        view: Option<(String, String)>,
        requested: Option<Vec<super::ViewEndpoint>>,
    ) -> Result<String> {
        let id = selection.installed.id.clone();
        let identity = nonce()?;
        let prepared_context =
            context(&selection.package.manifest, &selection.grants, role.clone())?;
        let activity_available = prepared_context
            .capabilities
            .iter()
            .any(|capability| capability.name == "pmux:activity/read" && capability.available);
        let requested = requested.map(|endpoints| -> Result<BTreeMap<String, (usize, String)>> {
            ensure!(!endpoints.is_empty() && endpoints.len() <= 32, "views require 1 to 32 endpoints");
            let mut labels = BTreeSet::new();
            let mut sources = BTreeMap::new();
            for (index, endpoint) in endpoints.into_iter().enumerate() {
                super::endpoint::validate_label(&endpoint.label)?;
                ensure!(selection.grants.contains(&format!("activity.read:{}", endpoint.endpoint)), "endpoint permission is not approved; enable with --allow activity.read:LABEL first");
                ensure!(labels.insert(endpoint.label.clone()) && sources.insert(endpoint.endpoint, (index, endpoint.label)).is_none(), "view endpoint labels and sources must be unique");
            }
            Ok(sources)
        }).transpose()?;
        let display = |label: &str, index: usize| -> Option<(usize, String)> {
            requested.as_ref().map_or_else(
                || Some((index, label.into())),
                |labels| labels.get(label).cloned(),
            )
        };
        let definitions = {
            let state = self.state.lock().unwrap();
            state
                .journal
                .endpoints
                .values()
                .filter(|endpoint| {
                    activity_available
                        && selection
                            .grants
                            .contains(&format!("activity.read:{}", endpoint.label))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut endpoints = BTreeMap::new();
        if let Some((index, label)) = display("local", 0)
            .filter(|_| activity_available && selection.grants.contains("activity.read:local"))
        {
            endpoints.insert(number()?, (index, label, Access::Local));
        }
        for (index, endpoint) in definitions.into_iter().enumerate() {
            let Some((index, label)) = display(&endpoint.label, index + 1) else {
                continue;
            };
            let reader = {
                let mut remotes = self.remotes.lock().unwrap();
                remotes.retain(|_, remote| remote.strong_count() > 0);
                if let Some(reader) = remotes.get(&endpoint.id).and_then(Weak::upgrade) {
                    reader
                } else {
                    let reader = Remote::start(endpoint.clone(), self.active_remotes.clone())?;
                    remotes.insert(endpoint.id.clone(), Arc::downgrade(&reader));
                    reader
                }
            };
            let handle = loop {
                let handle = number()?;
                if !endpoints.contains_key(&handle) {
                    break handle;
                }
            };
            endpoints.insert(handle, (index, label, Access::Remote { endpoint, reader }));
        }
        let pending = {
            let mut state = self.state.lock().unwrap();
            ensure!(
                !self.stopped.load(Ordering::Acquire),
                "plug-in manager is stopping"
            );
            ensure!(
                state.workers.len() < MAX_WORKERS,
                "plug-in worker limit reached"
            );
            let slot = state
                .slots
                .get_mut(&id)
                .context("plug-in selection was revoked")?;
            ensure!(
                slot.generation == selection.generation,
                "plug-in selection was revoked"
            );
            if matches!(role, lifecycle::Role::ToolService) {
                ensure!(
                    slot.tool.is_none(),
                    "tool worker is already starting; retry"
                );
                let now = Instant::now();
                slot.attempts
                    .retain(|attempt| now.duration_since(*attempt) < Duration::from_secs(60));
                ensure!(
                    slot.attempts.len() < 3,
                    "tool worker restart limit reached; retry after one minute"
                );
                slot.attempts.push_back(now);
            }
            if view.is_some() {
                ensure!(
                    state
                        .views
                        .values()
                        .filter(|view| state
                            .workers
                            .get(&view.worker)
                            .is_some_and(|worker| worker.plugin == id))
                        .count()
                        < 8,
                    "plug-in view limit reached"
                );
            }
            let pending = PendingWorker::spawn(&self.executable)?;
            state.workers.insert(
                identity.clone(),
                Worker {
                    id: identity.clone(),
                    plugin: id.clone(),
                    generation: selection.generation.clone(),
                    cancel: pending.cancellation(),
                    phase: Phase::Starting,
                    endpoints,
                    rate_window: Instant::now(),
                    reads: 0,
                    last_reads: BTreeMap::new(),
                },
            );
            if let Some((connection, handle)) = &view {
                let lifecycle::Role::View(reference) = &role else {
                    bail!("invalid view worker role");
                };
                state.views.insert(
                    handle.clone(),
                    View {
                        connection: connection.clone(),
                        worker: identity.clone(),
                        reference: reference.clone(),
                    },
                );
            } else {
                state.slots.get_mut(&id).unwrap().tool = Some(identity.clone());
            }
            pending
        };
        let prepared = (|| {
            pending
                .prepare(
                    selection.package.component.clone(),
                    prepared_context,
                    selection.package.assets.clone(),
                    selection.package.schemas.clone(),
                )?
                .activate()
        })();
        let mut state = self.state.lock().unwrap();
        match prepared {
            Ok(active) => {
                if let Some(worker) = state.workers.get_mut(&identity) {
                    worker.phase = Phase::Running(Arc::new(active));
                    state.slots.get_mut(&id).unwrap().failure = None;
                    Ok(identity)
                } else {
                    active.cancel();
                    bail!("plug-in worker was revoked during readiness")
                }
            }
            Err(error) => {
                remove_worker(&mut state, &identity);
                if let Some(slot) = state
                    .slots
                    .get_mut(&id)
                    .filter(|slot| slot.generation == selection.generation)
                {
                    slot.failure = Some("worker preparation failed".into());
                }
                Err(error)
            }
        }
    }

    fn invoke(
        self: &Arc<Self>,
        connection: &str,
        min_interval_ms: u32,
        identity: &str,
        event: lifecycle::Message,
    ) -> Result<Reply> {
        ensure!(
            (250..=60_000).contains(&min_interval_ms),
            "read interval must be 250 to 60000 milliseconds"
        );
        if let lifecycle::Message::Tool(request) = &event {
            ensure!(
                request.arguments.len() <= 1024 * 1024,
                "command arguments exceed their byte budget"
            );
        }
        let process = {
            let state = self.state.lock().unwrap();
            let worker = admitted(&state, identity)?;
            let Phase::Running(process) = &worker.phase else {
                bail!("plug-in worker is starting");
            };
            process.clone()
        };
        let mut provider = Provider {
            manager: self.clone(),
            worker: identity.into(),
            epochs: BTreeSet::new(),
            remote_epochs: BTreeMap::new(),
            connection: connection.into(),
            min_interval_ms,
            read: BTreeSet::new(),
        };
        let response = process.invoke(event, &mut provider);
        if let Err(error) = response {
            if process.failed() {
                let mut state = self.state.lock().unwrap();
                if let Some(worker) = state.workers.get(identity) {
                    let plugin = worker.plugin.clone();
                    remove_worker(&mut state, identity);
                    state.slots.get_mut(&plugin).unwrap().failure =
                        Some("worker invocation failed; interrupted calls are not replayed".into());
                }
            }
            return Err(error);
        }
        // Recheck permission/generation and the local feed epoch after the child
        // reply, before admitting any result or frame to the frontend.
        let mut state = self.state.lock().unwrap();
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "plug-in manager is stopping"
        );
        admitted(&state, identity)?;
        if provider
            .remote_epochs
            .values()
            .any(|(reader, epoch)| reader.epoch().as_deref() != Some(epoch))
        {
            return Ok(Reply::Retry {
                reason: super::RetryReason::FeedChanged,
            });
        }
        if !provider.epochs.is_empty() {
            let plane = self.plane.upgrade().context("daemon is stopping")?;
            let epoch = plane.lock().unwrap().plugin_activity_epoch();
            if provider.epochs.len() != 1
                || !epoch.is_some_and(|epoch| provider.epochs.contains(&epoch))
            {
                return Ok(Reply::Retry {
                    reason: super::RetryReason::FeedChanged,
                });
            }
        }
        match response.unwrap() {
            lifecycle::Reply::Tool(response) => Ok(Reply::Tool {
                json: response.json,
                is_error: response.is_error,
            }),
            lifecycle::Reply::Frame(frame) => {
                let (handle, view) = state
                    .views
                    .iter()
                    .find(|(_, view)| view.worker == identity && view.reference.id == frame.id)
                    .context("view was revoked")?;
                let content = ValidatedFrame::new(frame.content, &view.reference.viewport)?;
                let bytes = postcard::to_allocvec(content.content())?;
                ensure!(
                    bytes.len() <= MAX_FRAME_BYTES,
                    "encoded frame exceeds its byte budget"
                );
                Ok(Reply::Frame {
                    next_tick_ms: frame.next_tick_ms,
                    handle: handle.clone(),
                    content: base64::engine::general_purpose::STANDARD.encode(bytes),
                })
            }
            lifecycle::Reply::Idle => Ok(Reply::Idle),
            lifecycle::Reply::Closed => {
                let handle = state
                    .views
                    .iter()
                    .find(|(_, view)| view.worker == identity)
                    .map(|(handle, _)| handle.clone())
                    .context("view was revoked")?;
                let cancel = remove_worker(&mut state, identity);
                drop(state);
                if let Some(cancel) = cancel {
                    cancel.cancel();
                }
                Ok(Reply::ViewClosed { handle })
            }
            _ => bail!("unexpected plug-in invocation reply"),
        }
    }

    fn open_view(
        self: &Arc<Self>,
        connection: &str,
        id: &str,
        contribution: String,
        viewport: frame::Viewport,
        preferences: frame::Preferences,
        endpoints: Option<Vec<super::ViewEndpoint>>,
    ) -> Result<Reply> {
        validate_viewport(&viewport)?;
        let selection = self.selection(id)?;
        ensure!(
            selection
                .package
                .manifest
                .contributions
                .iter()
                .any(|entry| matches!(entry, Contribution::View { id, .. } if id == &contribution)),
            "unknown view contribution"
        );
        let handle = nonce()?;
        let reference = lifecycle::ViewReference {
            preferences,
            id: number()?,
            contribution,
            viewport,
        };
        self.start(
            selection,
            lifecycle::Role::View(reference),
            Some((connection.into(), handle.clone())),
            endpoints,
        )?;
        Ok(Reply::ViewOpened { handle })
    }

    fn view_event(
        self: &Arc<Self>,
        connection: &str,
        handle: &str,
        event: frame::Event,
    ) -> Result<Reply> {
        if matches!(event, frame::Event::Close) {
            self.close_view(connection, handle)?;
            return Ok(Reply::Idle);
        }
        if let frame::Event::Resize(viewport) = &event {
            validate_viewport(viewport)?;
        }
        let view = {
            let mut state = self.state.lock().unwrap();
            let view = state
                .views
                .get_mut(handle)
                .filter(|view| view.connection == connection)
                .context("unknown view handle for this connection")?;
            if let frame::Event::Resize(viewport) = &event {
                view.reference.viewport = viewport.clone();
            }
            view.clone()
        };
        self.invoke(
            connection,
            super::protocol::default_read_interval(),
            &view.worker,
            lifecycle::Message::View(lifecycle::ViewEvent {
                id: view.reference.id,
                event,
            }),
        )
    }

    fn close_view(&self, connection: &str, handle: &str) -> Result<()> {
        let cancel = {
            let mut state = self.state.lock().unwrap();
            let view = state
                .views
                .get(handle)
                .filter(|view| view.connection == connection)
                .context("unknown view handle for this connection")?;
            let identity = view.worker.clone();
            remove_worker(&mut state, &identity)
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        Ok(())
    }

    fn close_connection(&self, connection: &str) {
        let cancellations = {
            let mut state = self.state.lock().unwrap();
            state.exports.retain(|(owner, _), _| owner != connection);
            for worker in state.workers.values_mut() {
                worker
                    .last_reads
                    .retain(|(owner, _), _| owner != connection);
            }
            let identities = state
                .views
                .values()
                .filter(|view| view.connection == connection)
                .map(|view| view.worker.clone())
                .collect::<Vec<_>>();
            identities
                .into_iter()
                .filter_map(|identity| remove_worker(&mut state, &identity))
                .collect::<Vec<_>>()
        };
        for cancel in cancellations {
            cancel.cancel();
        }
    }

    fn exports(&self, connection: &str, allowlist: Vec<String>) -> Result<Reply> {
        ensure!(allowlist.len() <= 16, "too many plug-ins in MCP allowlist");
        let mut tools = Vec::new();
        let mut names = BTreeSet::new();
        let mut selections = Vec::new();
        let mut bytes = 0usize;
        for id in allowlist.into_iter().collect::<BTreeSet<_>>() {
            super::manifest::validate_plugin_id(&id)?;
            {
                let state = self.state.lock().unwrap();
                if !matches!(
                    state.journal.selections.get(&id),
                    Some(Desired::Enabled { .. })
                ) || state.slots.get(&id).is_some_and(|slot| slot.incompatible)
                {
                    continue;
                }
            }
            let selection = self.selection(&id)?;
            for contribution in &selection.package.manifest.contributions {
                if let Contribution::Command {
                    id: contribution,
                    mcp_name: Some(name),
                    ..
                } = contribution
                {
                    ensure!(
                        names.insert(name.clone()),
                        "MCP export names conflict across enabled plug-ins"
                    );
                    let schema = &selection.package.schemas[contribution];
                    bytes += schema.input.len() + schema.output.len() + 1024;
                    ensure!(
                        bytes <= 1024 * 1024 && tools.len() < 128,
                        "MCP exports exceed their byte/count budget"
                    );
                    let handle = nonce()?;
                    tools.push(ToolExport {
                        handle: handle.clone(),
                        plugin: id.clone(),
                        contribution: contribution.clone(),
                        name: name.clone(),
                        input_schema: schema.input.clone(),
                        output_schema: schema.output.clone(),
                    });
                    selections.push((
                        handle,
                        Export {
                            plugin: id.clone(),
                            generation: selection.generation.clone(),
                            contribution: contribution.clone(),
                        },
                    ));
                }
            }
        }
        let mut state = self.state.lock().unwrap();
        for (_, export) in &selections {
            ensure!(
                state
                    .slots
                    .get(&export.plugin)
                    .is_some_and(|slot| slot.generation == export.generation),
                "plug-in exports changed during discovery; retry"
            );
        }
        state.exports.retain(|(owner, _), _| owner != connection);
        for (handle, export) in selections {
            state.exports.insert((connection.into(), handle), export);
        }
        Ok(Reply::Exports { tools })
    }
}

fn admitted<'a>(state: &'a State, identity: &str) -> Result<&'a Worker> {
    let worker = state
        .workers
        .get(identity)
        .context("plug-in worker was revoked")?;
    ensure!(
        matches!(
            state.journal.selections.get(&worker.plugin),
            Some(Desired::Enabled { .. })
        ) && state
            .slots
            .get(&worker.plugin)
            .is_some_and(|slot| slot.generation == worker.generation),
        "plug-in worker generation was revoked"
    );
    Ok(worker)
}

fn remove_worker(state: &mut State, identity: &str) -> Option<Cancellation> {
    state.views.retain(|_, view| view.worker != identity);
    let worker = state.workers.remove(identity)?;
    if let Some(slot) = state.slots.get_mut(&worker.plugin) {
        if slot.tool.as_deref() == Some(identity) {
            slot.tool = None;
        }
    }
    Some(worker.cancel)
}

struct Provider {
    manager: Arc<Manager>,
    worker: String,
    epochs: BTreeSet<String>,
    remote_epochs: BTreeMap<u64, (Arc<Remote>, String)>,
    connection: String,
    min_interval_ms: u32,
    read: BTreeSet<u64>,
}

impl ActivityProvider for Provider {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        if self.manager.stopped.load(Ordering::Acquire) {
            return Err(read::Error::Revoked);
        }
        let state = self
            .manager
            .state
            .lock()
            .map_err(|_| read::Error::Revoked)?;
        let worker = admitted(&state, &self.worker).map_err(|_| read::Error::Revoked)?;
        let mut handles = worker.endpoints.iter().collect::<Vec<_>>();
        handles.sort_by_key(|(_, (index, _, _))| *index);
        let endpoints = handles
            .into_iter()
            .map(|(handle, (_, label, access))| read::Endpoint {
                handle: *handle,
                label: label.clone(),
                kind: match access {
                    Access::Local => read::EndpointKind::Local,
                    Access::Remote { endpoint, .. } => match endpoint.source {
                        super::EndpointSource::Socket { .. } => read::EndpointKind::Socket,
                        _ => read::EndpointKind::Command,
                    },
                },
                min_interval_ms: self.min_interval_ms,
            })
            .collect::<Vec<_>>();
        Ok(endpoints)
    }

    fn poll(
        &mut self,
        endpoint: u64,
        after: Option<ActivityCursor>,
        limit: u16,
    ) -> Result<ActivityPage, read::Error> {
        if self.manager.stopped.load(Ordering::Acquire) {
            return Err(read::Error::Revoked);
        }
        let mut state = self
            .manager
            .state
            .lock()
            .map_err(|_| read::Error::Revoked)?;
        let worker = admitted(&state, &self.worker).map_err(|_| read::Error::Revoked)?;
        let (_, _, access) = worker
            .endpoints
            .get(&endpoint)
            .cloned()
            .ok_or(read::Error::UnknownEndpoint)?;
        if !(1..=256).contains(&limit) {
            return Err(read::Error::InvalidRequest);
        }
        let worker = state
            .workers
            .get_mut(&self.worker)
            .ok_or(read::Error::Revoked)?;
        let now = Instant::now();
        if !self.read.contains(&endpoint) {
            let key = (self.connection.clone(), endpoint);
            if worker.last_reads.get(&key).is_some_and(|last| {
                now.duration_since(*last) < Duration::from_millis(u64::from(self.min_interval_ms))
            }) {
                return Err(read::Error::RateLimited);
            }
            if !worker.last_reads.contains_key(&key) && worker.last_reads.len() >= 4096 {
                return Err(read::Error::RateLimited);
            }
            worker.last_reads.insert(key, now);
            self.read.insert(endpoint);
        }
        if now.duration_since(worker.rate_window) >= Duration::from_secs(1) {
            worker.rate_window = now;
            worker.reads = 0;
        }
        if worker.reads >= pmux_plugin_api::MAX_ACTIVITY_READS_PER_SECOND {
            return Err(read::Error::RateLimited);
        }
        worker.reads += 1;
        if let Access::Remote { reader, .. } = access {
            drop(state);
            let page = reader.page(after, limit)?;
            if let Some((_, previous)) = self.remote_epochs.get(&endpoint) {
                if previous != &page.epoch {
                    return Err(read::Error::Interrupted);
                }
            }
            self.remote_epochs
                .insert(endpoint, (reader, page.epoch.clone()));
            return Ok(page);
        }
        let plane = self.manager.plane.upgrade().ok_or(read::Error::Revoked)?;
        let page = plane
            .lock()
            .map_err(|_| read::Error::Revoked)?
            .plugin_activity_page(after, limit)?;
        self.epochs.insert(page.epoch.clone());
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket() -> PathBuf {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("pmux-plugin-journal-{}", nonce().unwrap()));
        private_directory(&root).unwrap();
        root.join("daemon.sock")
    }

    #[test]
    fn disabled_state_survives_restart_and_retry_ids_cannot_change_their_action() {
        let socket = socket();
        let manager = Manager::new(&socket, Weak::new()).unwrap();
        let action = AdminAction::Disable {
            id: "org.moonbase.example".into(),
        };
        let retry = "a".repeat(32);
        let first = manager.admin(retry.clone(), action.clone()).unwrap();
        let generation = manager.state.lock().unwrap().slots["org.moonbase.example"]
            .generation
            .clone();
        assert_eq!(manager.admin(retry.clone(), action).unwrap(), first);
        assert_eq!(
            manager.state.lock().unwrap().slots["org.moonbase.example"].generation,
            generation
        );
        assert!(manager
            .admin(
                retry,
                AdminAction::Disable {
                    id: "org.moonbase.another".into()
                }
            )
            .is_err());
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(socket.with_extension("plugins.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved["selections"]["org.moonbase.example"],
            serde_json::json!({"state":"disabled","selected":null})
        );
        assert_eq!(saved["completed"].as_array().unwrap().len(), 1);
        drop(manager);
        let restarted = Manager::new(&socket, Weak::new()).unwrap();
        let state = restarted.state.lock().unwrap();
        assert!(!status(&state, "org.moonbase.example").enabled);
        assert_ne!(state.slots["org.moonbase.example"].generation, generation);
        drop(state);
        drop(restarted);
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn disabled_and_uninstalled_plugins_have_no_mcp_exports() {
        let socket = socket();
        let manager = Manager::new(&socket, Weak::new()).unwrap();
        manager
            .admin(
                "a".repeat(32),
                AdminAction::Disable {
                    id: "org.moonbase.example".into(),
                },
            )
            .unwrap();
        assert_eq!(
            manager
                .exports(
                    "connection",
                    vec![
                        "org.moonbase.example".into(),
                        "org.moonbase.uninstalled".into()
                    ]
                )
                .unwrap(),
            Reply::Exports { tools: Vec::new() }
        );
        assert!(manager
            .exports("connection", vec!["invalid id".into()])
            .is_err());
        drop(manager);
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn only_one_manager_can_write_an_instance_journal() {
        let socket = socket();
        let first = Manager::new(&socket, Weak::new()).unwrap();
        assert!(Manager::new(&socket, Weak::new()).is_err());
        drop(first);
        let second = Manager::new(&socket, Weak::new()).unwrap();
        drop(second);
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_journal_commit_changes_neither_desired_state_nor_retry_history() {
        let socket = socket();
        let manager = Manager::new(&socket, Weak::new()).unwrap();
        std::fs::create_dir(socket.with_extension("plugins.json")).unwrap();
        assert!(manager
            .admin(
                "a".repeat(32),
                AdminAction::Disable {
                    id: "org.moonbase.example".into()
                }
            )
            .is_err());
        let state = manager.state.lock().unwrap();
        assert!(state.journal.selections.is_empty());
        assert!(state.journal.completed.is_empty());
        assert!(state.slots.is_empty());
        drop(state);
        drop(manager);
        std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn unconfigured_scope_strings_never_grant_native_authority() {
        assert!(validate_grants(
            &BTreeSet::from(["activity.read:local".into()]),
            &BTreeMap::new()
        )
        .is_ok());
        for scope in [
            "activity.write:local",
            "pty.read:local",
            "activity.read:/tmp/daemon.sock",
            "activity.read:*",
            "activity.read:endpoint:unknown",
        ] {
            assert!(validate_grants(&BTreeSet::from([scope.into()]), &BTreeMap::new()).is_err());
        }
    }
}
