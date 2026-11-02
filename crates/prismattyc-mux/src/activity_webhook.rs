//! Signed outbound activity notifications. Secrets and endpoint details stay
//! out of errors and logs; the daemon owns all network access.

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use prismattyc_core::activity::{ActivityCursor, ActivityInput, ActivityKind, ActivitySource};
use prismattyc_core::AgentId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

use crate::control::{ControlRequest, ControlResponseData, PROTOCOL_VERSION};

type HmacSha256 = Hmac<Sha256>;

const MAX_SECRET_BYTES: u64 = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEBOUNCE: Duration = Duration::from_millis(750);
const MAX_QUEUE: usize = 64;
const MAX_ATTEMPTS: u8 = 6;
const MAX_HOOKED_SEATS: usize = 512;
const MAX_HOOKED_RUNS_PER_SEAT: usize = 16;
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct WebhookConfig {
    endpoint: Url,
    secret: Vec<u8>,
}

impl WebhookConfig {
    pub fn from_mux(section: &crate::config::MuxSection) -> Result<Option<Self>> {
        if section.activity_webhook_secret.is_some() {
            bail!("inline webhook secrets are not supported; configure a secret file");
        }
        match (
            section.activity_webhook_url.as_deref(),
            section.activity_webhook_secret_file.as_ref(),
        ) {
            (None, None) => Ok(None),
            (Some(raw_url), Some(secret_path)) => {
                let endpoint =
                    Url::parse(raw_url).map_err(|_| anyhow::anyhow!("invalid webhook URL"))?;
                if !matches!(endpoint.scheme(), "http" | "https")
                    || endpoint.host_str().is_none()
                    || endpoint.fragment().is_some()
                {
                    bail!("webhook URL must be an http(s) URL without a fragment");
                }
                let secret_path = expand_home(secret_path);
                let metadata = std::fs::metadata(&secret_path)
                    .map_err(|_| anyhow::anyhow!("webhook secret file is unavailable"))?;
                if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SECRET_BYTES {
                    bail!("webhook secret file has an invalid size");
                }
                let mut secret = std::fs::read(&secret_path)
                    .map_err(|_| anyhow::anyhow!("webhook secret file is unavailable"))?;
                while matches!(secret.last(), Some(b'\n' | b'\r')) {
                    secret.pop();
                }
                if secret.len() < 32 {
                    bail!("webhook secret must contain at least 32 bytes");
                }
                Ok(Some(Self { endpoint, secret }))
            }
            _ => bail!("webhook URL and secret file must be configured together"),
        }
    }

    pub fn sign(&self, timestamp: &str, body: &[u8]) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts keys of every size");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        let signature = mac.finalize().into_bytes();
        let mut encoded = String::with_capacity(64);
        for byte in signature {
            use std::fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        format!("sha256={encoded}")
    }

    pub fn post(&self, timestamp: &str, body: &[u8]) -> Result<()> {
        let signature = self.sign(timestamp, body);
        let agent = ureq::AgentBuilder::new()
            .timeout(REQUEST_TIMEOUT)
            .redirects(0)
            .build();
        let response = agent
            .post(self.endpoint.as_str())
            .set("Content-Type", "application/json")
            .set("X-Prismattyc-Timestamp", timestamp)
            .set("X-Prismattyc-Signature", &signature)
            .send_bytes(body);
        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) if (200..300).contains(&code) => Ok(()),
            Err(_) => bail!("webhook delivery failed"),
        }
    }
}

fn expand_home(path: &std::path::Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if path == std::path::Path::new("~") || raw.starts_with("~/") {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        if let Some(home) = home {
            return PathBuf::from(home).join(raw.strip_prefix("~/").unwrap_or(""));
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Serialize)]
pub struct WebhookPayload<'a> {
    pub seat: &'a str,
    pub state: &'a str,
    pub reason: Option<&'a str>,
    pub pane_id: Option<&'a str>,
    pub space: Option<&'a str>,
    pub timestamp: &'a str,
}

pub fn encode_payload(payload: &WebhookPayload<'_>) -> Result<Vec<u8>> {
    serde_json::to_vec(payload).context("cannot encode webhook payload")
}

struct Delivery {
    body: Vec<u8>,
    attempt: u8,
    retry_at: Instant,
}

#[derive(Default)]
struct SessionContext {
    pane_id: Option<String>,
    space: Option<String>,
}

#[derive(Default)]
struct Debouncer {
    by_seat: HashMap<String, Debounced>,
}

struct Debounced {
    due: Instant,
    body: Vec<u8>,
    terminal: bool,
    after_terminal: Option<Vec<u8>>,
}

impl Debouncer {
    fn record(&mut self, now: Instant, seat: String, body: Vec<u8>, terminal: bool, runtime: bool) {
        if !self.by_seat.contains_key(&seat) && self.by_seat.len() == MAX_QUEUE {
            if let Some(oldest) = self
                .by_seat
                .iter()
                .min_by_key(|(_, item)| item.due)
                .map(|(seat, _)| seat.clone())
            {
                self.by_seat.remove(&oldest);
            }
        }
        if let Some(item) = self.by_seat.get_mut(&seat) {
            if item.terminal && runtime && !terminal {
                item.after_terminal = Some(body);
                return;
            }
        }
        self.by_seat.insert(
            seat,
            Debounced {
                due: now + DEBOUNCE,
                body,
                terminal,
                after_terminal: None,
            },
        );
    }

    fn take_ready(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let ready = self
            .by_seat
            .iter()
            .filter(|(_, item)| item.due <= now)
            .map(|(seat, _)| seat.clone())
            .collect::<Vec<_>>();
        ready
            .into_iter()
            .filter_map(|seat| {
                self.by_seat.remove(&seat).map(|item| {
                    if let Some(body) = item.after_terminal {
                        self.by_seat.insert(
                            seat,
                            Debounced {
                                due: now + DEBOUNCE,
                                body,
                                terminal: false,
                                after_terminal: None,
                            },
                        );
                    }
                    item.body
                })
            })
            .collect()
    }
}

fn retry_delay(attempt: u8) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(6)).min(30_000))
}

fn enqueue(queue: &mut VecDeque<Delivery>, body: Vec<u8>, now: Instant) {
    if queue.len() == MAX_QUEUE {
        queue.pop_front();
    }
    queue.push_back(Delivery {
        body,
        attempt: 0,
        retry_at: now,
    });
}

/// Polls the daemon on an independent thread and delivers signed notifications.
/// Network waits and retries never run on pmuxd's control thread.
pub struct Worker {
    stops: Vec<mpsc::Sender<()>>,
    threads: Vec<JoinHandle<()>>,
}

impl Worker {
    pub fn start(socket: PathBuf, config_path: PathBuf) -> Result<Self> {
        let (webhook_stop, webhook_stopped) = mpsc::channel();
        let webhook_thread = std::thread::Builder::new()
            .name("pmux-activity-webhook".into())
            .spawn({
                let socket = socket.clone();
                let config_path = config_path.clone();
                move || run_worker(&socket, &config_path, webhook_stopped)
            })
            .context("start activity webhook worker")?;
        let (projection_stop, projection_stopped) = mpsc::channel();
        let projection_thread = match std::thread::Builder::new()
            .name("pmux-activity-sessions".into())
            .spawn(move || run_session_projection(&socket, projection_stopped))
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = webhook_stop.send(());
                let _ = webhook_thread.join();
                return Err(error).context("start activity session projection");
            }
        };
        Ok(Self {
            stops: vec![webhook_stop, projection_stop],
            threads: vec![webhook_thread, projection_thread],
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for stop in &self.stops {
            let _ = stop.send(());
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn load_config(path: &Path) -> Option<WebhookConfig> {
    let section = crate::config::load_mux_section(path).ok()?;
    WebhookConfig::from_mux(&section).ok().flatten()
}

fn run_worker(socket: &Path, config_path: &Path, stopped: mpsc::Receiver<()>) {
    let mut cursor: Option<ActivityCursor> = None;
    let mut active = false;
    let mut pending = Debouncer::default();
    let mut queue = VecDeque::<Delivery>::new();
    let mut ownership = WebhookOwnership::default();
    let mut session_context = HashMap::<String, SessionContext>::new();
    let mut next_snapshot = Instant::now();
    let mut request_id = 1u64;
    loop {
        if stopped.recv_timeout(POLL_INTERVAL).is_ok() {
            return;
        }
        let Some(config) = load_config(config_path) else {
            pending.by_seat.clear();
            queue.clear();
            ownership = WebhookOwnership::default();
            cursor = None;
            active = false;
            continue;
        };
        if !active {
            let mut caught_up = false;
            for _ in 0..2 {
                let request = ControlRequest::ReadActivity {
                    version: PROTOCOL_VERSION,
                    request_id,
                    cursor: cursor.clone(),
                    limit: 256,
                };
                request_id = request_id.wrapping_add(1).max(1);
                let Ok(ControlResponseData::Activity { page }) =
                    crate::activity::exchange(socket, request)
                else {
                    break;
                };
                if page.reset.is_some() {
                    ownership = WebhookOwnership::default();
                }
                for record in &page.records {
                    let _ = ownership.observe(&record.event);
                }
                cursor = Some(page.cursor);
                if !page.has_more {
                    caught_up = true;
                    break;
                }
            }
            if caught_up {
                active = true;
            }
            continue;
        }
        if Instant::now() >= next_snapshot {
            if let Some(snapshot) = read_snapshot(socket, request_id) {
                request_id = request_id.wrapping_add(1).max(1);
                session_context = snapshot_context(snapshot);
            }
            next_snapshot = Instant::now() + SNAPSHOT_INTERVAL;
        }
        let request = ControlRequest::ReadActivity {
            version: PROTOCOL_VERSION,
            request_id,
            cursor: cursor.clone(),
            limit: 256,
        };
        request_id = request_id.wrapping_add(1).max(1);
        if let Ok(ControlResponseData::Activity { page }) =
            crate::activity::exchange(socket, request)
        {
            if page.reset.is_some() {
                ownership = WebhookOwnership::default();
            }
            for record in &page.records {
                if !ownership.observe(&record.event) {
                    continue;
                }
                if let Some((state, reason)) = state_for(record.event.kind) {
                    let timestamp = record.received_at_ms.to_string();
                    let context = session_context.get(record.event.seat.as_str());
                    let payload = WebhookPayload {
                        seat: record.event.seat.as_str(),
                        state,
                        reason: Some(reason),
                        pane_id: context.and_then(|context| context.pane_id.as_deref()),
                        space: context.and_then(|context| context.space.as_deref()),
                        timestamp: &timestamp,
                    };
                    if let Ok(body) = encode_payload(&payload) {
                        let terminal = match record.event.source {
                            ActivitySource::Hooks => matches!(
                                record.event.kind,
                                ActivityKind::SessionFinished | ActivityKind::Stopped
                            ),
                            ActivitySource::Runtime => record.event.kind == ActivityKind::Stopped,
                            ActivitySource::Notify => false,
                        };
                        pending.record(
                            Instant::now(),
                            record.event.seat.to_string(),
                            body,
                            terminal,
                            record.event.source == ActivitySource::Runtime,
                        );
                    }
                }
            }
            cursor = Some(page.cursor);
        }
        let now = Instant::now();
        for body in pending.take_ready(now) {
            enqueue(&mut queue, body, now);
        }
        if let Some(index) = queue.iter().position(|delivery| delivery.retry_at <= now) {
            let mut delivery = queue.remove(index).expect("ready position exists");
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs().to_string())
                .unwrap_or_else(|_| "0".into());
            if config.post(&timestamp, &delivery.body).is_err() {
                delivery.attempt += 1;
                if delivery.attempt < MAX_ATTEMPTS {
                    let delay = retry_delay(delivery.attempt);
                    delivery.retry_at = Instant::now() + delay;
                    queue.push_back(delivery);
                }
            }
        }
    }
}

#[derive(Default)]
struct WebhookOwnership {
    hooked_runs: HashMap<String, HashSet<String>>,
    runtime_runs: HashMap<String, String>,
}

impl WebhookOwnership {
    fn observe(&mut self, event: &ActivityInput) -> bool {
        let seat = event.seat.as_str();
        match event.source {
            ActivitySource::Hooks => {
                let runs = self.hooked_runs.entry(seat.to_owned()).or_default();
                if matches!(
                    event.kind,
                    ActivityKind::SessionFinished | ActivityKind::Stopped
                ) {
                    runs.remove(&event.run_id);
                    if runs.is_empty() {
                        self.hooked_runs.remove(seat);
                    }
                } else {
                    if runs.len() >= MAX_HOOKED_RUNS_PER_SEAT && !runs.contains(&event.run_id) {
                        if let Some(evict) = runs.iter().next().cloned() {
                            runs.remove(&evict);
                        }
                    }
                    runs.insert(event.run_id.clone());
                    if self.hooked_runs.len() > MAX_HOOKED_SEATS {
                        if let Some(evict) = self
                            .hooked_runs
                            .keys()
                            .find(|key| key.as_str() != seat)
                            .cloned()
                        {
                            self.hooked_runs.remove(&evict);
                        }
                    }
                }
                true
            }
            ActivitySource::Runtime => {
                let prior_run = self
                    .runtime_runs
                    .insert(seat.to_owned(), event.run_id.clone());
                if self.runtime_runs.len() > MAX_HOOKED_SEATS {
                    if let Some(evict) = self
                        .runtime_runs
                        .keys()
                        .find(|key| key.as_str() != seat)
                        .cloned()
                    {
                        self.runtime_runs.remove(&evict);
                    }
                }
                if event.kind == ActivityKind::Stopped
                    || prior_run.is_some_and(|prior| runtime_run_changed(&prior, &event.run_id))
                {
                    self.hooked_runs.remove(seat);
                }
                event.kind == ActivityKind::Stopped || !self.hooked_runs.contains_key(seat)
            }
            ActivitySource::Notify => true,
        }
    }
}

fn runtime_run_changed(prior: &str, current: &str) -> bool {
    fn parts(run_id: &str) -> Option<(&str, &str, Option<&str>)> {
        let mut fields = run_id.split('-');
        if fields.next()? != "session" {
            return None;
        }
        let session_id = fields.next()?;
        let process_key = fields.next()?;
        let harness_key = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        Some((
            session_id,
            process_key,
            (harness_key != "none").then_some(harness_key),
        ))
    }

    match (parts(prior), parts(current)) {
        (
            Some((prior_session, prior_process, prior_harness)),
            Some((current_session, current_process, current_harness)),
        ) => {
            prior_session != current_session
                || prior_process != current_process
                || match (prior_harness, current_harness) {
                    (Some(prior), Some(current)) => prior != current,
                    (Some(_), None) => true,
                    (None, Some(_)) | (None, None) => false,
                }
        }
        _ => prior != current,
    }
}

fn read_snapshot(socket: &Path, request_id: u64) -> Option<crate::control::Snapshot> {
    match crate::activity::exchange(
        socket,
        ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        },
    ) {
        Ok(ControlResponseData::Snapshot { snapshot }) => Some(snapshot),
        _ => None,
    }
}

fn snapshot_context(snapshot: crate::control::Snapshot) -> HashMap<String, SessionContext> {
    let mut context = HashMap::new();
    for session in snapshot.sessions {
        let Some(seat) = session.agent_id else {
            continue;
        };
        if AgentId::new(seat.clone()).is_err() {
            continue;
        }
        let panes = session
            .windows
            .iter()
            .flat_map(|window| window.panes.iter())
            .collect::<Vec<_>>();
        let pane = panes
            .iter()
            .copied()
            .max_by_key(|pane| pane.ledger.last_output_at_ms)
            .or_else(|| panes.first().copied());
        context.insert(
            seat,
            SessionContext {
                pane_id: pane.map(|pane| pane.id.to_string()),
                space: session.space_id,
            },
        );
    }
    context
}

#[derive(Clone)]
struct ProjectedSession {
    session_id: u64,
    run_id: String,
    state: ActivityKind,
    harness_pids: HashMap<u64, u32>,
}

fn run_session_projection(socket: &Path, stopped: mpsc::Receiver<()>) {
    let mut cursor: Option<ActivityCursor> = None;
    let mut request_id = 1u64;
    let mut event_id = 1u64;
    let mut force_runtime = HashSet::<String>::new();
    let mut projected = HashMap::<String, ProjectedSession>::new();
    loop {
        if stopped.recv_timeout(SNAPSHOT_INTERVAL).is_ok() {
            return;
        }
        let mut feed_available = false;
        for _ in 0..2 {
            let request = ControlRequest::ReadActivity {
                version: PROTOCOL_VERSION,
                request_id,
                cursor: cursor.clone(),
                limit: 256,
            };
            request_id = request_id.wrapping_add(1).max(1);
            let Ok(ControlResponseData::Activity { page }) =
                crate::activity::exchange(socket, request)
            else {
                break;
            };
            feed_available = true;
            if page.reset.is_some() {
                projected.clear();
                force_runtime.clear();
            }
            for record in &page.records {
                if record.event.source == ActivitySource::Hooks
                    && matches!(
                        record.event.kind,
                        ActivityKind::SessionFinished | ActivityKind::Stopped
                    )
                {
                    force_runtime.insert(record.event.seat.to_string());
                }
            }
            let has_more = page.has_more;
            cursor = Some(page.cursor);
            if !has_more {
                break;
            }
        }
        if !feed_available {
            continue;
        }
        let Some(snapshot) = read_snapshot(socket, request_id) else {
            request_id = request_id.wrapping_add(1).max(1);
            continue;
        };
        request_id = request_id.wrapping_add(1).max(1);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let mut current = std::collections::HashSet::new();
        for session in snapshot.sessions {
            let Some(seat) = session.agent_id.as_deref() else {
                continue;
            };
            let Ok(seat_id) = AgentId::new(seat.to_owned()) else {
                continue;
            };
            let (run_id, harness_pids) = snapshot_run_identity(&session, projected.get(seat));
            let state = snapshot_state(&session, now_ms);
            current.insert(seat.to_owned());
            let changed = projected
                .get(seat)
                .is_none_or(|prior| prior.run_id != run_id || prior.state != state)
                || force_runtime.contains(seat);
            let published = if changed {
                publish_runtime(
                    socket,
                    &mut request_id,
                    &mut event_id,
                    seat_id,
                    run_id.clone(),
                    state,
                )
            } else {
                true
            };
            if published {
                force_runtime.remove(seat);
                projected.insert(
                    seat.to_owned(),
                    ProjectedSession {
                        session_id: session.id,
                        run_id,
                        state,
                        harness_pids,
                    },
                );
            }
        }
        let absent = projected
            .keys()
            .filter(|seat| !current.contains(*seat))
            .cloned()
            .collect::<Vec<_>>();
        for seat in absent {
            let Some(prior) = projected.get(&seat).cloned() else {
                continue;
            };
            if prior.state != ActivityKind::Stopped {
                let Ok(seat_id) = AgentId::new(seat.clone()) else {
                    projected.remove(&seat);
                    continue;
                };
                if !publish_runtime(
                    socket,
                    &mut request_id,
                    &mut event_id,
                    seat_id,
                    prior.run_id,
                    ActivityKind::Stopped,
                ) {
                    continue;
                }
            }
            projected.remove(&seat);
            force_runtime.remove(&seat);
        }
        force_runtime.retain(|seat| current.contains(seat));
    }
}

fn snapshot_run_identity(
    session: &crate::control::SessionSnapshot,
    previous: Option<&ProjectedSession>,
) -> (String, HashMap<u64, u32>) {
    let previous = previous.filter(|previous| previous.session_id == session.id);
    let mut harness_pids = HashMap::new();
    let processes = session
        .windows
        .iter()
        .flat_map(|window| window.panes.iter())
        .map(|pane| (pane, pane.child_pid))
        .map(|(pane, child_pid)| {
            let foreground_pid = child_pid.and_then(crate::procinfo::live_foreground_pid);
            let recognized = child_pid.is_some_and(|root| {
                crate::procinfo::foreground_agent(root) != crate::InjectAgent::Unknown
            });
            let detected = recognized.then_some(foreground_pid).flatten();
            let retained = previous
                .and_then(|previous| previous.harness_pids.get(&pane.id).copied())
                .filter(|prior| {
                    foreground_pid.is_some_and(|foreground| {
                        crate::procinfo::is_process_descendant(*prior, foreground)
                    })
                });
            let harness_pid = detected.or(retained);
            if let Some(pid) = harness_pid {
                harness_pids.insert(pane.id, pid);
            }
            format!("{}:{child_pid:?}", pane.id)
        })
        .collect::<Vec<_>>()
        .join(";");
    let digest = Sha256::digest(processes.as_bytes());
    let process_key = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut harnesses = harness_pids
        .iter()
        .map(|(pane, pid)| format!("{pane}:{pid}"))
        .collect::<Vec<_>>();
    harnesses.sort_unstable();
    let harnesses = harnesses.join(";");
    let harness_digest = Sha256::digest(harnesses.as_bytes());
    let harness_key = if harnesses.is_empty() {
        "none".to_owned()
    } else {
        harness_digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    (
        format!("session-{}-{process_key}-{harness_key}", session.id),
        harness_pids,
    )
}

fn snapshot_state(session: &crate::control::SessionSnapshot, now_ms: u64) -> ActivityKind {
    let panes = session
        .windows
        .iter()
        .flat_map(|window| window.panes.iter())
        .collect::<Vec<_>>();
    if panes.is_empty() || panes.iter().all(|pane| pane.child_pid.is_none()) {
        return ActivityKind::Stopped;
    }
    if panes.iter().any(|pane| {
        pane.child_pid.is_some()
            && pane
                .ledger
                .last_output_at_ms
                .is_some_and(|at| now_ms.saturating_sub(at) < 60_000)
    }) {
        ActivityKind::Working
    } else {
        ActivityKind::Waiting
    }
}

fn publish_runtime(
    socket: &Path,
    next_request_id: &mut u64,
    event_id: &mut u64,
    seat: AgentId,
    run_id: String,
    kind: ActivityKind,
) -> bool {
    let request_id = *next_request_id;
    *next_request_id = (*next_request_id).wrapping_add(1).max(1);
    let request = ControlRequest::PublishActivity {
        version: PROTOCOL_VERSION,
        request_id,
        event: ActivityInput {
            schema_version: 1,
            seat,
            run_id,
            event_id: format!("runtime-{}", *event_id),
            source: ActivitySource::Runtime,
            kind,
        },
    };
    *event_id = (*event_id).wrapping_add(1).max(1);
    matches!(
        crate::activity::exchange(socket, request),
        Ok(ControlResponseData::ActivityPublished { .. })
    )
}

fn state_for(kind: ActivityKind) -> Option<(&'static str, &'static str)> {
    use ActivityKind::*;
    match kind {
        Waiting => Some(("waiting", "waiting")),
        MailReceived => Some(("needs-you", "mail received")),
        ToolFailed | CommandFailed | EditFailed | TestFailed => Some(("failed", "activity failed")),
        Stopped | SessionFinished | TurnFinished => Some(("stopped", "activity stopped")),
        _ => Some(("working", "activity updated")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MuxSection;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn signing_uses_timestamp_dot_body_hmac_sha256() {
        let mut section = MuxSection::default();
        let file = std::env::temp_dir().join(format!("pmux-webhook-secret-{}", std::process::id()));
        std::fs::write(&file, [b"test-key-".as_slice(), &[b'x'; 32]].concat()).unwrap();
        section.activity_webhook_url = Some("https://example.invalid/hook?token=private".into());
        section.activity_webhook_secret_file = Some(file.clone());
        let config = WebhookConfig::from_mux(&section).unwrap().unwrap();
        assert_eq!(
            config.sign("1700000000", br#"{"seat":"worker-a"}"#),
            "sha256=ee81d2907720eb47429007c3d8f41b0741a7e24e365867ae143874d34c7aea59"
        );
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn webhook_configuration_rejects_inline_and_incomplete_secrets() {
        let mut section = MuxSection::default();
        section.activity_webhook_secret = Some("private".into());
        let error = WebhookConfig::from_mux(&section).err().unwrap();
        assert_eq!(
            error.to_string(),
            "inline webhook secrets are not supported; configure a secret file"
        );
        section.activity_webhook_secret = None;
        section.activity_webhook_url = Some("https://example.invalid/hook?token=private".into());
        assert!(WebhookConfig::from_mux(&section).is_err());
    }

    #[test]
    fn bad_secret_files_fail_without_echoing_the_path_or_contents() {
        let file =
            std::env::temp_dir().join(format!("pmux-webhook-bad-key-{}", std::process::id()));
        std::fs::write(&file, b"private-short-key").unwrap();
        let section = MuxSection {
            activity_webhook_url: Some("https://example.invalid/hook?token=private".into()),
            activity_webhook_secret_file: Some(file.clone()),
            ..MuxSection::default()
        };
        let error = WebhookConfig::from_mux(&section).err().unwrap().to_string();
        assert_eq!(error, "webhook secret must contain at least 32 bytes");
        assert!(!error.contains(&file.to_string_lossy().to_string()));
        assert!(!error.contains("private"));
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn fake_receiver_observes_body_and_verifiable_signature_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                received.extend_from_slice(&buffer[..count]);
                if let Some(headers_end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers =
                        String::from_utf8_lossy(&received[..headers_end]).to_ascii_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if received.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            received
        });
        let config = WebhookConfig {
            endpoint: Url::parse(&format!("http://{address}/hook?private=secret")).unwrap(),
            secret: b"01234567890123456789012345678901".to_vec(),
        };
        let body = br#"{"seat":"worker-a","state":"working"}"#;
        config.post("1700000000", body).unwrap();
        let request = String::from_utf8_lossy(&receiver.join().unwrap()).to_ascii_lowercase();
        assert!(request.contains("x-prismattyc-timestamp: 1700000000"));
        assert!(request.contains(&format!(
            "x-prismattyc-signature: {}",
            config.sign("1700000000", body)
        )));
        assert!(request.contains(std::str::from_utf8(body).unwrap()));
    }

    #[test]
    fn debounce_coalesces_each_seat_until_the_quiet_period_ends() {
        let now = Instant::now();
        let mut debounce = Debouncer::default();
        debounce.record(now, "seat-a".into(), b"first".to_vec(), false, false);
        debounce.record(
            now + Duration::from_millis(500),
            "seat-a".into(),
            b"latest".to_vec(),
            false,
            false,
        );
        assert!(debounce
            .take_ready(now + Duration::from_millis(700))
            .is_empty());
        assert_eq!(
            debounce.take_ready(now + Duration::from_millis(1_300)),
            vec![b"latest".to_vec()]
        );
    }

    #[test]
    fn terminal_hook_survives_runtime_coalescing() {
        let now = Instant::now();
        let mut debounce = Debouncer::default();
        debounce.record(now, "seat-a".into(), b"stopped".to_vec(), true, false);
        debounce.record(
            now + Duration::from_millis(500),
            "seat-a".into(),
            b"waiting".to_vec(),
            false,
            true,
        );
        assert_eq!(
            debounce.take_ready(now + Duration::from_millis(800)),
            vec![b"stopped".to_vec()]
        );
        assert!(debounce
            .take_ready(now + Duration::from_millis(1_500))
            .is_empty());
        assert_eq!(
            debounce.take_ready(now + Duration::from_millis(1_600)),
            vec![b"waiting".to_vec()]
        );
    }

    #[test]
    fn hook_run_owns_webhook_status_until_it_finishes_or_runtime_stops() {
        fn observe(
            ownership: &mut WebhookOwnership,
            source: ActivitySource,
            run_id: &str,
            kind: ActivityKind,
        ) -> bool {
            ownership.observe(&ActivityInput {
                schema_version: 1,
                seat: AgentId::new("worker-a").unwrap(),
                run_id: run_id.into(),
                event_id: "event-1".into(),
                source,
                kind,
            })
        }

        let mut ownership = WebhookOwnership::default();
        assert!(observe(
            &mut ownership,
            ActivitySource::Hooks,
            "run-a",
            ActivityKind::CommandFailed
        ));
        assert!(!observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run",
            ActivityKind::Working
        ));
        assert!(observe(
            &mut ownership,
            ActivitySource::Hooks,
            "run-a",
            ActivityKind::SessionFinished
        ));
        assert!(observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run",
            ActivityKind::Waiting
        ));

        assert!(observe(
            &mut ownership,
            ActivitySource::Hooks,
            "run-b",
            ActivityKind::TurnStarted
        ));
        assert!(!observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run",
            ActivityKind::Waiting
        ));
        assert!(!observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run",
            ActivityKind::Working
        ));
        assert!(observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run-new",
            ActivityKind::Working
        ));

        assert!(observe(
            &mut ownership,
            ActivitySource::Hooks,
            "run-c",
            ActivityKind::TurnStarted
        ));
        assert!(observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run-new",
            ActivityKind::Stopped
        ));
        assert!(observe(
            &mut ownership,
            ActivitySource::Runtime,
            "pane-run-new",
            ActivityKind::Waiting
        ));

        let mut identity_ownership = WebhookOwnership::default();
        assert!(observe(
            &mut identity_ownership,
            ActivitySource::Hooks,
            "run-d",
            ActivityKind::TurnStarted
        ));
        assert!(!observe(
            &mut identity_ownership,
            ActivitySource::Runtime,
            "session-7-process-none",
            ActivityKind::Working
        ));
        assert!(!observe(
            &mut identity_ownership,
            ActivitySource::Runtime,
            "session-7-process-harness",
            ActivityKind::Working
        ));
        assert!(observe(
            &mut identity_ownership,
            ActivitySource::Runtime,
            "session-7-process-newharness",
            ActivityKind::Working
        ));
    }

    #[test]
    fn runtime_identity_enrichment_preserves_hook_ownership() {
        assert!(!runtime_run_changed(
            "session-7-process-none",
            "session-7-process-harness"
        ));
        assert!(runtime_run_changed(
            "session-7-process-harness",
            "session-7-process-none"
        ));
        assert!(runtime_run_changed(
            "session-7-process-harness-a",
            "session-7-process-harness-b"
        ));
        assert!(runtime_run_changed(
            "session-7-process-none",
            "session-8-process-none"
        ));
    }

    #[test]
    fn retry_backoff_grows_and_delivery_queue_stays_bounded() {
        assert_eq!(retry_delay(1), Duration::from_millis(1_000));
        assert_eq!(retry_delay(4), Duration::from_millis(8_000));
        assert_eq!(retry_delay(20), Duration::from_millis(30_000));
        let now = Instant::now();
        let mut queue = VecDeque::new();
        for index in 0..=MAX_QUEUE {
            enqueue(&mut queue, vec![index as u8], now);
        }
        assert_eq!(queue.len(), MAX_QUEUE);
        assert_eq!(queue.front().unwrap().body, vec![1]);
    }

    #[test]
    fn receiver_down_errors_are_generic_and_do_not_echo_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = Url::parse(&format!("http://{address}/hook?secret=private")).unwrap();
        let config = WebhookConfig {
            endpoint,
            secret: b"01234567890123456789012345678901".to_vec(),
        };
        let error = config.post("1700000000", b"{}").unwrap_err().to_string();
        assert_eq!(error, "webhook delivery failed");
        assert!(!error.contains("private"));
    }
}
