// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context as _, Result};
use pmux_plugin_api::host::pmux::{activity::read, plugin::host, plugin::lifecycle, view::frame};
use pmux_plugin_api::host::{Plugin, PluginPre};
use pmux_plugin_api::{
    validate_viewport, ValidatedFrame, API_VERSION, CONTRACTS, IMPORT_CONTRACTS, MAX_FRAME_BYTES,
};
use prismattyc_core::activity as domain;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Strategy};

const MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESULT_BYTES: u32 = 1024 * 1024;
const CALL_FUEL: u64 = 200_000_000;
const CALL_EPOCHS: u64 = 100;

/// Host-owned endpoint policy. Check current grants/generation on every call,
/// and independently bound and cancel external I/O.
pub trait ActivityProvider: Send {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error>;
    fn poll(
        &mut self,
        endpoint: u64,
        after: Option<domain::ActivityCursor>,
        limit: u16,
    ) -> Result<domain::ActivityPage, read::Error>;
}

pub struct Runtime {
    engine: Engine,
}

impl Runtime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .strategy(Strategy::Cranelift)
            .target("pulley64")?
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true)
            .max_wasm_stack(512 * 1024);
        Ok(Self {
            engine: Engine::new(&config)?,
        })
    }

    /// Check the complete import/export contract without instantiating guest code.
    pub fn validate_component(&self, bytes: &[u8]) -> Result<()> {
        self.component(bytes)?;
        Ok(())
    }

    fn component(&self, bytes: &[u8]) -> Result<PluginPre<HostState>> {
        ensure!(bytes.len() <= MAX_COMPONENT_BYTES, "component is too large");
        ensure!(
            bytes.starts_with(b"\0asm"),
            "expected standard component bytes"
        );
        let component = Component::new(&self.engine, bytes)?;
        for (name, _) in component.component_type().imports(&self.engine) {
            ensure!(
                IMPORT_CONTRACTS.contains(&name),
                "unsupported component import"
            );
        }
        let mut linker = Linker::<HostState>::new(&self.engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        PluginPre::new(linker.instantiate_pre(&component)?)
    }

    /// Compile and complete readiness without admitting data reads or output.
    pub fn prepare(
        &self,
        bytes: &[u8],
        context: lifecycle::Context,
        provider: Box<dyn ActivityProvider>,
        assets: BTreeMap<String, Vec<u8>>,
    ) -> Result<PreparedComponent> {
        validate_context(&context)?;
        ensure!(assets.len() <= 128, "too many component assets");
        let mut asset_bytes = 0usize;
        for (name, bytes) in &assets {
            ensure!(valid_asset_name(name), "invalid component asset name");
            ensure!(bytes.len() <= 1024 * 1024, "component asset is too large");
            asset_bytes = asset_bytes
                .checked_add(bytes.len())
                .context("asset size overflow")?;
            ensure!(
                asset_bytes <= 4 * 1024 * 1024,
                "component assets exceed their budget"
            );
        }
        let prepared = self.component(bytes)?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(context.limits.memory_bytes as usize)
            .table_elements(16_384)
            .instances(16)
            .memories(8)
            .tables(8)
            .build();
        let activity = context
            .capabilities
            .iter()
            .any(|capability| capability.name == "pmux:activity/read" && capability.available);
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits,
                phase: Phase::Prepared,
                activity,
                provider,
                assets,
                started: Instant::now(),
                imports: BTreeMap::new(),
                calls: 0,
            },
        );
        store.limiter(|state| &mut state.limits);
        reset_budget(&mut store)?;
        let timer = EpochTimer::start(self.engine.clone())?;
        let plugin = prepared.instantiate(&mut store)?;
        let reply = plugin.call_handle(&mut store, &lifecycle::Message::Start(context.clone()))?;
        match reply {
            Ok(lifecycle::Reply::Ready(version)) if version == API_VERSION => {}
            _ => bail!("component did not confirm its negotiated API"),
        }
        Ok(PreparedComponent(Instance {
            store,
            plugin,
            context,
            _timer: timer,
        }))
    }
}

pub struct PreparedComponent(Instance);
pub struct ActiveComponent(Instance);

impl PreparedComponent {
    /// The manager calls this only after committing selection and generation.
    pub fn activate(mut self) -> ActiveComponent {
        self.0.store.data_mut().phase = Phase::Active;
        ActiveComponent(self.0)
    }
}

impl ActiveComponent {
    pub fn handle(&mut self, event: lifecycle::Message) -> Result<lifecycle::Reply> {
        ensure!(
            matches!(self.0.store.data().phase, Phase::Active),
            "component view has closed"
        );
        if let lifecycle::Message::Tool(request) = &event {
            ensure!(
                request.id != 0
                    && !request.contribution.is_empty()
                    && request.contribution.len() <= 64,
                "invalid component invocation"
            );
            ensure!(
                request.arguments.len() <= MAX_RESULT_BYTES as usize,
                "component arguments exceed their byte budget"
            );
            serde_json::from_str::<serde_json::Value>(&request.arguments)
                .context("invalid component argument JSON")?;
        }
        match (&self.0.context.role, &event) {
            (lifecycle::Role::ToolService, lifecycle::Message::Tool(_)) => {}
            (lifecycle::Role::View(view), lifecycle::Message::View(message))
                if view.id == message.id => {}
            _ => bail!("event does not match the component role"),
        }
        if let (lifecycle::Role::View(view), lifecycle::Message::View(message)) =
            (&mut self.0.context.role, &event)
        {
            if let frame::Event::Resize(viewport) = &message.event {
                validate_viewport(viewport)?;
                view.viewport = *viewport;
            }
        }
        reset_budget(&mut self.0.store)?;
        self.0.store.data_mut().imports.clear();
        self.0.store.data_mut().calls = 0;
        let started = Instant::now();
        let result = self.0.plugin.call_handle(&mut self.0.store, &event);
        let reply = result.map_err(|error| {
            let imports = &self.0.store.data().imports;
            let host = imports.values().map(|value| value.2).sum::<Duration>();
            let calls = imports.values().map(|value| value.0).sum::<u16>();
            let records = imports.values().map(|value| value.1).sum::<usize>();
            error.context(format!(
                "component call: wall={}ms provider={}ms calls={calls} records={records} endpoints={}",
                started.elapsed().as_millis(),
                host.as_millis(),
                imports.keys().filter(|endpoint| **endpoint != 0).count(),
            ))
        })?
            .map_err(|_| anyhow::anyhow!("component returned an invocation error"))?;
        match (&event, reply) {
            (lifecycle::Message::Tool(request), lifecycle::Reply::Tool(response))
                if request.id == response.id =>
            {
                ensure!(
                    response.json.len() <= self.0.context.limits.result_bytes as usize,
                    "component result exceeds its byte budget"
                );
                serde_json::from_str::<serde_json::Value>(&response.json)
                    .context("component returned invalid result JSON")?;
                Ok(lifecycle::Reply::Tool(response))
            }
            (lifecycle::Message::View(message), lifecycle::Reply::Frame(result))
                if message.id == result.id =>
            {
                let lifecycle::Role::View(view) = &self.0.context.role else {
                    bail!("frame from a component without a view");
                };
                let content = ValidatedFrame::new(result.content, &view.viewport)?;
                ensure!(
                    (33..=1000).contains(&result.next_tick_ms),
                    "component frame refresh interval exceeds its bounds"
                );
                ensure!(
                    content.byte_bound() <= self.0.context.limits.frame_bytes as usize,
                    "component frame exceeds its byte budget"
                );
                Ok(lifecycle::Reply::Frame(lifecycle::ViewFrame {
                    next_tick_ms: result.next_tick_ms,
                    id: result.id,
                    content: content.into_content(),
                }))
            }
            (lifecycle::Message::View(_), lifecycle::Reply::Idle) => Ok(lifecycle::Reply::Idle),
            (lifecycle::Message::View(_), lifecycle::Reply::Closed) => {
                self.0.store.data_mut().phase = Phase::Revoked;
                Ok(lifecycle::Reply::Closed)
            }
            _ => bail!("component reply does not match its invocation"),
        }
    }

    pub fn stop(mut self) -> Result<()> {
        self.0.store.data_mut().phase = Phase::Revoked;
        reset_budget(&mut self.0.store)?;
        match self
            .0
            .plugin
            .call_handle(&mut self.0.store, &lifecycle::Message::Stop)?
        {
            Ok(lifecycle::Reply::Idle) => Ok(()),
            _ => bail!("component did not stop cleanly"),
        }
    }
}

struct Instance {
    store: Store<HostState>,
    plugin: Plugin,
    context: lifecycle::Context,
    _timer: EpochTimer,
}

#[derive(Clone, Copy)]
enum Phase {
    Prepared,
    Active,
    Revoked,
}

struct HostState {
    limits: StoreLimits,
    phase: Phase,
    activity: bool,
    provider: Box<dyn ActivityProvider>,
    assets: BTreeMap<String, Vec<u8>>,
    started: Instant,
    imports: BTreeMap<u64, (u16, usize, Duration)>,
    calls: u16,
}

impl frame::Host for HostState {}
impl pmux_plugin_api::host::pmux::tool::call::Host for HostState {}
impl lifecycle::Host for HostState {}

impl HostState {
    fn admit_read(&mut self) -> Result<(), read::Error> {
        match self.phase {
            Phase::Prepared => Err(read::Error::NotActivated),
            Phase::Revoked => Err(read::Error::Revoked),
            Phase::Active if !self.activity => Err(read::Error::PermissionDenied),
            Phase::Active => Ok(()),
        }?;
        if self.calls >= pmux_plugin_api::MAX_HOST_IMPORTS_PER_CALL {
            return Err(read::Error::RateLimited);
        }
        self.calls += 1;
        Ok(())
    }
}

impl read::Host for HostState {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        self.admit_read()?;
        let started = Instant::now();
        let result = self.provider.endpoints();
        self.imports.entry(0).or_default().0 += 1;
        self.imports.entry(0).or_default().2 += started.elapsed();
        let endpoints = result?;
        if endpoints.len() > 32
            || endpoints.iter().any(|endpoint| {
                endpoint.label.is_empty()
                    || endpoint.label.len() > 64
                    || !endpoint.label.is_ascii()
                    || endpoint.label.chars().any(char::is_control)
                    || !(250..=60_000).contains(&endpoint.min_interval_ms)
            })
        {
            return Err(read::Error::InvalidRequest);
        }
        Ok(endpoints)
    }

    fn sessions(&mut self, endpoint: u64) -> Result<Option<read::SessionSnapshot>, read::Error> {
        Ok(convert_sessions(
            self.activity_page(endpoint, None, 1)?.sessions,
        ))
    }

    fn poll(
        &mut self,
        endpoint: u64,
        after: Option<read::Cursor>,
        limit: u16,
    ) -> Result<read::Page, read::Error> {
        Ok(convert_page(self.activity_page(endpoint, after, limit)?))
    }
}

impl host::Host for HostState {
    fn now(&mut self) -> frame::Clock {
        frame::Clock {
            wall_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
            monotonic_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        }
    }

    fn asset(&mut self, name: String) -> Result<Vec<u8>, String> {
        if matches!(self.phase, Phase::Revoked) {
            return Err("component revoked".into());
        }
        if !valid_asset_name(&name) {
            return Err("invalid asset name".into());
        }
        self.assets
            .get(&name)
            .cloned()
            .ok_or_else(|| "asset unavailable".into())
    }
}

fn validate_context(context: &lifecycle::Context) -> Result<()> {
    ensure!(
        context.api_version == API_VERSION,
        "incompatible component API"
    );
    ensure!(
        context.limits.memory_bytes >= 1024 * 1024
            && context.limits.memory_bytes <= MAX_MEMORY_BYTES,
        "invalid component memory limit"
    );
    ensure!(
        context.limits.result_bytes > 0 && context.limits.result_bytes <= MAX_RESULT_BYTES,
        "invalid component result limit"
    );
    ensure!(
        context.limits.frame_bytes > 0 && context.limits.frame_bytes as usize <= MAX_FRAME_BYTES,
        "invalid component frame limit"
    );
    let mut names = BTreeSet::new();
    for capability in &context.capabilities {
        ensure!(
            names.insert(&capability.name),
            "duplicate component capability"
        );
        ensure!(
            CONTRACTS
                .iter()
                .any(|(name, version)| *name == capability.name && *version == capability.version),
            "unsupported component capability"
        );
    }
    if let lifecycle::Role::View(view) = &context.role {
        ensure!(
            view.id != 0 && !view.contribution.is_empty() && view.contribution.len() <= 64,
            "invalid component view"
        );
        validate_viewport(&view.viewport)?;
    }
    let required = match context.role {
        lifecycle::Role::ToolService => "pmux:tool/call",
        lifecycle::Role::View(_) => "pmux:view/frame",
    };
    ensure!(
        context
            .capabilities
            .iter()
            .any(|capability| capability.name == required && capability.available),
        "component role is not admitted"
    );
    Ok(())
}

fn valid_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('/')
        && !name.contains('\\')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !name.chars().any(char::is_control)
}

fn reset_budget(store: &mut Store<HostState>) -> Result<()> {
    store.set_fuel(CALL_FUEL)?;
    store.set_epoch_deadline(CALL_EPOCHS);
    Ok(())
}

struct EpochTimer {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl EpochTimer {
    fn start(engine: Engine) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pmux-plugin-epoch".into())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(Duration::from_millis(10)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    engine.increment_epoch();
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EpochTimer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn convert_page(page: domain::ActivityPage) -> read::Page {
    read::Page {
        epoch: page.epoch,
        oldest_sequence: page.oldest_sequence,
        current_sequence: page.current_sequence,
        records: page
            .records
            .into_iter()
            .map(|record| read::ActivityRecord {
                sequence: record.sequence,
                received_at_ms: record.received_at_ms,
                event: read::Input {
                    schema_version: record.event.schema_version,
                    seat: record.event.seat.to_string(),
                    run_id: record.event.run_id,
                    event_id: record.event.event_id,
                    source: match record.event.source {
                        domain::ActivitySource::Notify => read::Source::Notify,
                        domain::ActivitySource::Hooks => read::Source::Hooks,
                        domain::ActivitySource::Runtime => read::Source::Runtime,
                    },
                    kind: convert_kind(record.event.kind),
                    detail: record.event.detail.map(|detail| read::Detail {
                        tool: detail.tool,
                        file: detail.file,
                        command: detail.command,
                        duration_ms: detail.duration_ms,
                        exit_code: detail.exit_code,
                        task: detail.task,
                        operation_id: detail.operation_id,
                        model: detail.model,
                    }),
                },
            })
            .collect(),
        cursor: read::Cursor {
            epoch: page.cursor.epoch,
            sequence: page.cursor.sequence,
        },
        has_more: page.has_more,
        reset: page.reset.map(|reset| match reset {
            domain::ActivityReset::Restarted => read::Reset::Restarted,
            domain::ActivityReset::RetentionGap => read::Reset::RetentionGap,
            domain::ActivityReset::AheadOfFeed => read::Reset::AheadOfFeed,
        }),
    }
}

fn convert_kind(kind: domain::ActivityKind) -> read::Kind {
    use domain::ActivityKind as K;
    match kind {
        K::SessionStarted => read::Kind::SessionStarted,
        K::SessionFinished => read::Kind::SessionFinished,
        K::TurnStarted => read::Kind::TurnStarted,
        K::TurnFinished => read::Kind::TurnFinished,
        K::ToolStarted => read::Kind::ToolStarted,
        K::ToolFinished => read::Kind::ToolFinished,
        K::ToolFailed => read::Kind::ToolFailed,
        K::CommandStarted => read::Kind::CommandStarted,
        K::CommandFinished => read::Kind::CommandFinished,
        K::CommandFailed => read::Kind::CommandFailed,
        K::EditStarted => read::Kind::EditStarted,
        K::EditFinished => read::Kind::EditFinished,
        K::EditFailed => read::Kind::EditFailed,
        K::TestStarted => read::Kind::TestStarted,
        K::TestPassed => read::Kind::TestPassed,
        K::TestFailed => read::Kind::TestFailed,
        K::Working => read::Kind::Working,
        K::Waiting => read::Kind::Waiting,
        K::Stopped => read::Kind::Stopped,
        K::MailSent => read::Kind::MailSent,
        K::MailReceived => read::Kind::MailReceived,
    }
}

fn convert_sessions(sessions: Option<domain::ActivitySessions>) -> Option<read::SessionSnapshot> {
    sessions.map(|snapshot| read::SessionSnapshot {
        observed_at_ms: snapshot.observed_at_ms,
        seats: snapshot
            .seats
            .into_iter()
            .map(|session| read::Session {
                seat: session.seat.to_string(),
                session_id: session.session_id,
                state: match session.state {
                    domain::ActivitySessionState::Working => read::SessionState::Working,
                    domain::ActivitySessionState::Waiting => read::SessionState::Waiting,
                    domain::ActivitySessionState::Idle => read::SessionState::Idle,
                    domain::ActivitySessionState::Stuck => read::SessionState::Stuck,
                    domain::ActivitySessionState::Stopped => read::SessionState::Stopped,
                },
                last_output_at_ms: session.last_output_at_ms,
                harness: session.harness,
                model: session.model,
                quota: session.quota,
                project: session.project,
            })
            .collect(),
    })
}

impl HostState {
    fn activity_page(
        &mut self,
        endpoint: u64,
        after: Option<read::Cursor>,
        limit: u16,
    ) -> Result<domain::ActivityPage, read::Error> {
        self.admit_read()?;
        if limit == 0
            || limit > 256
            || after.as_ref().is_some_and(|cursor| {
                cursor.epoch.is_empty() || cursor.epoch.len() > 128 || !cursor.epoch.is_ascii()
            })
        {
            return Err(read::Error::InvalidRequest);
        }
        let after = after.map(|cursor| domain::ActivityCursor {
            epoch: cursor.epoch,
            sequence: cursor.sequence,
        });
        let started = Instant::now();
        let result = self.provider.poll(endpoint, after, limit);
        let metric = self.imports.entry(endpoint).or_default();
        metric.0 += 1;
        metric.1 += result.as_ref().map_or(0, |page| page.records.len());
        metric.2 += started.elapsed();
        let page = result?;
        if page.records.len() > usize::from(limit)
            || page
                .records
                .iter()
                .any(|record| record.event.schema_version != 1)
        {
            return Err(read::Error::IncompatibleSchema);
        }
        if page.epoch.is_empty()
            || page.epoch.len() > 128
            || page.epoch.chars().any(char::is_control)
            || page.cursor.epoch != page.epoch
            || page
                .records
                .iter()
                .any(|record| record.event.validate_metadata().is_err())
        {
            return Err(read::Error::InvalidRequest);
        }
        if page
            .sessions
            .as_ref()
            .is_some_and(|snapshot| snapshot.seats.len() > 4096)
        {
            return Err(read::Error::InvalidRequest);
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmux_plugin_fixture::{component, module};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn static_validation_does_not_run_a_core_start_function() {
        use wasm_encoder::Section;
        let mut function = None;
        let mut sections = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(module()) {
            let payload = payload.unwrap();
            if let wasmparser::Payload::ExportSection(exports) = &payload {
                for export in exports.clone() {
                    let export = export.unwrap();
                    if export.name == "fixture_start" {
                        function = Some(export.index);
                    }
                }
            }
            if let Some((id, range)) = payload.as_section() {
                sections.push((id, range));
            }
        }
        let start = wasm_encoder::StartSection {
            function_index: function.unwrap(),
        };
        let mut modified = wasm_encoder::Module::new();
        let mut inserted = false;
        for (id, range) in sections {
            if id > start.id() && !inserted {
                modified.section(&start);
                inserted = true;
            }
            modified.section(&wasm_encoder::RawSection {
                id,
                data: &module()[range],
            });
        }
        assert!(inserted);
        let bytes = wit_component::ComponentEncoder::default()
            .module(&modified.finish())
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        let runtime = Runtime::new().unwrap();
        runtime.validate_component(&bytes).unwrap();
        let (provider, _, _) = provider();
        let error = runtime
            .prepare(
                &bytes,
                context(false, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .err()
            .expect("infinite core start was not invoked during preparation");
        assert!(matches!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(wasmtime::Trap::OutOfFuel | wasmtime::Trap::Interrupt)
        ));
    }

    struct Provider {
        on: Arc<AtomicBool>,
        reads: Arc<AtomicUsize>,
        invalid_metadata: bool,
    }

    impl ActivityProvider for Provider {
        fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
            Ok(vec![read::Endpoint {
                handle: 7,
                label: "local".into(),
                kind: read::EndpointKind::Local,
                min_interval_ms: 250,
            }])
        }

        fn poll(
            &mut self,
            endpoint: u64,
            _: Option<domain::ActivityCursor>,
            _: u16,
        ) -> Result<domain::ActivityPage, read::Error> {
            if endpoint != 7 {
                return Err(read::Error::UnknownEndpoint);
            }
            if !self.on.load(Ordering::Acquire) {
                return Err(read::Error::FeedDisabled);
            }
            self.reads.fetch_add(1, Ordering::AcqRel);
            Ok(domain::ActivityPage {
                epoch: "fixture-epoch".into(),
                oldest_sequence: 1,
                current_sequence: 1,
                records: vec![domain::ActivityRecord {
                    sequence: 1,
                    received_at_ms: 1234,
                    event: domain::ActivityInput {
                        schema_version: 1,
                        seat: prismattyc_core::AgentId::new("fixture-seat").unwrap(),
                        run_id: if self.invalid_metadata {
                            "invalid/path"
                        } else {
                            "run-1"
                        }
                        .into(),
                        event_id: "event-1".into(),
                        source: domain::ActivitySource::Hooks,
                        kind: domain::ActivityKind::Waiting,
                        detail: None,
                        process_ids: Vec::new(),
                    },
                }],
                cursor: domain::ActivityCursor {
                    epoch: "fixture-epoch".into(),
                    sequence: 1,
                },
                has_more: false,
                reset: None,
                sessions: None,
            })
        }
    }

    fn context(available: bool, role: lifecycle::Role) -> lifecycle::Context {
        let role_capability = match role {
            lifecycle::Role::ToolService => "pmux:tool/call",
            lifecycle::Role::View(_) => "pmux:view/frame",
        };
        lifecycle::Context {
            api_version: API_VERSION.into(),
            capabilities: vec![
                lifecycle::Capability {
                    name: "pmux:activity/read".into(),
                    version: "1.3.0".into(),
                    available,
                },
                lifecycle::Capability {
                    name: role_capability.into(),
                    version: "1.0.0".into(),
                    available: true,
                },
            ],
            limits: lifecycle::Limits {
                memory_bytes: 4 * 1024 * 1024,
                result_bytes: MAX_RESULT_BYTES,
                frame_bytes: MAX_FRAME_BYTES as u32,
            },
            role,
        }
    }

    fn provider() -> (Box<Provider>, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let on = Arc::new(AtomicBool::new(true));
        let reads = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Provider {
                on: on.clone(),
                reads: reads.clone(),
                invalid_metadata: false,
            }),
            on,
            reads,
        )
    }

    fn tool(contribution: &str) -> lifecycle::Message {
        lifecycle::Message::Tool(pmux_plugin_api::host::pmux::tool::call::Request {
            id: 1,
            contribution: contribution.into(),
            arguments: "{}".into(),
        })
    }

    fn result_json(reply: lifecycle::Reply) -> serde_json::Value {
        let lifecycle::Reply::Tool(response) = reply else {
            panic!("expected tool result");
        };
        serde_json::from_str(&response.json).unwrap()
    }

    #[test]
    fn real_component_cannot_read_during_preparation_or_without_a_grant() {
        let runtime = Runtime::new().unwrap();
        let (provider, _, reads) = provider();
        let prepared = runtime
            .prepare(
                component(),
                context(false, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(reads.load(Ordering::Acquire), 0);
        let mut active = prepared.activate();
        assert_eq!(
            result_json(active.handle(tool("poll")).unwrap()),
            serde_json::json!({"error":"permission-denied"})
        );
        assert_eq!(reads.load(Ordering::Acquire), 0);
        active.stop().unwrap();
    }

    #[test]
    fn a_running_component_observes_its_provider_on_every_read() {
        let runtime = Runtime::new().unwrap();
        let (provider, on, reads) = provider();
        let mut active = runtime
            .prepare(
                component(),
                context(true, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .unwrap()
            .activate();
        assert_eq!(
            result_json(active.handle(tool("poll")).unwrap()),
            serde_json::json!({
                "epoch":"fixture-epoch", "sequence":1, "seat":"fixture-seat",
            })
        );
        on.store(false, Ordering::Release);
        assert_eq!(
            result_json(active.handle(tool("poll")).unwrap()),
            serde_json::json!({"error":"feed-disabled"})
        );
        on.store(true, Ordering::Release);
        assert_eq!(
            result_json(active.handle(tool("poll")).unwrap()),
            serde_json::json!({
                "epoch":"fixture-epoch", "sequence":1, "seat":"fixture-seat",
            })
        );
        assert_eq!(reads.load(Ordering::Acquire), 2);
        active.stop().unwrap();
    }

    #[test]
    fn a_component_role_requires_its_declared_capability() {
        let runtime = Runtime::new().unwrap();
        let (provider, _, _) = provider();
        let mut context = context(true, lifecycle::Role::ToolService);
        context
            .capabilities
            .retain(|capability| capability.name != "pmux:tool/call");
        let error = runtime
            .prepare(component(), context, provider, BTreeMap::new())
            .err()
            .expect("role admitted without its capability");
        assert_eq!(error.to_string(), "component role is not admitted");
    }

    #[test]
    fn non_metadata_identifiers_do_not_cross_the_component_import() {
        let runtime = Runtime::new().unwrap();
        let (mut provider, _, _) = provider();
        provider.invalid_metadata = true;
        let mut active = runtime
            .prepare(
                component(),
                context(true, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .unwrap()
            .activate();
        assert_eq!(
            result_json(active.handle(tool("poll")).unwrap()),
            serde_json::json!({"error":"invalid-request"})
        );
        active.stop().unwrap();
    }

    #[test]
    fn frames_from_a_real_component_are_validated_before_delivery() {
        let runtime = Runtime::new().unwrap();
        let (provider, _, _) = provider();
        let role = lifecycle::Role::View(lifecycle::ViewReference {
            preferences: frame::Preferences::default(),
            id: 7,
            contribution: "main".into(),
            viewport: frame::Viewport {
                columns: 3,
                rows: 1,
                pixels: None,
            },
        });
        let mut active = runtime
            .prepare(component(), context(false, role), provider, BTreeMap::new())
            .unwrap()
            .activate();
        let tick = |wall_ms| {
            lifecycle::Message::View(lifecycle::ViewEvent {
                id: 7,
                event: frame::Event::Tick(frame::Clock {
                    wall_ms,
                    monotonic_ms: 0,
                }),
            })
        };
        let lifecycle::Reply::Frame(frame) = active.handle(tick(1)).unwrap() else {
            panic!("expected frame");
        };
        assert_eq!(frame.content.cells[0].text, "F");
        assert!(active
            .handle(tick(666))
            .unwrap_err()
            .to_string()
            .contains("printable grapheme"));
        active.stop().unwrap();
    }

    #[test]
    fn a_real_component_cannot_grow_past_its_memory_limit() {
        let runtime = Runtime::new().unwrap();
        let (provider, _, _) = provider();
        let mut active = runtime
            .prepare(
                component(),
                context(false, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .unwrap()
            .activate();
        assert_eq!(
            result_json(active.handle(tool("grow")).unwrap()),
            serde_json::json!({"allocation_denied":true})
        );
        active.stop().unwrap();
    }

    #[test]
    fn an_infinite_component_loop_is_interrupted() {
        let runtime = Runtime::new().unwrap();
        let (provider, _, _) = provider();
        let mut active = runtime
            .prepare(
                component(),
                context(false, lifecycle::Role::ToolService),
                provider,
                BTreeMap::new(),
            )
            .unwrap()
            .activate();
        let start = Instant::now();
        let error = active.handle(tool("loop")).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(wasmtime::Trap::OutOfFuel | wasmtime::Trap::Interrupt)
        ));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
