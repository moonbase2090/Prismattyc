//! User-facing restart coordinator. Live PTY owners are deferred by default.
use super::*;
use prismattyc_mux::component_restart as components;
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
            "--json" => {}
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
        println!("pmux restart [--all|--host|--daemon|--mcp] [--plan] [--json]\n\nRestart cooperative components. Default: all safe components.\nHost: restore windows and reconnect mux sessions; defer if blank terminals exist.\nMCP: restart supervised adapters without replaying pending operations.\nDaemon: restart only when no sessions exist. --stop-sessions explicitly permits\nstopping every session; the restart then runs from a detached helper.\n--consented-config FILE limits an update restart to the unchanged saved session list.\nUse --plan to inspect the impact before changing anything.");
        return Ok(());
    }
    let opts = parse(&args)?;
    let sessions = running_sessions(paths)?;
    let hosts = components::live_components(&paths.socket, "host");
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
            if let Some(file) = &opts.consent_config {
                let (mut client, client_id, session_ids) =
                    session_config::check_for_restart(paths, file)
                        .context("session consent check failed; no sessions were stopped")?;
                let snapshot = take_snapshot(&mut client)?;
                session_config::hold_spaces(
                    snapshot
                        .sessions
                        .iter()
                        .filter(|session| session_ids.contains(&session.id))
                        .filter_map(|session| session.space_id.as_deref()),
                )?;
                if opts.delete_consent_config {
                    std::fs::remove_file(file)?;
                }
                for session_id in session_ids {
                    super::destroy_session(&mut client, session_id).with_context(|| {
                        format!("consented session {session_id} could not be stopped")
                    })?;
                }
                shutdown_idle(paths, &mut client, client_id)?;
                let mut up = Command::new(&executable);
                up.arg("--socket")
                    .arg(&paths.socket)
                    .arg("up")
                    .stdout(Stdio::null());
                anyhow::ensure!(up.status()?.success(), "daemon up failed");
                results.push(json!({"component":"daemon","status":"restarted"}));
            } else {
                let verbs: &[&str] = if opts.stop_sessions {
                    &["stop", "up"]
                } else {
                    stop_idle(paths)?;
                    &["up"]
                };
                for verb in verbs {
                    let mut command = Command::new(&executable);
                    command
                        .arg("--socket")
                        .arg(&paths.socket)
                        .arg(verb)
                        .stdout(Stdio::null());
                    if let Some(server) = prismattyc_mux::release_update::installed_binary("pmuxd")
                    {
                        command.env("PMUX_SERVER", server);
                    }
                    anyhow::ensure!(command.status()?.success(), "daemon {verb} failed");
                }
                results.push(json!({"component":"daemon","status":"restarted"}));
            }
        } else {
            results.push(json!({"component":"daemon","status":"deferred","detail":"sessions are running; use --stop-sessions only when ready to end them","sessions":sessions}));
        }
    }
    restart_cooperative(paths, &opts, hosts, adapters, &mut results)?;
    println!(
        "{}",
        json!({"status":"restart_results","components":results})
    );
    Ok(())
}

fn restart_cooperative(
    paths: &Paths,
    opts: &Options,
    hosts: Vec<u32>,
    adapters: Vec<u32>,
    results: &mut Vec<serde_json::Value>,
) -> Result<()> {
    let mut pending = Vec::new();
    for (component, pids, enabled) in [("host", hosts, opts.host), ("mcp", adapters, opts.mcp)] {
        if !enabled {
            continue;
        }
        if pids.is_empty() {
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
