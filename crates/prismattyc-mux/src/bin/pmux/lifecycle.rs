//! User-facing restart coordinator. Live PTY owners are deferred by default.
use super::*;
use prismattyc_mux::component_restart as components;
use prismattyc_mux::release_update::progress::{self, Progress};
use serde_json::json;

#[derive(Default, Debug)]
struct Options {
    host: bool,
    daemon: bool,
    mcp: bool,
    plan: bool,
    stop_sessions: bool,
    worker: bool,
    consent_config: Option<PathBuf>,
    delete_consent_config: bool,
    except_pids: Vec<u32>,
    /// Report each daemon restart step as a progress line on stderr.
    progress: bool,
}
fn parse(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();
    let mut all = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => opts.host = true,
            "--daemon" | "--mux" => opts.daemon = true,
            "--mcp" => opts.mcp = true,
            "--all" => all = true,
            "--plan" => opts.plan = true,
            "--stop-sessions" => opts.stop_sessions = true,
            "--worker" => opts.worker = true,
            "--consented-config" => {
                opts.consent_config = Some(PathBuf::from(
                    args.next().context("--consented-config requires a file")?,
                ));
            }
            "--delete-consented-config" => opts.delete_consent_config = true,
            "--except-pid" => {
                let pid = args.next().context("--except-pid requires a process id")?;
                opts.except_pids
                    .push(pid.parse().context("--except-pid requires a process id")?);
            }
            "--json" => {}
            "--progress" => opts.progress = true,
            other => bail!("unknown restart option {other:?}; use pmux restart --help"),
        }
    }
    if all || (!opts.host && !opts.daemon && !opts.mcp) {
        opts.host = true;
        opts.daemon = true;
        opts.mcp = true;
    }
    anyhow::ensure!(
        !opts.stop_sessions || opts.daemon,
        "--stop-sessions requires a daemon restart"
    );
    anyhow::ensure!(
        opts.consent_config.is_none() || opts.stop_sessions,
        "--consented-config requires --stop-sessions"
    );
    anyhow::ensure!(
        !opts.delete_consent_config || opts.consent_config.is_some(),
        "--delete-consented-config requires --consented-config"
    );
    Ok(opts)
}

fn stop_idle(paths: &Paths) -> Result<()> {
    if probe_socket_liveness(&paths.socket) == SocketLiveness::Missing {
        return Ok(());
    }
    let mut client = Client::connect(&paths.socket)?;
    let ControlResponseData::ClientRegistered { client_id } =
        client.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })?
    else {
        bail!("could not register restart coordinator");
    };
    shutdown_idle(paths, &mut client, client_id)
}

fn shutdown_idle(paths: &Paths, client: &mut Client, client_id: u64) -> Result<()> {
    let reply = client
        .request(|request_id| ControlRequest::ShutdownIdle {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        })
        .context(
            "idle restart refused; older daemons require an explicit --stop-sessions restart",
        )?;
    anyhow::ensure!(
        matches!(reply, ControlResponseData::ShutdownAccepted),
        "idle shutdown rejected"
    );
    let deadline = Instant::now() + STOP_GRACE;
    while paths.socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(
        !paths.socket.exists(),
        "daemon did not finish shutdown; no signals sent"
    );
    Ok(())
}

fn running_sessions(paths: &Paths) -> Result<Vec<(u64, String)>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(25);
    loop {
        let held = prismattyc_mux::daemon_lock::DaemonLock::is_held(&paths.socket)?;
        let result = match probe_socket_liveness(&paths.socket) {
            SocketLiveness::Live => list_sessions(paths),
            SocketLiveness::Missing | SocketLiveness::Stale if !held => return Ok(Vec::new()),
            SocketLiveness::Missing | SocketLiveness::Stale => Err(anyhow::anyhow!(
                "the daemon instance lock is occupied but no control peer answers"
            )),
            SocketLiveness::Foreign => bail!("foreign control socket"),
        };
        match result {
            Ok(sessions) => return Ok(sessions),
            Err(error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !held || remaining.is_zero() {
                    let holder = if held {
                        prismattyc_mux::daemon_lock::DaemonLock::holder_description(&paths.socket)
                    } else {
                        "the daemon".into()
                    };
                    return Err(error).context(format!("Could not read running sessions from {holder}. Wait for the daemon to respond and retry. No sessions were stopped."));
                }
                std::thread::sleep(delay.min(remaining));
                delay = (delay * 2).min(Duration::from_millis(250));
            }
        }
    }
}

pub(super) fn restart(paths: &Paths, args: Vec<String>) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("pmux restart [--all|--host|--daemon|--mcp] [--except-pid PID]... [--plan] [--json] [--progress]\n\nRestart cooperative components. Default: all safe components.\nHost: restore windows and reconnect mux sessions; defer if blank terminals exist.\n--except-pid PID leaves that host alone; the caller restarts itself last.\nMCP: restart supervised adapters without replaying pending operations.\nDaemon: restart only when no sessions exist. --stop-sessions explicitly permits\nstopping every session; the restart then runs from a detached helper.\n--consented-config FILE limits an update restart to the unchanged saved session list.\nUse --plan to inspect the impact before changing anything.\n--progress reports each daemon restart step as a progress line on stderr.");
        return Ok(());
    }
    let opts = parse(&args)?;
    if opts.progress {
        progress::enable();
    }
    let sessions = running_sessions(paths)?;
    let mut hosts = components::live_components(&paths.socket, "host");
    hosts.retain(|pid| !opts.except_pids.contains(pid));
    let adapters = components::live_components(&paths.socket, "mcp");
    if opts.plan {
        println!(
            "{}",
            json!({"status":"planned","host_pids":if opts.host{hosts}else{Vec::new()},"mcp_supervisor_pids":if opts.mcp{adapters}else{Vec::new()},"daemon":opts.daemon,"sessions":sessions,"daemon_deferred":opts.daemon&&!sessions.is_empty()&&!opts.stop_sessions,"stops_sessions":opts.stop_sessions})
        );
        return Ok(());
    }
    if opts.daemon && opts.stop_sessions && !opts.worker {
        // The caller may itself live in a session being stopped. Detach the
        // entire coordinator before asking the old daemon to shut down.
        let executable = prismattyc_mux::release_update::installed_binary("pmux")
            .unwrap_or(std::env::current_exe()?);
        let log = paths.logfile.with_extension("restart.log");
        let output = prismattyc_mux::private_fs::append(&log)?;
        let mut child = Command::new(executable);
        child
            .arg("--socket")
            .arg(&paths.socket)
            .arg("restart")
            .args(&args)
            .arg("--worker")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        let process = prismattyc_mux::platform::spawn_detached(&mut child)?;
        println!(
            "{}",
            json!({"status":"scheduled","pid":process.id(),"log":log,"sessions_to_stop":sessions})
        );
        return Ok(());
    }
    let _lock = components::coordinator_lock(&paths.socket)?;
    let mut results = Vec::new();
    if opts.daemon {
        if sessions.is_empty() || opts.stop_sessions {
            // Run lifecycle verbs as children so their progress cannot corrupt JSON.
            let executable = prismattyc_mux::release_update::installed_binary("pmux")
                .unwrap_or(std::env::current_exe()?);
            // A kept consent file means the user chose to save the sessions:
            // the new daemon restores them with their layouts and scrollback.
            let mut kept = None;
            let mut live_done = false;
            let mut steps = Steps::new(&opts, live_update_binary().is_some());
            if let Some(file) = &opts.consent_config {
                let (mut client, client_id, session_ids) =
                    session_config::check_for_restart(paths, file)
                        .context("session consent check failed; no sessions were stopped")?;
                if opts.delete_consent_config {
                    steps.start("Stopping the sessions");
                } else {
                    steps.start("Saving the sessions");
                }
                let snapshot = take_snapshot(&mut client)?;
                if opts.delete_consent_config {
                    session_config::hold_spaces(
                        snapshot
                            .sessions
                            .iter()
                            .filter(|session| session_ids.contains(&session.id))
                            .filter_map(|session| session.space_id.as_deref()),
                    )?;
                    std::fs::remove_file(file)?;
                    for session_id in session_ids {
                        super::destroy_session(&mut client, session_id).with_context(|| {
                            format!("consented session {session_id} could not be stopped")
                        })?;
                    }
                    shutdown_idle(paths, &mut client, client_id)?;
                } else {
                    let view = prismattyc_mux::attach_tabs::load(
                        &prismattyc_mux::attach_tabs::layout_path_from_socket(&paths.socket),
                    );
                    let workspace =
                        prismattyc_mux::workspace::Workspace::for_update(snapshot, view).context(
                            "could not record the sessions to restore; no sessions were stopped",
                        )?;
                    let live = match live_update_binary() {
                        Some(binary) => {
                            steps.start("Handing the sessions to the new pmux server");
                            live_update(paths, &mut client, client_id, &binary)
                        }
                        None => LiveUpdate::Off,
                    };
                    drop(client);
                    match live {
                        LiveUpdate::Done { version } => {
                            results.push(json!({"component":"daemon","status":"updated_live","version":version}));
                            live_done = true;
                            kept = Some(workspace.session_names());
                        }
                        LiveUpdate::Off | LiveUpdate::Refused(_) | LiveUpdate::Lost(_) => {
                            if let LiveUpdate::Refused(reason) | LiveUpdate::Lost(reason) = &live {
                                results.push(json!({"component":"daemon","status":"live_update_skipped","detail":reason}));
                                steps.add(2);
                            }
                            let stuck =
                                prismattyc_mux::daemon_lock::DaemonLock::is_held(&paths.socket)
                                    .unwrap_or(true);
                            if !matches!(live, LiveUpdate::Lost(_)) || stuck {
                                steps.start("Stopping the old daemon");
                                // An orderly stop writes every pane's scrollback for the
                                // next daemon. Write the workspace after the old daemon
                                // exits so its last checkpoint cannot replace it.
                                stop_daemon(&executable, paths)?;
                            } else {
                                steps.start("The old daemon has stopped");
                            }
                            workspace.write(&paths.socket).context(
                                "the daemon stopped, but the sessions to restore could not be written",
                            )?;
                            kept = Some(workspace.session_names());
                        }
                    }
                }
            } else if opts.stop_sessions {
                steps.start("Stopping the old daemon");
                stop_daemon(&executable, paths)?;
            } else {
                steps.start("Stopping the old daemon");
                stop_idle(paths)?;
            }
            if !live_done {
                steps.start("Starting the new daemon");
                let mut up = Command::new(&executable);
                up.arg("--socket").arg(&paths.socket).arg("up");
                if kept.is_some() {
                    up.arg("--restore-workspace");
                }
                up.stdout(Stdio::null());
                if let Some(server) = prismattyc_mux::release_update::installed_binary("pmuxd") {
                    up.env("PMUX_SERVER", server);
                }
                anyhow::ensure!(
                    prismattyc_process::status(&mut up)?.success(),
                    "daemon up failed"
                );
                results.push(json!({"component":"daemon","status":"restarted"}));
            }
            if let Some(names) = kept {
                steps.start("Checking the restored sessions");
                results.push(restored_sessions(paths, names));
            }
            steps.start("Reconciling the official plug-ins");
            results.push(reconciled_plugins(paths));
        } else {
            results.push(json!({"component":"daemon","status":"deferred","detail":"sessions are running; use --stop-sessions only when ready to end them","sessions":sessions}));
            results.push(json!({"component":"plugins","status":"pending","detail":"bundled plug-in migration waits for the daemon restart"}));
        }
    }
    restart_cooperative(paths, &opts, hosts, adapters, &mut results)?;
    println!(
        "{}",
        json!({"status":"restart_results","components":results})
    );
    Ok(())
}

/// Reports each step of a daemon restart as it starts, as a progress line
/// with `--progress` and as text otherwise.
struct Steps {
    index: usize,
    count: usize,
}

impl Steps {
    /// The steps the daemon restart in `opts` takes, in order.
    fn new(opts: &Options, live_update: bool) -> Self {
        let count = match &opts.consent_config {
            // Save the sessions, hand them to the new server, check the
            // sessions, plug-ins.
            Some(_) if !opts.delete_consent_config && live_update => 4,
            // Save the sessions, stop, start, check the sessions, plug-ins.
            Some(_) if !opts.delete_consent_config => 5,
            // Stop the sessions (and with them the daemon), or stop the
            // daemon; start; plug-ins.
            _ => 3,
        };
        Steps { index: 0, count }
    }

    /// A live update fell back to a restart: it stops and starts the server.
    fn add(&mut self, steps: usize) {
        self.count += steps;
    }

    fn start(&mut self, item: &str) {
        self.index += 1;
        progress::emit(&Progress::Restart {
            item: item.into(),
            index: self.index,
            count: self.count,
        });
    }
}

/// How a live update went.
enum LiveUpdate {
    /// Turned off with `[mux] live_update = false`, or no new pmuxd found.
    Off,
    /// The new server runs the sessions.
    Done { version: String },
    /// The old server said no and still runs the sessions unchanged.
    Refused(String),
    /// The old server accepted but no new server came up: the sessions ended.
    Lost(String),
}

/// The pmuxd to hand the sessions to, or `None` when live updates are off.
fn live_update_binary() -> Option<PathBuf> {
    if !cfg!(unix) {
        return None;
    }
    let enabled = prismattyc_mux::load_mux_section(&prismattyc_mux::prism_config_path())
        .map(|file| file.live_update())
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    prismattyc_mux::release_update::installed_binary("pmuxd")
        .or_else(|| prismattyc_mux::executable::sibling(&std::env::current_exe().ok()?, &["pmuxd"]))
}

/// Ask the running server to hand its sessions to `binary`, then wait for the
/// new server on the same socket.
fn live_update(paths: &Paths, client: &mut Client, client_id: u64, binary: &Path) -> LiveUpdate {
    let expected = match prismattyc_mux::handoff::probe(binary) {
        Ok(reply) => reply.package_version,
        Err(error) => return LiveUpdate::Refused(error.to_string()),
    };
    let accepted = client.request(|request_id| ControlRequest::HandoffUpgrade {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        binary: binary.to_owned(),
    });
    match accepted {
        Ok(ControlResponseData::ShutdownAccepted) => {}
        Ok(other) => return LiveUpdate::Refused(format!("unexpected answer {other:?}")),
        Err(error) => return LiveUpdate::Refused(format!("{error:#}")),
    }
    let deadline = Instant::now() + LIVE_UPDATE_WAIT;
    loop {
        let running = Client::connect(&paths.socket).and_then(|mut client| {
            client.request(|request_id| ControlRequest::ServerInfo {
                version: PROTOCOL_VERSION,
                request_id,
            })
        });
        if let Ok(ControlResponseData::ServerInfo {
            package_version, ..
        }) = running
        {
            if package_version == expected {
                return LiveUpdate::Done {
                    version: package_version,
                };
            }
        }
        let held = prismattyc_mux::daemon_lock::DaemonLock::is_held(&paths.socket).unwrap_or(true);
        if !held {
            return LiveUpdate::Lost("the new pmux server did not start".into());
        }
        if Instant::now() >= deadline {
            return LiveUpdate::Lost(format!(
                "the new pmux server did not answer in {LIVE_UPDATE_WAIT:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long a live update may take before the restart gives up on it.
const LIVE_UPDATE_WAIT: Duration = Duration::from_secs(20);

/// Stop the daemon in order (its sessions end; their scrollback is written).
fn stop_daemon(executable: &Path, paths: &Paths) -> Result<()> {
    let stopped = prismattyc_process::status(
        Command::new(executable)
            .arg("--socket")
            .arg(&paths.socket)
            .arg("stop")
            .stdout(Stdio::null()),
    )?;
    anyhow::ensure!(stopped.success(), "daemon stop failed");
    Ok(())
}

/// `pmux stop --save`: record the running sessions as the workspace, stop
/// the server in order so every pane's scrollback is written, then write the
/// workspace for `pmux up --restore-workspace`. These are the steps a
/// restart that keeps its sessions takes, without the start.
pub(super) fn stop_saving(paths: &Paths) -> Result<()> {
    let _lock = components::coordinator_lock(&paths.socket)?;
    let mut client = Client::connect(&paths.socket).context("the pmux server is not running")?;
    let snapshot = take_snapshot(&mut client)?;
    let view = prismattyc_mux::attach_tabs::load(
        &prismattyc_mux::attach_tabs::layout_path_from_socket(&paths.socket),
    );
    let workspace = prismattyc_mux::workspace::Workspace::for_update(snapshot, view)
        .context("could not record the sessions to restore; nothing was stopped")?;
    let saved = workspace.session_names();
    drop(client);
    let executable = std::env::current_exe()?;
    stop_daemon(&executable, paths)?;
    // Written after the server exits so its last checkpoint cannot replace it.
    workspace
        .write(&paths.socket)
        .context("the server stopped, but the sessions to restore could not be written")?;
    println!("{}", json!({"status": "stopped", "saved": saved}));
    Ok(())
}

/// Which saved sessions the restarted daemon brought back.
fn restored_sessions(paths: &Paths, saved: Vec<String>) -> serde_json::Value {
    let running = Client::connect(&paths.socket).and_then(|mut client| take_snapshot(&mut client));
    match running {
        Ok(snapshot) => {
            let live: std::collections::HashSet<_> =
                snapshot.sessions.iter().map(|s| s.name.as_str()).collect();
            let (restored, lost): (Vec<_>, Vec<_>) = saved
                .into_iter()
                .partition(|name| live.contains(name.as_str()));
            let status = if lost.is_empty() {
                "restored"
            } else {
                "partial"
            };
            json!({"component":"sessions","status":status,"restored":restored,"lost":lost})
        }
        Err(error) => {
            json!({"component":"sessions","status":"unconfirmed","saved":saved,"detail":format!("{error:#}")})
        }
    }
}

fn reconciled_plugins(paths: &Paths) -> serde_json::Value {
    use prismattyc_mux::plugins::{Client, Reply, Request};

    let report = Client::connect(&paths.socket)
        .and_then(|mut client| client.request(Request::StartupReconciliation));
    match report {
        Ok(Reply::StartupReconciliation { changed, failures }) => {
            let status = if failures.is_empty() {
                "reconciled"
            } else {
                "failed"
            };
            json!({"component":"plugins","status":status,"changed":changed,"failures":failures})
        }
        Ok(_) => {
            json!({"component":"plugins","status":"unconfirmed","detail":"unexpected plug-in reconciliation response"})
        }
        Err(error) => {
            json!({"component":"plugins","status":"unconfirmed","detail":format!("{error:#}")})
        }
    }
}

fn restart_cooperative(
    paths: &Paths,
    opts: &Options,
    hosts: Vec<u32>,
    adapters: Vec<u32>,
    results: &mut Vec<serde_json::Value>,
) -> Result<()> {
    let mut pending = Vec::new();
    // A window that is open but has not renewed its registration (an idle
    // window from an older release) is named instead of being skipped silently.
    let idle_hosts: Vec<u32> = if opts.host {
        components::idle_components(&paths.socket, "host")
            .into_iter()
            .filter(|pid| !opts.except_pids.contains(pid))
            .collect()
    } else {
        Vec::new()
    };
    for pid in &idle_hosts {
        results.push(json!({"component":"host","status":"idle","pid":pid,"detail":"open but idle, so it was not asked to restart"}));
    }
    for (component, pids, enabled) in [("host", hosts, opts.host), ("mcp", adapters, opts.mcp)] {
        if !enabled {
            continue;
        }
        if pids.is_empty() && (component != "host" || idle_hosts.is_empty()) {
            results.push(json!({"component":component,"status":"not_registered","detail":"no cooperative component found; older hosts must be reopened manually"}));
        }
        for pid in pids {
            pending.push((
                component,
                components::request(&paths.socket, component, pid)?,
            ));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pending.is_empty() && Instant::now() < deadline {
        pending.retain(|(component, request)| {
            let path = components::response_path(&paths.socket, &request.id);
            let response = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<components::Response>(&bytes).ok());
            if let Some(response) = response.filter(|r| {
                #[cfg(windows)]
                {
                    r.id == request.id && r.pid == request.pid && r.generation == request.generation
                }
                #[cfg(unix)]
                {
                    r.id == request.id
                }
            }) {
                results.push(json!({"component":component,"response":response}));
                let _ = std::fs::remove_file(path);
                false
            } else {
                true
            }
        });
        if !pending.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for (component, request) in pending {
        results.push(json!({"component":component,"status":"pending","request_id":request.id,"receipt":components::response_path(&paths.socket,&request.id)}));
    }
    Ok(())
}

pub(super) fn versions(paths: &Paths) -> Result<()> {
    let mut installed = Vec::new();
    for binary in prismattyc_mux::release_update::BINARIES {
        let executable = prismattyc_mux::release_update::installed_binary(binary)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| prismattyc_mux::executable::sibling(&exe, &[binary]))
            })
            .unwrap_or_else(|| find_bin(&[], &[binary]));
        let label = prismattyc_mux::release_update::version_label(&executable).ok();
        installed.push(json!({"component":binary,"path":executable,"version":label}));
    }
    let mut running = Vec::new();
    for component in ["host", "mcp"] {
        for pid in components::live_components(&paths.socket, component) {
            let path = components::directory(&paths.socket).join(format!("{component}-{pid}.json"));
            if let Ok(Ok(value)) =
                std::fs::read(path).map(|b| serde_json::from_slice::<serde_json::Value>(&b))
            {
                running.push(value);
            }
        }
    }
    let liveness = probe_socket_liveness(&paths.socket);
    let occupied = liveness == SocketLiveness::Live
        || (matches!(liveness, SocketLiveness::Missing | SocketLiveness::Stale)
            && prismattyc_mux::daemon_lock::DaemonLock::is_held(&paths.socket)?);
    let daemon = if occupied {
        let report = (|| -> Result<serde_json::Value> {
            let mut client = Client::connect(&paths.socket)?;
            match client.request(|request_id| ControlRequest::ServerInfo {
                version: PROTOCOL_VERSION,
                request_id,
            })? {
                ControlResponseData::ServerInfo {
                    package_version,
                    pid,
                } => Ok(json!({"version":package_version,"pid":pid})),
                _ => bail!("daemon returned an unexpected version response"),
            }
        })();
        match report {
            Ok(report) => report,
            Err(error) => {
                let holder =
                    prismattyc_mux::daemon_lock::DaemonLock::holder_description(&paths.socket);
                json!({"status":"running","version":null,"pid":read_valid_pid(paths),"detail":format!("{holder} is still running, but its version could not be read: {error:#}. Wait for it to respond before restarting.")})
            }
        }
    } else {
        json!({"status":"not_running"})
    };
    println!(
        "{}",
        json!({"release_repository":prismattyc_mux::release_update::REPOSITORY,"installed":installed,"running":running,"daemon":daemon})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn except_pid_is_collected_and_must_be_a_process_id() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let opts = parse(&args(&["--host", "--except-pid", "7", "--except-pid", "9"])).unwrap();
        assert_eq!(opts.except_pids, vec![7, 9]);
        assert!(parse(&args(&["--host", "--except-pid", "x"])).is_err());
        assert!(parse(&args(&["--host", "--except-pid"])).is_err());
    }

    #[test]
    fn restart_selection_requires_explicit_permission_to_stop_sessions() {
        let parse_words =
            |words: &[&str]| parse(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        for words in [vec![], vec!["--all"], vec!["--json", "--plan"]] {
            let options = parse_words(&words).unwrap();
            assert!(options.host && options.daemon && options.mcp);
            assert!(!options.stop_sessions);
        }
        let host = parse_words(&["--host"]).unwrap();
        assert!(host.host && !host.daemon && !host.mcp);
        let mcp = parse_words(&["--mcp", "--worker"]).unwrap();
        assert!(mcp.mcp && mcp.worker && !mcp.host && !mcp.daemon);
        for daemon in ["--daemon", "--mux"] {
            let options = parse_words(&[daemon, "--stop-sessions", "--plan"]).unwrap();
            assert!(options.daemon && options.stop_sessions && options.plan);
            assert!(!options.host && !options.mcp);
        }
        assert!(parse_words(&["--host", "--stop-sessions"]).is_err());
        assert!(parse_words(&["--mcp", "--stop-sessions"]).is_err());
        assert!(parse_words(&["--daemno"])
            .unwrap_err()
            .to_string()
            .contains("unknown restart option"));
    }
}
