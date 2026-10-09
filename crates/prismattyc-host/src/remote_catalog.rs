//! Background SSH catalog requests for remote Spaces (issue #24).
//!
//! Contract: `docs/design/remote-spaces-ssh.md`. One bounded, non-PTY
//! `ssh … pmux space catalog` runs per explicit connect or refresh; nothing
//! polls a disconnected destination. Results reach the event loop through
//! [`CatalogFetcher::poll`], and a response whose generation is no longer
//! current is dropped. Authentication is key or agent only: `BatchMode=yes`
//! turns every prompt into a visible [`FetchError`].

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use prismattyc_mux::remote_catalog::{
    parse_catalog, Catalog, DestinationId, RemoteSessionId, RemoteSpaceId, SshDestination,
    MAX_CATALOG_BYTES,
};

/// Fixed remote argv. Request data never becomes remote shell text.
const REMOTE_COMMAND: [&str; 3] = ["pmux", "space", "catalog"];
const CONNECT_TIMEOUT_SECS: u32 = 10;
/// Whole-request budget, including connect and the remote command.
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);
const STDERR_LIMIT: u64 = 4096;
const WAIT_STEP: Duration = Duration::from_millis(10);

/// Builds the child process for one destination. Tests substitute a script.
pub type Launcher = Arc<dyn Fn(&SshDestination) -> Command + Send + Sync>;
/// Called from a worker thread after it finishes, so the host can wake its
/// event loop and call [`CatalogFetcher::poll`].
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -- ALIAS pmux space catalog`.
///
/// System SSH config, agent and host-key checking apply unchanged. The alias
/// is its own argument after `--` and is validated not to start with `-`.
pub fn ssh_command(destination: &SshDestination) -> Command {
    let mut command = prismattyc_mux::platform::hidden_command("ssh");
    command
        .arg("-T")
        .args(["-o", "BatchMode=yes"])
        .arg("-o")
        .arg(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"))
        .arg("--")
        .arg(destination.ssh_alias.as_str())
        .args(REMOTE_COMMAND);
    command
}

/// Program and argv for a one-step remote attach in a PTY tab:
/// `ssh -t -o BatchMode=yes -o ConnectTimeout=10 -- ALIAS pmux attach
/// --session-id N --space-id HEX`. Only typed ids reach the remote command;
/// the remote refuses a session its Space no longer owns and never starts
/// a daemon.
pub fn attach_command(
    destination: &SshDestination,
    session: RemoteSessionId,
    space: &RemoteSpaceId,
) -> (String, Vec<String>) {
    let args = vec![
        "-t".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "--".into(),
        destination.ssh_alias.as_str().into(),
        "pmux".into(),
        "attach".into(),
        "--session-id".into(),
        session.0.to_string(),
        "--space-id".into(),
        space.as_str().into(),
    ];
    ("ssh".into(), args)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogState {
    Disconnected,
    Loading { generation: u64 },
    Ready { generation: u64, catalog: Catalog },
    Failed { generation: u64, error: FetchError },
}

/// Why a catalog request produced no catalog. Display text is user-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The local `ssh` binary could not start.
    SshUnavailable(String),
    /// No usable key or agent identity (`BatchMode=yes` refuses prompts).
    AuthenticationRequired,
    /// Unknown or changed host key. Never accepted automatically.
    HostKeyUnverified,
    /// DNS, refused connection, or another SSH transport failure.
    Unreachable(String),
    TimedOut,
    /// `pmux` is not on the remote non-interactive `PATH`.
    RemoteCliMissing,
    /// The remote `pmuxd` is not running. It is never started from here.
    DaemonNotRunning,
    Oversized,
    InvalidCatalog(String),
    RemoteFailed(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SshUnavailable(detail) => write!(f, "cannot run ssh: {detail}"),
            Self::AuthenticationRequired => f.write_str(
                "SSH authentication failed; load a key into ssh-agent or configure IdentityFile",
            ),
            Self::HostKeyUnverified => f.write_str(
                "SSH host key is unknown or changed; verify it with ssh in a terminal first",
            ),
            Self::Unreachable(detail) => write!(f, "cannot reach host: {detail}"),
            Self::TimedOut => f.write_str("remote catalog request timed out"),
            Self::RemoteCliMissing => f.write_str("pmux is not installed on the remote PATH"),
            Self::DaemonNotRunning => f.write_str("pmuxd is not running on the remote host"),
            Self::Oversized => write!(f, "remote catalog exceeds {MAX_CATALOG_BYTES} bytes"),
            Self::InvalidCatalog(detail) => write!(f, "invalid remote catalog: {detail}"),
            Self::RemoteFailed(detail) => write!(f, "remote catalog failed: {detail}"),
        }
    }
}

type Outcome = Result<Catalog, FetchError>;

struct Pending {
    generation: u64,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Outcome>,
    worker: JoinHandle<()>,
}

struct Entry {
    destination: SshDestination,
    state: CatalogState,
    pending: Option<Pending>,
    /// Cancelled workers still killing and reaping their `ssh`.
    retiring: Vec<JoinHandle<()>>,
    /// Generation to start once `retiring` is empty.
    queued: Option<u64>,
}

/// Per-destination catalog state with at most one live request each.
pub struct CatalogFetcher {
    launcher: Launcher,
    wake: Wake,
    deadline: Duration,
    entries: HashMap<DestinationId, Entry>,
    next_generation: u64,
}

impl CatalogFetcher {
    pub fn new(wake: Wake) -> Self {
        Self::with_launcher(Arc::new(ssh_command), wake, REQUEST_DEADLINE)
    }

    pub fn with_launcher(launcher: Launcher, wake: Wake, deadline: Duration) -> Self {
        Self {
            launcher,
            wake,
            deadline,
            entries: HashMap::new(),
            next_generation: 0,
        }
    }

    pub fn state(&self, id: &DestinationId) -> &CatalogState {
        const DISCONNECTED: &CatalogState = &CatalogState::Disconnected;
        self.entries
            .get(id)
            .map_or(DISCONNECTED, |entry| &entry.state)
    }

    /// Start a request unless one is already running for this destination.
    /// Returns false for a duplicate connect.
    pub fn connect(&mut self, destination: &SshDestination) -> bool {
        if self
            .entries
            .get(&destination.id)
            .is_some_and(|entry| entry.pending.is_some() || entry.queued.is_some())
        {
            return false;
        }
        self.restart(destination);
        true
    }

    /// Cancel any running request and start a fresh one after the old
    /// `ssh` has been reaped.
    pub fn reconnect(&mut self, destination: &SshDestination) {
        self.restart(destination);
    }

    /// Cancel any request and forget the catalog. No further network work.
    pub fn disconnect(&mut self, id: &DestinationId) {
        if let Some(entry) = self.entries.get_mut(id) {
            retire(entry);
            entry.queued = None;
            entry.state = CatalogState::Disconnected;
        }
    }

    /// Apply finished requests and start queued ones. Returns destinations
    /// whose request settled. `connect`, `reconnect` and `disconnect` change
    /// state synchronously and are not reported here.
    pub fn poll(&mut self) -> Vec<DestinationId> {
        let mut changed = Vec::new();
        let ids: Vec<DestinationId> = self.entries.keys().cloned().collect();
        for id in ids {
            let entry = self.entries.get_mut(&id).expect("listed entry");
            entry.retiring.retain(|worker| !worker.is_finished());
            if let Some(generation) = entry.queued {
                if entry.retiring.is_empty() {
                    entry.queued = None;
                    entry.pending = Some(spawn_request(
                        &self.launcher,
                        &self.wake,
                        self.deadline,
                        &entry.destination,
                        generation,
                    ));
                }
            }
            let Some(pending) = &entry.pending else {
                continue;
            };
            // Each request has its own channel, so any result is current.
            let generation = pending.generation;
            entry.state = match pending.rx.try_recv() {
                Ok(Ok(catalog)) => CatalogState::Ready {
                    generation,
                    catalog,
                },
                Ok(Err(error)) => CatalogState::Failed { generation, error },
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => CatalogState::Failed {
                    generation,
                    error: FetchError::RemoteFailed("catalog worker stopped".into()),
                },
            };
            entry.pending = None;
            changed.push(id.clone());
        }
        changed
    }

    fn restart(&mut self, destination: &SshDestination) {
        self.next_generation += 1;
        let generation = self.next_generation;
        let entry = self
            .entries
            .entry(destination.id.clone())
            .or_insert_with(|| Entry {
                destination: destination.clone(),
                state: CatalogState::Disconnected,
                pending: None,
                retiring: Vec::new(),
                queued: None,
            });
        retire(entry);
        entry.destination = destination.clone();
        entry.state = CatalogState::Loading { generation };
        entry.queued = Some(generation);
        self.poll();
    }
}

impl Drop for CatalogFetcher {
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            retire(entry);
        }
    }
}

fn retire(entry: &mut Entry) {
    if let Some(pending) = entry.pending.take() {
        pending.cancel.store(true, Ordering::Release);
        entry.retiring.push(pending.worker);
    }
}

fn spawn_request(
    launcher: &Launcher,
    wake: &Wake,
    deadline: Duration,
    destination: &SshDestination,
    generation: u64,
) -> Pending {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let command = launcher(destination);
    let worker = {
        let cancel = Arc::clone(&cancel);
        let wake = Arc::clone(wake);
        std::thread::Builder::new()
            .name(format!("remote-catalog-{}", destination.id))
            .spawn(move || {
                if let Some(result) = run_request(command, &cancel, deadline) {
                    let _ = tx.send(result);
                }
                wake();
            })
            .expect("spawn remote catalog worker")
    };
    Pending {
        generation,
        cancel,
        rx,
        worker,
    }
}

/// Run one request to completion. `None` means it was cancelled; the child
/// is killed and reaped on every path.
fn run_request(
    mut command: Command,
    cancel: &AtomicBool,
    deadline: Duration,
) -> Option<Result<Catalog, FetchError>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Some(Err(FetchError::SshUnavailable(error.to_string()))),
    };
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = read_bounded_stdout(&mut child, Arc::clone(&overflow));
    let stderr = read_bounded_stderr(&mut child);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if cancel.load(Ordering::Acquire) {
            reap(&mut child);
            return None;
        }
        if overflow.load(Ordering::Acquire) {
            reap(&mut child);
            return Some(Err(FetchError::Oversized));
        }
        if start.elapsed() >= deadline {
            reap(&mut child);
            return Some(Err(FetchError::TimedOut));
        }
        std::thread::sleep(WAIT_STEP);
    };
    let Some(status) = status else {
        reap(&mut child);
        return Some(Err(FetchError::RemoteFailed("lost the ssh process".into())));
    };
    let bytes = stdout
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if overflow.load(Ordering::Acquire) {
        return Some(Err(FetchError::Oversized));
    }
    let stderr = stderr
        .and_then(|rx| rx.recv_timeout(Duration::from_secs(1)).ok())
        .unwrap_or_default();
    Some(finish(status, &bytes, &stderr))
}

fn finish(status: ExitStatus, stdout: &[u8], stderr: &str) -> Result<Catalog, FetchError> {
    if status.success() {
        let trimmed = stdout.trim_ascii_end();
        return parse_catalog(trimmed)
            .map_err(|error| FetchError::InvalidCatalog(format!("{error:#}")));
    }
    Err(classify_failure(status.code(), stderr))
}

/// Map an `ssh` or remote `pmux` failure to something a person can act on.
fn classify_failure(code: Option<i32>, stderr: &str) -> FetchError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
    {
        FetchError::HostKeyUnverified
    } else if lower.contains("permission denied")
        || lower.contains("too many authentication failures")
    {
        FetchError::AuthenticationRequired
    } else if code == Some(127)
        || lower.contains("command not found")
        || lower.contains("pmux: not found")
    {
        FetchError::RemoteCliMissing
    } else if code == Some(255) {
        FetchError::Unreachable(last_line(stderr, "ssh exited with status 255"))
    } else if lower.contains("not running") {
        FetchError::DaemonNotRunning
    } else {
        let fallback = code.map_or_else(
            || "remote command was killed".to_string(),
            |code| format!("exit status {code}"),
        );
        FetchError::RemoteFailed(last_line(stderr, &fallback))
    }
}

fn last_line(text: &str, fallback: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Keep at most `MAX_CATALOG_BYTES`; flag and drain anything beyond it so a
/// full pipe never blocks the child.
fn read_bounded_stdout(
    child: &mut Child,
    overflow: Arc<AtomicBool>,
) -> Option<JoinHandle<Vec<u8>>> {
    let mut stdout = child.stdout.take()?;
    std::thread::Builder::new()
        .name("remote-catalog-stdout".into())
        .spawn(move || {
            let mut out = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if out.len() + n > MAX_CATALOG_BYTES => {
                        overflow.store(true, Ordering::Release);
                        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
                        break;
                    }
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                }
            }
            out
        })
        .ok()
}

/// First 4 KiB of stderr with control characters replaced; the rest drained.
fn read_bounded_stderr(child: &mut Child) -> Option<mpsc::Receiver<String>> {
    let mut stderr = child.stderr.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("remote-catalog-stderr".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.by_ref().take(STDERR_LIMIT).read_to_end(&mut bytes);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            let text: String = String::from_utf8_lossy(&bytes)
                .chars()
                .map(|ch| {
                    if ch.is_control() && ch != '\n' {
                        ' '
                    } else {
                        ch
                    }
                })
                .collect();
            let _ = tx.send(text.trim().to_string());
        })
        .ok()?;
    Some(rx)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use prismattyc_mux::remote_catalog::SshAlias;
    use std::str::FromStr;
    use std::sync::atomic::AtomicUsize;

    const CATALOG: &str = r#"{"version":1,"producer":"test","spaces":[{"id":"0123456789abcdef0123456789abcdef","name":"work","sessions":[{"id":3,"name":"work-1"}],"active_session":3}],"unavailable":[]}"#;

    fn destination(id: &str) -> SshDestination {
        SshDestination {
            id: DestinationId::from_str(id).unwrap(),
            label: id.into(),
            ssh_alias: SshAlias::from_str("devbox").unwrap(),
        }
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    fn fetcher_with(
        deadline: Duration,
        launch: impl Fn(usize) -> Command + Send + Sync + 'static,
    ) -> (CatalogFetcher, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let calls = AtomicUsize::new(0);
        let wake_count = Arc::clone(&wakes);
        let fetcher = CatalogFetcher::with_launcher(
            Arc::new(move |_| launch(calls.fetch_add(1, Ordering::SeqCst))),
            Arc::new(move || {
                wake_count.fetch_add(1, Ordering::SeqCst);
            }),
            deadline,
        );
        (fetcher, wakes)
    }

    fn fetcher(script: impl Into<String>) -> (CatalogFetcher, Arc<AtomicUsize>) {
        let script = script.into();
        fetcher_with(Duration::from_secs(10), move |_| sh(&script))
    }

    fn print_catalog() -> String {
        format!("printf '%s\\n' '{CATALOG}'")
    }

    fn settle(fetcher: &mut CatalogFetcher, id: &DestinationId) -> CatalogState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            fetcher.poll();
            let state = fetcher.state(id).clone();
            if !matches!(state, CatalogState::Loading { .. }) {
                return state;
            }
            assert!(Instant::now() < deadline, "request never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn failure(script: &str) -> FetchError {
        let (mut fetcher, _) = fetcher(script);
        let dest = destination("devbox");
        assert!(fetcher.connect(&dest));
        match settle(&mut fetcher, &dest.id) {
            CatalogState::Failed { error, .. } => error,
            other => panic!("expected failure: {other:?}"),
        }
    }

    fn pid_alive(pid: i32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn default_fetcher_does_no_work_before_connect() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let mut fetcher = CatalogFetcher::new(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(fetcher.deadline, REQUEST_DEADLINE);
        let dest = destination("devbox");
        assert!(fetcher.poll().is_empty());
        assert_eq!(fetcher.state(&dest.id), &CatalogState::Disconnected);
        fetcher.disconnect(&dest.id);
        assert!(fetcher.entries.is_empty(), "disconnect never creates work");
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ssh_command_is_batch_mode_with_alias_after_double_dash() {
        let command = ssh_command(&destination("devbox"));
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "devbox",
                "pmux",
                "space",
                "catalog"
            ]
        );
    }

    #[test]
    fn attach_command_passes_only_typed_ids_after_double_dash() {
        let space =
            RemoteSpaceId::try_from("0123456789abcdef0123456789abcdef".to_string()).unwrap();
        let (program, args) = attach_command(&destination("devbox"), RemoteSessionId(42), &space);
        assert_eq!(program, "ssh");
        assert_eq!(
            args,
            [
                "-t",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "devbox",
                "pmux",
                "attach",
                "--session-id",
                "42",
                "--space-id",
                "0123456789abcdef0123456789abcdef"
            ]
        );
    }

    #[test]
    fn ready_catalog_is_applied_and_wakes_the_loop() {
        let (mut fetcher, wakes) = fetcher(print_catalog());
        let dest = destination("devbox");
        assert_eq!(fetcher.state(&dest.id), &CatalogState::Disconnected);
        assert!(fetcher.connect(&dest));
        assert!(matches!(
            fetcher.state(&dest.id),
            CatalogState::Loading { .. }
        ));
        let CatalogState::Ready { catalog, .. } = settle(&mut fetcher, &dest.id) else {
            panic!("expected ready")
        };
        assert_eq!(catalog.spaces[0].name, "work");
        assert!(wakes.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn poll_reports_only_settled_requests() {
        let (mut fetcher, _) = fetcher(print_catalog());
        let dest = destination("devbox");
        fetcher.connect(&dest);
        let deadline = Instant::now() + Duration::from_secs(10);
        let changed = loop {
            let changed = fetcher.poll();
            if !changed.is_empty() {
                break changed;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(changed, vec![dest.id.clone()]);
        assert!(fetcher.poll().is_empty(), "no new result, no change");
    }

    #[test]
    fn failures_are_classified_for_display() {
        assert_eq!(
            failure("echo 'user@devbox: Permission denied (publickey).' >&2; exit 255"),
            FetchError::AuthenticationRequired
        );
        assert_eq!(
            failure("echo 'Host key verification failed.' >&2; exit 255"),
            FetchError::HostKeyUnverified
        );
        assert_eq!(
            failure("echo 'bash: line 1: pmux: command not found' >&2; exit 127"),
            FetchError::RemoteCliMissing
        );
        assert_eq!(
            failure("echo 'Error: not running' >&2; exit 1"),
            FetchError::DaemonNotRunning
        );
        assert_eq!(
            failure("echo 'ssh: Could not resolve hostname devbox: nodename nor servname provided' >&2; exit 255"),
            FetchError::Unreachable(
                "ssh: Could not resolve hostname devbox: nodename nor servname provided".into()
            )
        );
        assert_eq!(
            failure("echo boom >&2; exit 3"),
            FetchError::RemoteFailed("boom".into())
        );
        assert!(matches!(
            failure("echo '{}'"),
            FetchError::InvalidCatalog(_)
        ));
    }

    #[test]
    fn missing_ssh_binary_is_reported() {
        let (mut fetcher, _) = fetcher_with(Duration::from_secs(5), |_| {
            Command::new("/nonexistent/prismattyc-ssh")
        });
        let dest = destination("devbox");
        fetcher.connect(&dest);
        assert!(matches!(
            settle(&mut fetcher, &dest.id),
            CatalogState::Failed {
                error: FetchError::SshUnavailable(_),
                ..
            }
        ));
    }

    #[test]
    fn deadline_kills_a_hung_request() {
        let (mut fetcher, _) = fetcher_with(Duration::from_millis(200), |_| sh("exec sleep 30"));
        let dest = destination("devbox");
        let start = Instant::now();
        fetcher.connect(&dest);
        assert!(matches!(
            settle(&mut fetcher, &dest.id),
            CatalogState::Failed {
                error: FetchError::TimedOut,
                ..
            }
        ));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn oversized_output_is_cut_off() {
        let error = failure("while :; do printf '%1024s' x; done");
        assert_eq!(error, FetchError::Oversized);
    }

    #[test]
    fn stderr_is_scrubbed_and_bounded() {
        assert_eq!(
            failure("printf 'a\\033]0;title\\007b\\n' >&2; exit 2"),
            FetchError::RemoteFailed("a ]0;title b".into())
        );
        let FetchError::RemoteFailed(detail) =
            failure("head -c 20000 /dev/zero | tr '\\0' x >&2; exit 2")
        else {
            panic!("expected remote failure")
        };
        assert_eq!(detail.len(), STDERR_LIMIT as usize);
    }

    #[test]
    fn duplicate_connect_starts_one_request() {
        let (mut fetcher, _) = fetcher_with(Duration::from_secs(10), |_| sh("exec sleep 1"));
        let dest = destination("devbox");
        assert!(fetcher.connect(&dest));
        assert!(!fetcher.connect(&dest));
        assert!(!fetcher.connect(&dest));
        fetcher.disconnect(&dest.id);
        assert!(
            fetcher.connect(&dest),
            "a new connect after disconnect is allowed"
        );
        fetcher.disconnect(&dest.id);
    }

    #[test]
    fn reconnect_reaps_the_old_request_and_ignores_its_result() {
        let dir = std::env::temp_dir().join(format!("prism-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("first.pid");
        let first = pid_file.clone();
        let (mut fetcher, _) = fetcher_with(Duration::from_secs(10), move |call| {
            if call == 0 {
                sh(&format!(
                    "echo $$ > '{}'; sleep 2; printf '%s' '{{\"version\":1,\"producer\":\"stale\",\"spaces\":[],\"unavailable\":[]}}'",
                    first.display()
                ))
            } else {
                sh(&format!("printf '%s' '{CATALOG}'"))
            }
        });
        let dest = destination("devbox");
        fetcher.connect(&dest);
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "first request never started");
            std::thread::sleep(Duration::from_millis(5));
        };
        fetcher.reconnect(&dest);
        let CatalogState::Ready { catalog, .. } = settle(&mut fetcher, &dest.id) else {
            panic!("expected ready")
        };
        assert_eq!(catalog.producer, "test", "stale response must not win");
        assert!(!pid_alive(pid), "cancelled ssh must be killed and reaped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disconnect_discards_an_in_flight_result() {
        let (mut fetcher, _) = fetcher(format!("sleep 0.2; {}", print_catalog()));
        let dest = destination("devbox");
        fetcher.connect(&dest);
        fetcher.disconnect(&dest.id);
        std::thread::sleep(Duration::from_millis(400));
        fetcher.poll();
        assert_eq!(fetcher.state(&dest.id), &CatalogState::Disconnected);
    }

    #[test]
    fn destinations_are_independent() {
        let (mut fetcher, _) = fetcher(print_catalog());
        let a = destination("a");
        let b = destination("b");
        assert!(fetcher.connect(&a));
        assert!(
            fetcher.connect(&b),
            "another destination is not a duplicate"
        );
        assert!(matches!(
            settle(&mut fetcher, &a.id),
            CatalogState::Ready { .. }
        ));
        assert!(matches!(
            settle(&mut fetcher, &b.id),
            CatalogState::Ready { .. }
        ));
    }

    #[test]
    fn classify_prefers_host_key_over_generic_ssh_failure() {
        assert_eq!(
            classify_failure(
                Some(255),
                "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nPermission denied"
            ),
            FetchError::HostKeyUnverified
        );
        assert_eq!(
            classify_failure(None, ""),
            FetchError::RemoteFailed("remote command was killed".into())
        );
    }
}

/// The production fetcher over real SSH (issue #24). Skipped unless
/// `PRISMATTYC_SSH_TEST_TARGET`, `_KEY`, `_KNOWN_HOSTS` and `_BIN` (the
/// directory with the built pmux and pmuxd) are set; see
/// `scripts/remote-ssh-tests.sh`. Remote commands run under `env` with an
/// isolated socket, so no live daemon is reached.
#[cfg(all(test, unix))]
mod real_ssh_tests {
    use super::*;
    use prismattyc_mux::remote_catalog::SshAlias;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    struct Env {
        target: String,
        key: PathBuf,
        known_hosts: PathBuf,
        bin: PathBuf,
    }

    fn env() -> Option<Env> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        Some(Env {
            target: var("PRISMATTYC_SSH_TEST_TARGET")?
                .to_string_lossy()
                .into_owned(),
            key: var("PRISMATTYC_SSH_TEST_KEY")?.into(),
            known_hosts: var("PRISMATTYC_SSH_TEST_KNOWN_HOSTS")?.into(),
            bin: var("PRISMATTYC_SSH_TEST_BIN")?.into(),
        })
    }

    /// `ssh_command`'s exact options, plus identity/known_hosts options
    /// before `--`, and the remote `pmux` wrapped in `env` for isolation.
    fn launcher(env: &Env, key: PathBuf, known_hosts: PathBuf, socket: PathBuf) -> Launcher {
        let pmux = env.bin.join("pmux");
        let target = env.target.clone();
        Arc::new(move |dest: &SshDestination| {
            let production = ssh_command(dest);
            let args: Vec<String> = production
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let split = args.iter().position(|arg| arg == "--").unwrap();
            let mut command = Command::new("/usr/bin/ssh");
            command.args(&args[..split]);
            command.args([
                "-i".to_string(),
                key.display().to_string(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-o".into(),
                "IdentityAgent=none".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", known_hosts.display()),
                "-o".into(),
                "GlobalKnownHostsFile=/dev/null".into(),
                "-o".into(),
                "StrictHostKeyChecking=yes".into(),
                "--".into(),
                target.clone(),
                "env".into(),
                format!("PMUX_SOCKET={}", socket.display()),
                format!("XDG_DATA_HOME={}", socket.parent().unwrap().display()),
                pmux.display().to_string(),
            ]);
            // Production remote argv after the alias: `pmux space catalog`.
            command.args(&args[split + 3..]);
            command
        })
    }

    fn settle(fetcher: &mut CatalogFetcher, id: &DestinationId) -> CatalogState {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            fetcher.poll();
            let state = fetcher.state(id).clone();
            if !matches!(state, CatalogState::Loading { .. }) {
                return state;
            }
            assert!(Instant::now() < deadline, "request never settled");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn fetch(env: &Env, key: &Path, known_hosts: &Path, socket: &Path) -> CatalogState {
        let dest = SshDestination {
            id: DestinationId::from_str("loopback").unwrap(),
            label: "loopback".into(),
            ssh_alias: SshAlias::from_str("loopback").unwrap(),
        };
        let mut fetcher = CatalogFetcher::with_launcher(
            launcher(env, key.into(), known_hosts.into(), socket.into()),
            Arc::new(|| {}),
            Duration::from_secs(30),
        );
        fetcher.connect(&dest);
        settle(&mut fetcher, &dest.id)
    }

    fn keygen(path: &Path) {
        let status = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn production_fetcher_over_real_ssh() {
        let Some(env) = env() else {
            eprintln!("skipped: PRISMATTYC_SSH_TEST_TARGET/_KEY/_KNOWN_HOSTS/_BIN not set");
            return;
        };
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("prism-host-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let local = |binary: &str| {
            let mut command = Command::new(env.bin.join(binary));
            command
                .env("PMUX_SOCKET", &socket)
                .env("XDG_DATA_HOME", &dir)
                .env_remove("PRISMATTYC_PANE_ID")
                .env_remove("PMUX_PANE_LOG")
                .stdin(Stdio::null());
            command
        };

        // No daemon: the remote CLI reports it and the host maps it.
        assert!(matches!(
            fetch(&env, &env.key, &env.known_hosts, &socket),
            CatalogState::Failed {
                error: FetchError::DaemonNotRunning,
                ..
            }
        ));

        let mut daemon = local("pmuxd")
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh", "-c", "exec sleep 999"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let created = local("pmux")
            .args(["space", "create", "work", "--no-attach"])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );

        let ready = fetch(&env, &env.key, &env.known_hosts, &socket);
        let CatalogState::Ready { catalog, .. } = &ready else {
            panic!("expected ready: {ready:?}")
        };
        assert_eq!(catalog.spaces[0].name, "work");

        let stranger = dir.join("stranger");
        keygen(&stranger);
        assert!(matches!(
            fetch(&env, &stranger, &env.known_hosts, &socket),
            CatalogState::Failed {
                error: FetchError::AuthenticationRequired,
                ..
            }
        ));

        let fake = dir.join("fake_host");
        keygen(&fake);
        let fake_pub = std::fs::read_to_string(fake.with_extension("pub")).unwrap();
        let fields: Vec<&str> = fake_pub.split_whitespace().take(2).collect();
        let host = env.target.rsplit('@').next().unwrap();
        let wrong = dir.join("wrong_known_hosts");
        std::fs::write(&wrong, format!("{host} {} {}\n", fields[0], fields[1])).unwrap();
        assert!(matches!(
            fetch(&env, &env.key, &wrong, &socket),
            CatalogState::Failed {
                error: FetchError::HostKeyUnverified,
                ..
            }
        ));

        daemon.kill().expect("kill test daemon");
        daemon.wait().expect("reap test daemon");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }
}
