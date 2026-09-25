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
}
fn parse(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();
    let mut all = false;
    for arg in args {
        match arg.as_str() {
            "--host" => opts.host = true,
            "--daemon" | "--mux" => opts.daemon = true,
            "--mcp" => opts.mcp = true,
            "--all" => all = true,
            "--plan" => opts.plan = true,
            "--stop-sessions" => opts.stop_sessions = true,
            "--worker" => opts.worker = true,
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

pub(super) fn restart(paths: &Paths, args: Vec<String>) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("pmux restart [--all|--host|--daemon|--mcp] [--plan] [--json]\n\nRestart cooperative components. Default: all safe components.\nHost: restore windows and reconnect mux sessions; defer if blank terminals exist.\nMCP: restart supervised adapters without replaying pending operations.\nDaemon: restart only when no sessions exist. --stop-sessions explicitly permits\nstopping every session; the restart then runs from a detached helper.\nUse --plan to inspect the impact before changing anything.");
        return Ok(());
    }
    let opts = parse(&args)?;
    let sessions = match probe_socket_liveness(&paths.socket) {
        SocketLiveness::Live => list_sessions(paths)?,
        SocketLiveness::Missing | SocketLiveness::Stale => Vec::new(),
        SocketLiveness::Foreign => bail!("foreign control socket"),
    };
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
        let output = OpenOptions::new().create(true).append(true).open(&log)?;
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
        prismattyc_mux::platform::detach_command(&mut child);
        let process = child.spawn()?;
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
                if let Some(server) = prismattyc_mux::release_update::installed_binary("pmuxd") {
                    command.env("PMUX_SERVER", server);
                }
                anyhow::ensure!(command.status()?.success(), "daemon {verb} failed");
            }
            results.push(json!({"component":"daemon","status":"restarted"}));
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
    let daemon = if probe_socket_liveness(&paths.socket) == SocketLiveness::Live {
        let mut client = Client::connect(&paths.socket)?;
        match client.request(|request_id| ControlRequest::ServerInfo {
            version: PROTOCOL_VERSION,
            request_id,
        }) {
            Ok(ControlResponseData::ServerInfo {
                package_version,
                pid,
            }) => json!({"version":package_version,"pid":pid}),
            _ => {
                json!({"version":null,"detail":"running daemon predates component version reporting"})
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
