//! `pmux restart` and `pmux login` against a private socket and config.
//! macOS login registration is not loaded here: that path calls launchctl.

#![cfg(unix)]

use prismattyc_mux::release_update::progress;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

mod support;
use support::clear_command_env;

fn temp_root(kind: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-{kind}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn socket_path(root: &Path) -> PathBuf {
    root.join("pmux.sock")
}

/// Tears down the test daemon however the test ends. `pmux restart` replaces
/// the child with a new detached pmuxd, so killing the child alone could
/// leave that replacement running; stopping whatever serves the private
/// socket covers both.
struct ServerGuard {
    child: Child,
    socket: PathBuf,
    root: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.socket.exists() {
            let _ = pmux(&self.socket, &self.root, &[], &["stop"]);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn start_server(socket: &Path, root: &Path) -> ServerGuard {
    start_server_with_env(socket, root, &[])
}

fn start_server_with_env(socket: &Path, root: &Path, env: &[(&str, &str)]) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    for (key, value) in env {
        command.env(key, value);
    }
    let stderr_path = root.join("daemon.stderr");
    let mut guard = ServerGuard {
        child: prismattyc_process::spawn(
            command
                .arg("--socket")
                .arg(socket)
                .args(["--", "/bin/sh", "-c", "exec sleep 999"])
                .stdout(Stdio::piped())
                .stderr(std::fs::File::create(&stderr_path).unwrap()),
        )
        .expect("spawn pmuxd"),
        socket: socket.to_path_buf(),
        root: root.to_path_buf(),
    };
    if let Err(error) = wait_for_server_ready(&mut guard.child, socket, &stderr_path) {
        panic!("{error}");
    }
    guard
}

/// A shell snippet that, once the command before it has finished, reports
/// completion through a FIFO, plus the receiver that sees the report.
/// The command's own output file exists before it holds the output, so
/// waiting for the file is not waiting for the command.
fn completion_signal(root: &Path) -> (String, std::sync::mpsc::Receiver<()>) {
    let fifo = root.join("launch-done");
    assert!(
        prismattyc_process::status(Command::new("mkfifo").arg(&fifo))
            .unwrap()
            .success()
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = fifo.clone();
    std::thread::spawn(move || {
        if std::fs::read(&reader).is_ok() {
            let _ = sender.send(());
        }
    });
    (format!("echo > '{}'", fifo.display()), receiver)
}

fn await_completion(signal: &std::sync::mpsc::Receiver<()>, seconds: u64) {
    signal
        .recv_timeout(Duration::from_secs(seconds))
        .expect("the restored command never reported completion");
}

fn wait_for_server_ready(
    child: &mut Child,
    socket: &Path,
    stderr_path: &Path,
) -> Result<(), String> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::fd::AsRawFd;

    let mut stdout = Vec::new();
    let result = (|| {
        let fd = child
            .stdout
            .as_ref()
            .expect("capture pmuxd stdout")
            .as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(Error::last_os_error());
        }
        let ready = format!("{}\n", socket.display());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "readiness deadline elapsed",
                ));
            }
            let mut event = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let count = unsafe { libc::poll(&mut event, 1, remaining.as_millis().max(1) as i32) };
            if count == -1 {
                let error = Error::last_os_error();
                if error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "readiness deadline elapsed",
                ));
            }
            let mut bytes = [0; 4096];
            match child.stdout.as_mut().unwrap().read(&mut bytes) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "pmuxd closed stdout before announcing readiness",
                    ));
                }
                Ok(count) => stdout.extend_from_slice(&bytes[..count]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            if stdout
                .split_inclusive(|byte| *byte == b'\n')
                .any(|line| line == ready.as_bytes())
            {
                return Ok(());
            }
        }
    })();
    result.map_err(|error| {
        let status = match child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            Ok(None) => "still running".into(),
            Err(error) => format!("could not read status: {error}"),
        };
        let stderr = std::fs::read_to_string(stderr_path)
            .unwrap_or_else(|error| format!("could not read stderr: {error}"));
        format!(
            "pmuxd did not announce readiness on {}: {error}; status={status}; stdout={:?}; stderr={stderr}",
            socket.display(),
            String::from_utf8_lossy(&stdout),
        )
    })
}

#[test]
fn an_existing_socket_is_not_a_daemon_readiness_signal() {
    use std::os::unix::net::UnixListener;

    let root = temp_root("startup-failure");
    let socket = socket_path(&root);
    let stale = UnixListener::bind(&socket).unwrap();
    let stderr_path = root.join("daemon.stderr");
    let mut child = prismattyc_process::spawn(Command::new("/bin/sh")
        .args([
            "-c",
            "printf 'STARTUP_STDOUT_LITERAL\\n'; printf 'STARTUP_ERROR_LITERAL\\n' >&2; exit 17",
        ])
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&stderr_path).unwrap()))
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(17));

    let error = wait_for_server_ready(&mut child, &socket, &stderr_path)
        .expect_err("a socket owned by another process is not readiness");
    assert!(error.contains("exit status: 17"), "{error}");
    assert!(error.contains("STARTUP_STDOUT_LITERAL"), "{error}");
    assert!(error.contains("STARTUP_ERROR_LITERAL"), "{error}");

    drop(stale);
    std::fs::remove_dir_all(root).unwrap();
}

fn apply_isolation(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env(
            "PRISMATTYC_CONFIG",
            root.join("config").join("prismattyc.toml"),
        )
        .env_remove("PMUX_SOCKET")
        .env_remove("PMUX_AGENT")
        .env_remove("PRISMATTYC_PANE_ID")
        .env_remove("PRISMATTYC_NO_LOGIN_SERVICE");
}

fn pmux(socket: &Path, root: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    command
        .arg("--socket")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null());
    for (key, value) in extra {
        command.env(key, value);
    }
    prismattyc_process::spawn(&mut command)
        .and_then(std::process::Child::wait_with_output)
        .expect("run pmux")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "pmux failed: {}", text(output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn fail(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "expected failure: {}",
        text(output)
    );
    let body = text(output);
    assert!(body.contains(needle), "missing {needle} in {body}");
}

fn answer_host(socket: &Path) -> std::thread::JoinHandle<()> {
    let socket = socket.to_path_buf();
    let pid = std::process::id();
    std::thread::spawn(move || {
        let path = prismattyc_mux::component_restart::request_path(&socket, "host", pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(id) = value["id"].as_str() {
                        let response = serde_json::json!({
                            "id": id,
                            "pid": pid,
                            "status": "restarted",
                            "detail": "test host",
                            "version": "0.0.0"
                        });
                        let dest = prismattyc_mux::component_restart::response_path(&socket, id);
                        let _ = std::fs::write(dest, response.to_string());
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })
}

#[test]
fn restart_plan_never_reports_a_locked_unresponsive_daemon_as_empty() {
    let root = temp_root("plan-stalled");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let pid = server.child.id();
    // The guard must resume the private daemon before ServerGuard tears it down,
    // including when an assertion fails.
    struct Resume(u32);
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = prismattyc_process::status(
                Command::new("kill").args(["-CONT", &self.0.to_string()]),
            );
        }
    }
    let _resume = Resume(pid);
    assert!(
        prismattyc_process::status(Command::new("kill").args(["-STOP", &pid.to_string()]))
            .unwrap()
            .success()
    );
    let output = pmux(&socket, &root, &[], &["restart", "--plan", "--json"]);
    fail(&output, "Could not read running sessions");
    let detail = text(&output);
    assert!(detail.contains(&format!("pid {pid}")), "{detail}");
    assert!(detail.contains("pmuxd"), "{detail}");
    assert!(!detail.contains("\"sessions\":[]"), "{detail}");
    let versions: serde_json::Value =
        serde_json::from_str(&ok(&pmux(&socket, &root, &[], &["versions"]))).unwrap();
    assert_eq!(versions["daemon"]["status"], "running");
    assert_eq!(versions["daemon"]["pid"], pid);
    assert!(versions["daemon"]["detail"]
        .as_str()
        .unwrap()
        .contains(&format!("pid {pid}")));
    let mut duplicate = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut duplicate);
    apply_isolation(&mut duplicate, &root);
    let refused = prismattyc_process::spawn(duplicate.arg("--socket").arg(&socket))
        .and_then(std::process::Child::wait_with_output)
        .unwrap();
    fail(&refused, "daemon lock");
    assert!(
        text(&refused).contains(&format!("pid {pid}")),
        "{}",
        text(&refused)
    );
    assert!(text(&refused).contains("pmuxd"), "{}", text(&refused));
    let resume = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        drop(_resume);
    });
    let plan: serde_json::Value = serde_json::from_str(&ok(&pmux(
        &socket,
        &root,
        &[],
        &["restart", "--plan", "--json"],
    )))
    .unwrap();
    resume.join().unwrap();
    assert_eq!(plan["sessions"].as_array().unwrap().len(), 1);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_open_window_that_stopped_renewing_its_registration_is_reported_idle() {
    let root = temp_root("restart-idle");
    let missing = root.join("missing.sock");
    prismattyc_mux::component_restart::register(&missing, "host").unwrap();
    let pid = std::process::id();
    let path =
        prismattyc_mux::component_restart::directory(&missing).join(format!("host-{pid}.json"));
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let fresh = prismattyc_mux::component_restart::FRESH.as_millis() as u64;
    value["sampled_ms"] = (prismattyc_mux::host_render_status::unix_ms() - fresh - 1).into();
    std::fs::write(&path, value.to_string()).unwrap();

    let report: serde_json::Value =
        serde_json::from_str(ok(&pmux(&missing, &root, &[], &["restart", "--host"])).trim())
            .unwrap();
    let hosts: Vec<&serde_json::Value> = report["components"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["component"] == "host")
        .collect();
    assert_eq!(hosts.len(), 1, "{report}");
    assert_eq!(hosts[0]["status"], "idle", "{report}");
    assert_eq!(hosts[0]["pid"], pid, "{report}");
    assert!(
        !prismattyc_mux::component_restart::request_path(&missing, "host", pid).exists(),
        "an idle window is not sent a request it would act on later"
    );
}

#[test]
fn restart_plans_and_cooperative_components() {
    let root = temp_root("restart");
    let missing = root.join("missing.sock");

    let help = ok(&pmux(&missing, &root, &[], &["restart", "--help"]));
    assert!(help.contains("pmux restart"), "{help}");
    assert!(help.contains("--plan"), "{help}");
    fail(
        &pmux(&missing, &root, &[], &["restart", "--bogus"]),
        "unknown restart option",
    );
    fail(
        &pmux(
            &missing,
            &root,
            &[],
            &["restart", "--host", "--stop-sessions"],
        ),
        "--stop-sessions requires a daemon restart",
    );

    let planned = ok(&pmux(&missing, &root, &[], &["restart", "--plan"]));
    assert!(planned.contains("\"status\":\"planned\""), "{planned}");
    assert!(planned.contains("\"daemon_deferred\":false"), "{planned}");
    let host_only = ok(&pmux(
        &missing,
        &root,
        &[],
        &["restart", "--host", "--plan", "--json"],
    ));
    assert!(host_only.contains("\"daemon\":false"), "{host_only}");
    let mux_only = ok(&pmux(
        &missing,
        &root,
        &[],
        &["restart", "--mux", "--mcp", "--plan"],
    ));
    assert!(mux_only.contains("\"daemon\":true"), "{mux_only}");

    let foreign = root.join("not-a-socket");
    std::fs::write(&foreign, b"nope").unwrap();
    fail(
        &pmux(&foreign, &root, &[], &["restart", "--plan"]),
        "foreign control socket",
    );

    let quiet = ok(&pmux(&missing, &root, &[], &["restart", "--mcp"]));
    assert!(quiet.contains("not_registered"), "{quiet}");
    assert!(quiet.contains("\"component\":\"mcp\""), "{quiet}");

    prismattyc_mux::component_restart::register(&missing, "host").unwrap();
    let answered = answer_host(&missing);
    let restarted = ok(&pmux(&missing, &root, &[], &["restart", "--host"]));
    answered.join().unwrap();
    // A live update or a restart: either way the server comes back.
    assert!(
        restarted.contains("\"status\":\"restarted\"")
            || restarted.contains("\"status\":\"updated_live\""),
        "{restarted}"
    );
    assert!(restarted.contains("test host"), "{restarted}");

    prismattyc_mux::component_restart::register(&missing, "host").unwrap();
    let pending = ok(&pmux(&missing, &root, &[], &["restart", "--host"]));
    assert!(pending.contains("\"status\":\"pending\""), "{pending}");

    let server_socket = socket_path(&root);
    let server = start_server(&server_socket, &root);
    let deferred = ok(&pmux(
        &server_socket,
        &root,
        &[],
        &["restart", "--daemon", "--plan"],
    ));
    assert!(deferred.contains("\"daemon_deferred\":true"), "{deferred}");

    let scheduled = ok(&pmux(
        &server_socket,
        &root,
        &[],
        &["restart", "--daemon", "--stop-sessions"],
    ));
    assert!(
        scheduled.contains("\"status\":\"scheduled\""),
        "{scheduled}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut replacement_pid = None;
    while Instant::now() < deadline {
        let versions = pmux(&server_socket, &root, &[], &["versions"]);
        if versions.status.success() {
            let versions: serde_json::Value = serde_json::from_slice(&versions.stdout).unwrap();
            if let Some(pid) = versions["daemon"]["pid"].as_u64() {
                if pid != u64::from(server.child.id()) {
                    replacement_pid = Some(pid);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        replacement_pid.is_some(),
        "restart never replaced the original daemon"
    );
    let status = ok(&pmux(&server_socket, &root, &[], &["status"]));
    assert!(status.contains("running"), "{status}");
    let _ = pmux(&server_socket, &root, &[], &["stop"]);
    drop(server);

    let skipped = ok(&pmux(
        &missing,
        &root,
        &[("PRISMATTYC_NO_LOGIN_SERVICE", "1")],
        &["login", "sync"],
    ));
    assert!(skipped.is_empty(), "{skipped}");
    assert!(!root.join("home/Library/LaunchAgents").exists());

    let disabled = ok(&pmux(&missing, &root, &[], &["login", "disable"]));
    assert!(disabled.contains("disabled"), "{disabled}");
    let config = std::fs::read_to_string(root.join("config/prismattyc.toml")).unwrap();
    assert!(config.contains("start_at_login = false"), "{config}");
    assert!(!root.join("home/Library/LaunchAgents").exists());

    let status = ok(&pmux(&missing, &root, &[], &["login", "status"]));
    assert!(status.contains("login restore: disabled"), "{status}");
    assert!(status.contains("login registration: absent"), "{status}");
}

#[cfg(target_os = "linux")]
#[test]
fn login_sync_registers_a_user_unit() {
    let root = temp_root("login");
    let socket = socket_path(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = root.join("systemctl.log");
    std::fs::write(
        bin.join("systemctl"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SYSTEMCTL_LOG\"\nif [ \"${FAIL_SYSTEMCTL:-}\" = 1 ]; then\n  echo systemctl refused >&2\n  exit 1\nfi\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(bin.join("systemctl"))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(bin.join("systemctl"), perms).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let isolated = [
        ("PATH", path.as_str()),
        ("SYSTEMCTL_LOG", log.to_str().unwrap()),
    ];

    let enabled = ok(&pmux(&socket, &root, &isolated, &["login", "enable"]));
    assert!(enabled.contains("login restore: enabled"), "{enabled}");
    assert!(
        enabled.contains("login registration: installed"),
        "{enabled}"
    );
    let record = std::fs::read_to_string(&log).unwrap();
    assert!(record.contains("daemon-reload"), "{record}");
    assert!(record.contains("--user enable --now"), "{record}");
    let unit = std::fs::read_dir(root.join("config/systemd/user"))
        .unwrap()
        .find_map(|entry| entry.ok())
        .expect("unit file")
        .path();
    assert!(unit.extension().and_then(|ext| ext.to_str()) == Some("service"));
    let body = std::fs::read_to_string(&unit).unwrap();
    assert!(body.contains("login run"), "{body}");

    std::fs::write(&log, "").unwrap();
    ok(&pmux(&socket, &root, &isolated, &["login", "sync"]));
    let again = std::fs::read_to_string(&log).unwrap();
    assert!(!again.contains("daemon-reload"), "{again}");
    assert!(again.contains("enable --now"), "{again}");

    let changed_home = root.join("other-home");
    std::fs::create_dir_all(&changed_home).unwrap();
    let mut changed = isolated.to_vec();
    changed.push(("HOME", changed_home.to_str().unwrap()));
    ok(&pmux(&socket, &root, &changed, &["login", "sync"]));
    let reloaded = std::fs::read_to_string(&log).unwrap();
    assert!(reloaded.contains("daemon-reload"), "{reloaded}");

    let planted = unit.with_extension("vbs");
    std::fs::write(&planted, "script").unwrap();
    let removed = ok(&pmux(&socket, &root, &isolated, &["login", "disable"]));
    assert!(removed.contains("disabled"), "{removed}");
    assert!(!unit.exists());
    assert!(!planted.exists());
    let disabled = std::fs::read_to_string(&log).unwrap();
    assert!(disabled.contains("disable --now"), "{disabled}");

    fail(
        &pmux(
            &socket,
            &root,
            &[
                ("PATH", path.as_str()),
                ("SYSTEMCTL_LOG", log.to_str().unwrap()),
                ("HOME", "bad\nhome"),
                (
                    "PRISMATTYC_CONFIG",
                    root.join("config/prismattyc.toml").to_str().unwrap(),
                ),
            ],
            &["login", "enable"],
        ),
        "invalid login environment",
    );

    fail(
        &pmux(
            &socket,
            &root,
            &[
                ("PATH", path.as_str()),
                ("SYSTEMCTL_LOG", log.to_str().unwrap()),
                ("FAIL_SYSTEMCTL", "1"),
            ],
            &["login", "enable"],
        ),
        "login service command failed",
    );
}

#[test]
fn session_configuration_restores_names_directories_and_commands_without_replay() {
    let root = temp_root("session-configuration");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let cwd = root.join("project");
    std::fs::create_dir(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();
    let mut create = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut create);
    apply_isolation(&mut create, &root);
    ok(
        &prismattyc_process::spawn(create.current_dir(&cwd).arg("--socket").arg(&socket).args([
            "new",
            "--no-attach",
            "work",
        ]))
        .and_then(std::process::Child::wait_with_output)
        .unwrap(),
    );
    let file = root.join("configuration.json");
    let (done, signal) = completion_signal(&root);
    let launch = format!("printf restored >> launches && {done}");
    ok(&pmux(
        &socket,
        &root,
        &[(
            prismattyc_mux::procinfo::TEST_FOREGROUND_COMMAND_ENV,
            &launch,
        )],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let work = saved["space"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "work")
        .unwrap();
    assert_eq!(work["windows"][0]["root"]["cwd"], cwd.to_str().unwrap());
    assert_eq!(work["windows"][0]["root"]["command"], launch);
    fail(
        &pmux(
            &socket,
            &root,
            &[],
            &["session-config", "save", file.to_str().unwrap()],
        ),
        "choose a new filename",
    );
    ok(&pmux(&socket, &root, &[], &["stop", "work"]));
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    await_completion(&signal, 8);
    assert_eq!(
        std::fs::read_to_string(cwd.join("launches")).unwrap(),
        "restored"
    );
    let repeated = ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    assert!(repeated.contains("Restored 0 sessions."), "{repeated}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("launches")).unwrap(),
        "restored"
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

/// A new pane can read as busy at the moment the saved command is written
/// (a slow shell start, or on Windows a process tree that cannot be read).
/// Restore must still replay the command into the pane it just created.
#[test]
fn restore_replays_the_saved_command_into_a_pane_that_looks_busy() {
    let root = temp_root("cfg-busy");
    let socket = socket_path(&root);
    // A shell that is a foreground program (not a shell) for a few seconds
    // before it becomes one, so the new pane looks busy when restore writes.
    let slow = root.join("slow-shell");
    std::fs::write(
        &slow,
        "#!/usr/bin/env python3\nimport os, time\ntime.sleep(3)\nos.execv('/bin/sh', ['sh'])\n",
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let server = start_server_with_env(&socket, &root, &[("SHELL", slow.to_str().unwrap())]);
    let cwd = root.join("project");
    std::fs::create_dir(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();
    let mut create = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut create);
    apply_isolation(&mut create, &root);
    ok(
        &prismattyc_process::spawn(create.current_dir(&cwd).arg("--socket").arg(&socket).args([
            "new",
            "--no-attach",
            "work",
        ]))
        .and_then(std::process::Child::wait_with_output)
        .unwrap(),
    );
    let file = root.join("configuration.json");
    let (done, signal) = completion_signal(&root);
    let launch = format!("printf restored >> launches && {done}");
    ok(&pmux(
        &socket,
        &root,
        &[(
            prismattyc_mux::procinfo::TEST_FOREGROUND_COMMAND_ENV,
            &launch,
        )],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    ok(&pmux(&socket, &root, &[], &["stop", "work"]));
    let restored = ok(&pmux(
        &socket,
        &root,
        &[("SHELL", slow.to_str().unwrap())],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    assert!(
        !restored.contains("Kept the program already running"),
        "restore skipped the command in its own new pane: {restored}"
    );
    await_completion(&signal, 15);
    assert_eq!(
        std::fs::read_to_string(cwd.join("launches")).unwrap_or_default(),
        "restored",
        "the saved command was not replayed into the new pane"
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

/// A pane running an agent that was given a startup prompt is saved as the
/// agent's resume command. Restoring must not run that prompt again.
#[test]
fn a_saved_agent_resumes_instead_of_replaying_its_startup_prompt() {
    let root = temp_root("agent-resume");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let cwd = root.join("project");
    std::fs::create_dir(&cwd).unwrap();
    // A process named `claude` with a prompt in its arguments, which stays up.
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "seat",
            "--",
            "/bin/bash",
            "-c",
            "exec -a claude /bin/sh -c 'sleep 999; :' claude 'do the secret task'",
        ],
    ));
    let file = root.join("configuration.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    let command = loop {
        let _ = std::fs::remove_file(&file);
        let saved = pmux(
            &socket,
            &root,
            &[],
            &["session-config", "save", file.to_str().unwrap()],
        );
        if saved.status.success() {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            let seat = value["space"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "seat")
                .cloned();
            if let Some(command) = seat.and_then(|s| {
                s["windows"][0]["root"]["command"]
                    .as_str()
                    .map(str::to_owned)
            }) {
                break command;
            }
        }
        assert!(Instant::now() < deadline, "the agent was never saved");
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(command, "claude --continue", "saved {command:?}");
    assert!(
        !command.contains("secret"),
        "the prompt was saved: {command}"
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hermes_saved_restart_does_not_replay_the_startup_query() {
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
    use std::os::unix::net::UnixListener;

    let root = temp_root("hermes-restart");
    let socket = socket_path(&root);
    let observations = root.join("argv.sock");
    let listener = UnixListener::bind(&observations).unwrap();
    let await_readable = |fd: BorrowedFd<'_>| {
        let mut ready = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut ready, 1, 20_000) },
            1,
            "no launch signal"
        );
    };
    let receive_argv = || {
        await_readable(listener.as_fd());
        let (peer, _) = listener.accept().unwrap();
        await_readable(peer.as_fd());
        serde_json::from_reader::<_, Vec<String>>(peer).unwrap()
    };
    let fixture = root.join("hermes");
    std::fs::write(
        &fixture,
        r#"#!/usr/bin/env python3
import json, os, socket, sys
with socket.socket(socket.AF_UNIX) as peer:
    peer.connect(os.environ["FIXTURE_ARGV_SOCKET"])
    peer.sendall(json.dumps(sys.argv[1:]).encode())
# Keep this fixture child alive until its owning private daemon stops it.
os.execv("/bin/sleep", ["sleep", "999"])
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &fixture,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let server = start_server_with_env(
        &socket,
        &root,
        &[("FIXTURE_ARGV_SOCKET", observations.to_str().unwrap())],
    );
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "--no-agent",
            "work",
            "--",
            fixture.to_str().unwrap(),
            "--profile",
            "demo",
            "chat",
            "--resume",
            "conversation-123",
            "--model",
            "model one",
            "--provider",
            "custom",
            "-q",
            "run the original task",
        ],
    ));
    assert_eq!(
        receive_argv(),
        [
            "--profile",
            "demo",
            "chat",
            "--resume",
            "conversation-123",
            "--model",
            "model one",
            "--provider",
            "custom",
            "-q",
            "run the original task",
        ]
    );
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let work = saved["space"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "work")
        .unwrap();
    ok(&pmux(&socket, &root, &[], &["stop", "work"]));
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    assert_eq!(
        receive_argv(),
        [
            "chat",
            "--resume=conversation-123",
            "--profile",
            "demo",
            "--model",
            "model one",
            "--provider",
            "custom",
        ]
    );
    assert_eq!(
        work["windows"][0]["root"]["command"],
        format!("{} chat --resume=conversation-123 --profile demo --model 'model one' --provider custom", fixture.display())
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_configuration_replays_original_launch_arguments_after_exec() {
    let root = temp_root("original-launch-arguments");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let log = root.join("arguments");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "work",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s\\n' \"$@\" >> \"$0\"; exec sleep 999",
            log.to_str().unwrap(),
            "resume",
            "conversation-123",
            "--model",
            "model one",
            "--config",
            "model_reasoning_effort=high",
        ],
    ));
    let doctor = ok(&pmux(&socket, &root, &[], &["doctor", "work"]));
    let child: u32 = doctor
        .lines()
        .find_map(|line| line.split_once("child "))
        .and_then(|(_, rest)| rest.split(',').next())
        .unwrap()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !prismattyc_mux::procinfo::live_foreground_command(child)
        .is_some_and(|command| command.contains("sleep 999"))
    {
        assert!(Instant::now() < deadline, "fixture did not exec sleep");
        std::thread::sleep(Duration::from_millis(20));
    }
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let work = saved["space"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "work")
        .unwrap();
    let command = work["windows"][0]["root"]["command"].as_str().unwrap();
    assert!(command.starts_with("/bin/sh -c "), "{command}");
    assert!(
        command.ends_with(
            "resume conversation-123 --model 'model one' --config model_reasoning_effort=high"
        ),
        "{command}"
    );
    ok(&pmux(&socket, &root, &[], &["stop", "work"]));
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    let expected =
        "resume\nconversation-123\n--model\nmodel one\n--config\nmodel_reasoning_effort=high\n";
    let twice = format!("{expected}{expected}");
    let deadline = Instant::now() + Duration::from_secs(8);
    while std::fs::read_to_string(&log).unwrap_or_default() != twice {
        assert!(
            Instant::now() < deadline,
            "original arguments were not replayed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_configuration_restores_valid_sessions_after_a_stale_directory() {
    let root = temp_root("stale-session-directory");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    for name in ["alpha", "stale", "zulu"] {
        let cwd = root.join(name);
        std::fs::create_dir(&cwd).unwrap();
        ok(&pmux(
            &socket,
            &root,
            &[],
            &["new", "--no-attach", "--cwd", cwd.to_str().unwrap(), name],
        ));
    }
    let space_file =
        PathBuf::from(ok(&pmux(&socket, &root, &[], &["space", "save", "desk"])).trim());
    let space: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&space_file).unwrap()).unwrap();
    let marker = space_file
        .parent()
        .unwrap()
        .join(format!(".restoring-{}", space["id"].as_str().unwrap()));
    std::fs::write(&marker, b"").unwrap();
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[(prismattyc_mux::procinfo::TEST_FOREGROUND_COMMAND_ENV, "")],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for session in saved["space"]["sessions"].as_array_mut().unwrap() {
        let name = session["name"].as_str().unwrap().to_string();
        if matches!(name.as_str(), "alpha" | "zulu") {
            session["windows"][0]["root"]["command"] = format!("printf {name} > restored").into();
        } else if name == "stale" {
            let stale = session["windows"][0]["root"].clone();
            session["windows"][0]["root"] = serde_json::json!({
                "kind": "split", "axis": "horizontal", "ratio": 0.5,
                "first": {"kind": "leaf", "cwd": root}, "second": stale,
            });
        }
    }
    std::fs::write(&file, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();
    for name in ["alpha", "stale", "zulu"] {
        ok(&pmux(&socket, &root, &[], &["stop", name]));
    }
    std::fs::remove_dir(root.join("stale")).unwrap();
    let restored = pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    );
    let listing = ok(&pmux(&socket, &root, &[], &["ls"]));
    assert!(listing.contains("zulu"), "{listing}\n{}", text(&restored));
    fail(&restored, "stale");
    assert!(marker.exists(), "a partial restore must keep autosave held");
    std::fs::create_dir(root.join("stale")).unwrap();
    let retry = ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    assert!(retry.contains("Restored 1 sessions."), "{retry}");
    assert!(
        !marker.exists(),
        "a complete retry releases the autosave hold"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    for name in ["alpha", "zulu"] {
        let marker = root.join(name).join("restored");
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(std::fs::read_to_string(marker).unwrap(), name);
    }
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn login_preference_keeps_comments_and_unrelated_config() {
    let root = temp_root("login-preservation");
    let socket = socket_path(&root);
    let config = root.join("config/prismattyc.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "# login explanation\nstart_at_login = false # inline explanation\nfuture_root = { count = 7 }\n",
    )
    .unwrap();
    let isolated = [("PRISMATTYC_NO_LOGIN_SERVICE", "1")];
    ok(&pmux(&socket, &root, &isolated, &["login", "enable"]));
    let enabled = std::fs::read_to_string(&config).unwrap();
    let values: toml::Value = enabled.parse().unwrap();
    assert_eq!(values["start_at_login"], toml::Value::Boolean(true));
    assert_eq!(values["future_root"]["count"], toml::Value::Integer(7));
    assert!(enabled.contains("# login explanation"));
    assert!(enabled.contains("# inline explanation"));
    ok(&pmux(&socket, &root, &isolated, &["login", "disable"]));
    let disabled = std::fs::read_to_string(&config).unwrap();
    let values: toml::Value = disabled.parse().unwrap();
    assert_eq!(values["start_at_login"], toml::Value::Boolean(false));
    assert_eq!(values["future_root"]["count"], toml::Value::Integer(7));
    assert!(disabled.contains("# inline explanation"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_configuration_check_rejects_in_place_layout_changes() {
    let root = temp_root("cfg-in");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "work",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "check", file.to_str().unwrap()],
    ));

    ok(&pmux(
        &socket,
        &root,
        &[],
        &["rename-pane", "--session", "work", "renamed"],
    ));
    let changed = pmux(
        &socket,
        &root,
        &[],
        &[
            "restart",
            "--daemon",
            "--stop-sessions",
            "--consented-config",
            file.to_str().unwrap(),
            "--worker",
            "--json",
        ],
    );
    fail(&changed, "session consent check failed");
    let sessions = ok(&pmux(&socket, &root, &[], &["restart", "--plan", "--json"]));
    assert!(sessions.contains("\"work\""), "{sessions}");
    drop(server);
}

#[test]
fn session_configuration_check_rejects_new_sessions_before_restart() {
    let root = temp_root("cfg-new");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "work",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "later",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));

    let changed = pmux(
        &socket,
        &root,
        &[],
        &[
            "restart",
            "--daemon",
            "--stop-sessions",
            "--consented-config",
            file.to_str().unwrap(),
            "--worker",
            "--json",
        ],
    );
    fail(&changed, "session consent check failed");
    let sessions = ok(&pmux(&socket, &root, &[], &["restart", "--plan", "--json"]));
    assert!(sessions.contains("\"work\""), "{sessions}");
    assert!(sessions.contains("\"later\""), "{sessions}");
    drop(server);
}

#[test]
fn session_configuration_restart_stops_only_consented_sessions() {
    let root = temp_root("cfg-stop");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    ok(&pmux(
        &socket,
        &root,
        &[],
        &[
            "new",
            "--no-attach",
            "work",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    let file = root.join("configuration.json");
    ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "save", file.to_str().unwrap()],
    ));

    let output = pmux(
        &socket,
        &root,
        &[],
        &[
            "restart",
            "--daemon",
            "--stop-sessions",
            "--consented-config",
            file.to_str().unwrap(),
            "--delete-consented-config",
            "--worker",
            "--json",
        ],
    );
    let restarted = ok(&output);
    // A live update or a restart: either way the server comes back.
    assert!(
        restarted.contains("\"status\":\"restarted\"")
            || restarted.contains("\"status\":\"updated_live\""),
        "{restarted}"
    );
    // Without --progress, each step is a plain line with the same words the
    // app shows.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let steps: Vec<_> = stderr
        .lines()
        .filter(|line| line.ends_with(" of 3)"))
        .collect();
    assert_eq!(
        steps,
        [
            "Stopping the sessions (1 of 3)",
            "Starting the new daemon (2 of 3)",
            "Reconciling the official plug-ins (3 of 3)",
        ],
        "{stderr}"
    );
    assert!(
        !file.exists(),
        "restart without saving keeps no configuration"
    );
    let sessions = ok(&pmux(&socket, &root, &[], &["restart", "--plan", "--json"]));
    assert!(!sessions.contains("\"work\""), "{sessions}");
    ok(&pmux(&socket, &root, &[], &["stop"]));
    drop(server);
}

/// Save and restart: the restarted daemon brings the sessions back by itself,
/// with their IDs and the scrollback they had, and relaunches their commands.
#[test]
fn saved_restart_restores_sessions_with_their_ids_and_scrollback() {
    let root = temp_root("cfg-keep");
    // This test covers the restart that relaunches each program; keep the
    // sessions from being handed over live instead.
    std::fs::write(
        root.join("config").join("prismattyc.toml"),
        "[mux]\nlive_update = false\n",
    )
    .unwrap();
    let socket = socket_path(&root);
    let pane_log = root.join("pane-log.json");
    let env = [("PMUX_PANE_LOG", pane_log.to_str().unwrap())];
    let server = start_server_with_env(&socket, &root, &env);
    let pmux = |args: &[&str]| pmux(&socket, &root, &env, args);
    let created = ok(&pmux(&[
        "new",
        "--no-attach",
        "work",
        "/bin/sh",
        "-c",
        "printf 'BEFORE-%s\\n' $$; exec sleep 999",
    ]));
    // Each run prints its own process id, so the old line can only come
    // back from the saved scrollback, not from the relaunched command.
    let before = first_watched_match(&socket, &root, &env, "work", "BEFORE-", None);
    let file = root.join("configuration.json");
    ok(&pmux(&["session-config", "save", file.to_str().unwrap()]));

    let output = pmux(&[
        "restart",
        "--daemon",
        "--stop-sessions",
        "--consented-config",
        file.to_str().unwrap(),
        "--worker",
        "--json",
        "--progress",
    ]);
    let restarted = ok(&output);
    // Each step is reported on stderr as it starts, in order; stdout keeps
    // only the report.
    let steps: Vec<_> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(progress::parse)
        .collect();
    let expected: Vec<_> = [
        "Saving the sessions",
        "Stopping the old daemon",
        "Starting the new daemon",
        "Checking the restored sessions",
        "Reconciling the official plug-ins",
    ]
    .iter()
    .enumerate()
    .map(|(index, item)| progress::Progress::Restart {
        item: (*item).into(),
        index: index + 1,
        count: 5,
    })
    .collect();
    assert_eq!(steps, expected, "{}", text(&output));
    let report: serde_json::Value = serde_json::from_str(restarted.trim()).unwrap();
    let sessions = report["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["component"] == "sessions")
        .unwrap_or_else(|| panic!("no sessions result: {restarted}"));
    assert_eq!(sessions["status"], "restored", "{restarted}");
    assert!(
        sessions["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "work"),
        "{restarted}"
    );
    assert!(file.exists(), "the saved configuration stays available");

    let listed = ok(&pmux(&["ls"]));
    let id = created
        .split("(id ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_else(|| panic!("no session id in {created}"));
    assert!(
        listed.contains(&format!("session work (id {id})")),
        "the session keeps its id {id}: {listed}"
    );
    // The relaunched command prints a new line next to the restored one.
    first_watched_match(&socket, &root, &env, "work", "BEFORE-", Some(&before));
    let history = ok(&pmux(&["save-buffer", "work", "-", "--history"]));
    assert!(
        history.contains(&before),
        "the old output {before} must come back: {history}"
    );
    ok(&pmux(&["stop"]));
    drop(server);
}

/// The first `PREFIX…` word, other than `except`, that a watching attach
/// shows for SESSION. The attach prints one line per screen revision; this
/// waits for those lines, bounded by one deadline, and never polls.
fn first_watched_match(
    socket: &Path,
    root: &Path,
    env: &[(&str, &str)],
    session: &str,
    prefix: &str,
    except: Option<&str>,
) -> String {
    use std::io::BufRead;
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut watcher = prismattyc_process::spawn(
        command
            .arg("--socket")
            .arg(socket)
            .args(["attach", session, "--watch", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .expect("spawn watching attach");
    let stdout = watcher.stdout.take().unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let found = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(line) = received.recv_timeout(remaining) else {
            break None;
        };
        let word = line.match_indices(prefix).find_map(|(start, _)| {
            let word: String = line[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            (word.len() > prefix.len() && Some(word.as_str()) != except).then_some(word)
        });
        if word.is_some() {
            break word;
        }
    };
    let _ = watcher.kill();
    let _ = watcher.wait();
    let _ = reader.join();
    found.unwrap_or_else(|| panic!("{session} never showed {prefix}"))
}

#[test]
fn consented_restart_and_restore_keep_every_space_member() {
    let root = temp_root("cfg-space");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let pmux = |args: &[&str]| pmux(&socket, &root, &[], args);
    ok(&pmux(&[
        "space",
        "create",
        "desk",
        "--session-name",
        "alpha",
        "--no-attach",
    ]));
    ok(&pmux(&["space", "add", "desk", "--name", "beta"]));
    let listed = ok(&pmux(&["space", "ls"]));
    assert!(listed.contains("desk  2 sessions"), "{listed}");

    // New shells settle for a moment (title, directory), and the restart
    // refuses a configuration the live sessions no longer match. Save until
    // a check right after the save agrees.
    let file = root.join("configuration.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let _ = std::fs::remove_file(&file);
        ok(&pmux(&["session-config", "save", file.to_str().unwrap()]));
        std::thread::sleep(Duration::from_millis(500));
        if pmux(&["session-config", "check", file.to_str().unwrap()])
            .status
            .success()
        {
            break;
        }
        assert!(Instant::now() < deadline, "the sessions never settled");
    }
    let restarted = ok(&pmux(&[
        "restart",
        "--daemon",
        "--stop-sessions",
        "--consented-config",
        file.to_str().unwrap(),
        "--worker",
        "--json",
    ]));
    // A live update or a restart: either way the server comes back.
    assert!(
        restarted.contains("\"status\":\"restarted\"")
            || restarted.contains("\"status\":\"updated_live\""),
        "{restarted}"
    );
    ok(&pmux(&[
        "session-config",
        "restore",
        file.to_str().unwrap(),
    ]));
    // Several autosave ticks on the new daemon, after the restore.
    std::thread::sleep(Duration::from_secs(2));

    let listed = ok(&pmux(&["space", "ls"]));
    assert!(
        listed.contains("desk  2 sessions"),
        "a restart must not empty the Space: {listed}"
    );
    let reopened = pmux(&["space", "open", "desk", "--replace", "--no-run"]);
    assert!(reopened.status.success(), "{}", text(&reopened));
    let sessions = ok(&pmux(&["restart", "--plan", "--json"]));
    assert!(sessions.contains("\"alpha\""), "{sessions}");
    assert!(sessions.contains("\"beta\""), "{sessions}");
    ok(&pmux(&["stop"]));
    drop(server);
}

/// One saved command with a line break must not abort the restore. The seat
/// is reported and skipped; every session still comes back and the other
/// seat's command still runs.
#[test]
fn restore_skips_a_saved_command_with_a_line_break_and_restores_the_rest() {
    let root = temp_root("cfg-linebreak");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let mut dirs = Vec::new();
    for name in ["good", "bad"] {
        let cwd = root.join(name);
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let mut create = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmux"));
        clear_command_env(&mut create);
        apply_isolation(&mut create, &root);
        ok(&prismattyc_process::spawn(
            create.current_dir(&cwd).arg("--socket").arg(&socket).args([
                "new",
                "--no-attach",
                name,
            ]),
        )
        .and_then(std::process::Child::wait_with_output)
        .unwrap());
        dirs.push(cwd);
    }
    let file = root.join("configuration.json");
    let (done, signal) = completion_signal(&root);
    ok(&pmux(
        &socket,
        &root,
        &[(
            prismattyc_mux::procinfo::TEST_FOREGROUND_COMMAND_ENV,
            &format!("printf ran >> launches && {done}"),
        )],
        &["session-config", "save", file.to_str().unwrap()],
    ));
    // Make the bad seat's saved command a two-line wrapper, as a captured
    // child process can be.
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for session in saved["space"]["sessions"].as_array_mut().unwrap() {
        if session["name"] == "bad" {
            session["windows"][0]["root"]["command"] =
                "printf ran >> launches\nprintf second >> launches".into();
        }
    }
    std::fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();
    ok(&pmux(&socket, &root, &[], &["stop", "good"]));
    ok(&pmux(&socket, &root, &[], &["stop", "bad"]));
    let restored = ok(&pmux(
        &socket,
        &root,
        &[],
        &["session-config", "restore", file.to_str().unwrap()],
    ));
    assert!(restored.contains("Restored 2 sessions."), "{restored}");
    assert!(
        restored.contains("Skipped the saved command for session bad"),
        "{restored}"
    );
    let listing = ok(&pmux(&socket, &root, &[], &["ls"]));
    assert!(
        listing.contains("session good") && listing.contains("session bad"),
        "{listing}"
    );
    await_completion(&signal, 8);
    assert_eq!(
        std::fs::read_to_string(dirs[0].join("launches")).unwrap(),
        "ran"
    );
    // Give a wrongly replayed command time to land before asserting it did not.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !dirs[1].join("launches").exists(),
        "the line-break command must not run"
    );
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn mkfifo(path: &Path) {
    let status = prismattyc_process::status(Command::new("mkfifo").arg(path)).unwrap();
    assert!(status.success(), "mkfifo {} failed", path.display());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_fifo_line(path: &Path, label: &str) -> String {
    use std::io::{BufRead, BufReader};
    let reader_path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        if let Ok(file) = std::fs::File::open(&reader_path) {
            let mut line = String::new();
            let _ = BufReader::new(file).read_line(&mut line);
            let _ = tx.send(line);
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("timed out waiting for FIFO {label}"));
    let _ = handle.join();
    line
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_pane_command_line(socket: &Path, session_name: &str, command_line: &str) {
    use prismattyc_mux::{
        ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, PROTOCOL_VERSION,
    };
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket).expect("connect control socket");
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut resp_line = String::new();

    let reg_req = serde_json::to_string(&ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id: 1,
    })
    .unwrap();
    writeln!(stream, "{reg_req}").unwrap();
    stream.flush().unwrap();
    reader.read_line(&mut resp_line).unwrap();
    let reg_resp: ControlResponse = serde_json::from_str(&resp_line).unwrap();
    let ControlResponseBody::Ok {
        response: ControlResponseData::ClientRegistered { client_id },
    } = reg_resp.body
    else {
        panic!("unexpected register response: {resp_line}");
    };
    resp_line.clear();

    let snap_req = serde_json::to_string(&ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id: 2,
    })
    .unwrap();
    writeln!(stream, "{snap_req}").unwrap();
    stream.flush().unwrap();

    reader.read_line(&mut resp_line).unwrap();
    let resp: ControlResponse = serde_json::from_str(&resp_line).unwrap();
    let ControlResponseBody::Ok {
        response: ControlResponseData::Snapshot { snapshot },
    } = resp.body
    else {
        panic!("unexpected snapshot response: {resp_line}");
    };
    let pane_id = snapshot
        .sessions
        .iter()
        .find(|s| s.name == session_name)
        .and_then(|s| s.windows.first())
        .and_then(|w| w.panes.first())
        .map(|p| p.id)
        .unwrap_or_else(|| panic!("session {session_name} pane missing"));

    let write_req = serde_json::to_string(&ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id: 3,
        client_id,
        pane_id,
        data: format!("{command_line}\r"),
    })
    .unwrap();
    writeln!(stream, "{write_req}").unwrap();
    stream.flush().unwrap();
    resp_line.clear();
    reader.read_line(&mut resp_line).unwrap();
    let ack: ControlResponse = serde_json::from_str(&resp_line).unwrap();
    assert!(
        matches!(ack.body, ControlResponseBody::Ok { .. }),
        "write_pane failed: {resp_line}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn session_config_check_and_consented_restart_ignore_transient_agent_children() {
    let root = temp_root("cfg-transient-child");
    // The agents must be resumed by the restart, not handed over live.
    std::fs::write(
        root.join("config").join("prismattyc.toml"),
        "[mux]\nlive_update = false\n",
    )
    .unwrap();
    let socket = socket_path(&root);
    let bin = root.join("bin");
    let state = root.join("state");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let ready_fifo = root.join("ready.fifo");
    let resumed_fifo = root.join("resumed.fifo");
    mkfifo(&ready_fifo);
    mkfifo(&resumed_fifo);

    let claude_bin = bin.join("claude");
    let mcp_bin = bin.join("sc-mcp");
    let caff_bin = bin.join("caffeinate");
    let holder_sh = state.join("holder.sh");
    write_executable(&holder_sh, "#!/bin/sh\nwhile read _; do :; done\n");

    let phase2_marker = state.join("phase2");
    let resumed_log = state.join("resumed.log");
    write_executable(
        &claude_bin,
        &format!(
            "#!/bin/sh\n\
             if [ -f '{}' ]; then\n\
               printf '%s\\n' \"$*\" > '{}'\n\
               echo resumed > '{}'\n\
               while read _; do :; done\n\
             fi\n\
             /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" < /dev/null' _ '{}' '{}' &\n\
             /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" < /dev/null' _ '{}' '{}' &\n\
             echo ready > '{}'\n\
             while read _; do :; done\n",
            phase2_marker.display(),
            resumed_log.display(),
            resumed_fifo.display(),
            mcp_bin.display(),
            holder_sh.display(),
            caff_bin.display(),
            holder_sh.display(),
            ready_fifo.display(),
        ),
    );
    let launch_sh = state.join("launch.sh");
    write_executable(
        &launch_sh,
        &format!(
            "#!/bin/sh\n\
             exec /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" --resume sess-exact-1 --model model-a' _ '{}' '{}'\n",
            claude_bin.display(),
            claude_bin.display(),
        ),
    );

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".into())
    );
    let extra = [
        ("PATH", path_env.as_str()),
        ("PRISMATTYC_NO_LOGIN_SERVICE", "1"),
    ];
    let server = start_server_with_env(&socket, &root, &extra);

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "new",
            "--no-attach",
            "work",
            "--",
            "/bin/sh",
            launch_sh.to_str().unwrap(),
        ],
    ));
    assert_eq!(wait_fifo_line(&ready_fifo, "claude ready").trim(), "ready");

    let cfg1 = root.join("cfg1.json");
    let cfg2 = root.join("cfg2.json");
    ok(&pmux(
        &socket,
        &root,
        &extra,
        &["session-config", "save", cfg1.to_str().unwrap()],
    ));
    ok(&pmux(
        &socket,
        &root,
        &extra,
        &["session-config", "save", cfg2.to_str().unwrap()],
    ));
    let saved_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cfg1).unwrap()).unwrap();
    let saved_json2: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cfg2).unwrap()).unwrap();
    assert_eq!(
        saved_json["space"]["sessions"], saved_json2["space"]["sessions"],
        "repeated session-config save must not flap on transient child processes"
    );
    assert_eq!(saved_json["session_ids"], saved_json2["session_ids"]);
    let work = saved_json["space"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "work")
        .unwrap();
    let saved_cmd = work["windows"][0]["root"]["command"].as_str().unwrap();
    assert!(
        saved_cmd.contains("--resume sess-exact-1 --model model-a"),
        "expected claude resume cmd, got {saved_cmd}"
    );
    assert!(
        !saved_cmd.contains("sc-mcp") && !saved_cmd.contains("caffeinate"),
        "transient child leaked into saved command: {saved_cmd}"
    );

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &["session-config", "check", cfg1.to_str().unwrap()],
    ));

    std::fs::write(&phase2_marker, b"1").unwrap();
    let restarted = ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "restart",
            "--daemon",
            "--stop-sessions",
            "--consented-config",
            cfg1.to_str().unwrap(),
            "--worker",
            "--json",
        ],
    ));
    // A live update or a restart: either way the server comes back.
    assert!(
        restarted.contains("\"status\":\"restarted\"")
            || restarted.contains("\"status\":\"updated_live\""),
        "{restarted}"
    );

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &["session-config", "restore", cfg1.to_str().unwrap()],
    ));
    assert_eq!(
        wait_fifo_line(&resumed_fifo, "claude resumed").trim(),
        "resumed"
    );
    let resumed_args = std::fs::read_to_string(&resumed_log).unwrap();
    assert_eq!(resumed_args.trim(), "--resume sess-exact-1 --model model-a");

    ok(&pmux(&socket, &root, &extra, &["stop"]));
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn plain_saved_restart_resumes_exact_conversations_and_monitor_socket_ready() {
    let root = temp_root("plain-exact-resume");
    let socket = socket_path(&root);
    let config_path = root.join("config/prismattyc.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "start_at_login = true\n[mux]\nspace_open_runs_commands = \"all\"\n",
    )
    .unwrap();

    let bin = root.join("bin");
    let state = root.join("state");
    let codex_home = root.join("home/.codex/sessions/2026/11/21");
    let claude_sessions = root.join("home/.claude/sessions");
    let grok_sessions = root.join("home/.grok/sessions/grok-exact-33");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&claude_sessions).unwrap();
    std::fs::create_dir_all(&grok_sessions).unwrap();

    let main_id = "22222222-2222-2222-2222-222222222222";
    let sub_id = "11111111-1111-1111-1111-111111111111";
    let sub_rollout = codex_home.join(format!("rollout-2026-11-21T00-00-00-{sub_id}.jsonl"));
    let main_rollout = codex_home.join(format!("rollout-2026-11-21T00-00-00-{main_id}.jsonl"));
    std::fs::write(
        &sub_rollout,
        format!(
            r#"{{"payload":{{"id":"{sub_id}","parent_thread_id":"{main_id}","thread_source":"subagent","source":{{"subagent":"x"}}}}}}"#
        ) + "\n",
    )
    .unwrap();
    std::fs::write(
        &main_rollout,
        format!(r#"{{"payload":{{"id":"{main_id}","source":"cli","thread_source":"user"}}}}"#)
            + "\n",
    )
    .unwrap();
    let grok_events = grok_sessions.join("events.jsonl");
    std::fs::write(&grok_events, "{}\n").unwrap();

    let phase2_marker = state.join("phase2");
    let claude_launches = state.join("claude.launches");
    let claude_resumed_log = state.join("claude.resumed.log");
    let codex_resumed_log = state.join("codex.resumed.log");
    let grok_resumed_log = state.join("grok.resumed.log");
    let monitor_resumed_log = state.join("monitor.resumed.log");

    let claude_r1 = state.join("claude-r1.fifo");
    let codex_r1 = state.join("codex-r1.fifo");
    let grok_r1 = state.join("grok-r1.fifo");
    let monitor_r1 = state.join("monitor-r1.fifo");
    let claude_r2 = state.join("claude-r2.fifo");
    let codex_r2 = state.join("codex-r2.fifo");
    let grok_r2 = state.join("grok-r2.fifo");
    let monitor_r2 = state.join("monitor-r2.fifo");
    for f in [
        &claude_r1,
        &codex_r1,
        &grok_r1,
        &monitor_r1,
        &claude_r2,
        &codex_r2,
        &grok_r2,
        &monitor_r2,
    ] {
        mkfifo(f);
    }

    let claude_bin = bin.join("claude");
    write_executable(
        &claude_bin,
        &format!(
            "#!/bin/sh\n\
             if [ -f '{}' ]; then\n\
               printf '%s\\n' \"$*\" > '{}'\n\
               echo ok > '{}'\n\
               while read _; do :; done\n\
             fi\n\
             printf '{{\"pid\":%s,\"sessionId\":\"claude-exact-11\"}}\\n' \"$$\" > '{}/'\"$$.json\"\n\
             echo ready > '{}'\n\
             while read _; do :; done\n",
            phase2_marker.display(),
            claude_resumed_log.display(),
            claude_r2.display(),
            claude_sessions.display(),
            claude_r1.display(),
        ),
    );
    let claude_launch_sh = state.join("launch-claude.sh");
    write_executable(
        &claude_launch_sh,
        &format!(
            "#!/bin/sh\n\
             printf 'ONCE\\n' >> '{}'\n\
             exec /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" --model model-a \"Initial claude prompt\"' _ '{}' '{}'\n",
            claude_launches.display(),
            claude_bin.display(),
            claude_bin.display(),
        ),
    );

    let codex_bin = bin.join("codex");
    write_executable(
        &codex_bin,
        &format!(
            "#!/bin/sh\n\
             if [ -f '{}' ]; then\n\
               printf '%s\\n' \"$*\" > '{}'\n\
               echo ok > '{}'\n\
               while read _; do :; done\n\
             fi\n\
             exec 3<'{}' 4<'{}'\n\
             echo ready > '{}'\n\
             while read _; do :; done\n",
            phase2_marker.display(),
            codex_resumed_log.display(),
            codex_r2.display(),
            sub_rollout.display(),
            main_rollout.display(),
            codex_r1.display(),
        ),
    );
    let codex_launch_sh = state.join("launch-codex.sh");
    write_executable(
        &codex_launch_sh,
        &format!(
            "#!/bin/sh\n\
             exec /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" --model model-b \"Read prompt.txt\"' _ '{}' '{}'\n",
            codex_bin.display(),
            codex_bin.display(),
        ),
    );

    let grok_bin = bin.join("grok");
    write_executable(
        &grok_bin,
        &format!(
            "#!/bin/sh\n\
             if [ -f '{}' ]; then\n\
               printf '%s\\n' \"$*\" > '{}'\n\
               echo ok > '{}'\n\
               while read _; do :; done\n\
             fi\n\
             exec 3<'{}'\n\
             echo ready > '{}'\n\
             while read _; do :; done\n",
            phase2_marker.display(),
            grok_resumed_log.display(),
            grok_r2.display(),
            grok_events.display(),
            grok_r1.display(),
        ),
    );
    let grok_launch_sh = state.join("launch-grok.sh");
    write_executable(
        &grok_launch_sh,
        &format!(
            "#!/bin/sh\n\
             exec /bin/bash -c 'exec -a \"$1\" /bin/sh \"$2\" --resume grok-exact-33 --model model-c -p \"Initial grok prompt\"' _ '{}' '{}'\n",
            grok_bin.display(),
            grok_bin.display(),
        ),
    );

    let monitor_probe = bin.join("monitor-probe");
    write_executable(
        &monitor_probe,
        &format!(
            "#!/bin/sh\n\
             if ! '{}' --socket '{}' ls >/dev/null 2>&1; then\n\
               exit 22\n\
             fi\n\
             if [ -f '{}' ]; then\n\
               echo CONNECTED > '{}'\n\
               echo ok > '{}'\n\
             else\n\
               echo ready > '{}'\n\
             fi\n\
             while read _; do :; done\n",
            env!("CARGO_BIN_EXE_pmux"),
            socket.display(),
            phase2_marker.display(),
            monitor_resumed_log.display(),
            monitor_r2.display(),
            monitor_r1.display(),
        ),
    );

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".into())
    );
    let extra = [
        ("PATH", path_env.as_str()),
        ("PRISMATTYC_NO_LOGIN_SERVICE", "1"),
    ];
    let server = start_server_with_env(&socket, &root, &extra);

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "new",
            "--no-attach",
            "claude-seat",
            "--",
            "/bin/sh",
            claude_launch_sh.to_str().unwrap(),
        ],
    ));
    assert_eq!(wait_fifo_line(&claude_r1, "claude r1").trim(), "ready");

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "new",
            "--no-attach",
            "codex-seat",
            "--",
            "/bin/sh",
            codex_launch_sh.to_str().unwrap(),
        ],
    ));
    assert_eq!(wait_fifo_line(&codex_r1, "codex r1").trim(), "ready");

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "new",
            "--no-attach",
            "grok-seat",
            "--",
            "/bin/sh",
            grok_launch_sh.to_str().unwrap(),
        ],
    ));
    assert_eq!(wait_fifo_line(&grok_r1, "grok r1").trim(), "ready");

    ok(&pmux(
        &socket,
        &root,
        &extra,
        &["new", "--no-attach", "monitor-seat"],
    ));
    write_pane_command_line(
        &socket,
        "monitor-seat",
        &format!(
            "/bin/bash -c 'exec -a \"{0}\" /bin/sh \"{0}\"'",
            monitor_probe.display()
        ),
    );
    assert_eq!(wait_fifo_line(&monitor_r1, "monitor r1").trim(), "ready");

    std::fs::write(&phase2_marker, b"1").unwrap();
    let restarted = ok(&pmux(
        &socket,
        &root,
        &extra,
        &[
            "restart",
            "--daemon",
            "--stop-sessions",
            "--worker",
            "--json",
        ],
    ));
    // A live update or a restart: either way the server comes back.
    assert!(
        restarted.contains("\"status\":\"restarted\"")
            || restarted.contains("\"status\":\"updated_live\""),
        "{restarted}"
    );

    assert_eq!(wait_fifo_line(&claude_r2, "claude r2").trim(), "ok");
    assert_eq!(wait_fifo_line(&codex_r2, "codex r2").trim(), "ok");
    assert_eq!(wait_fifo_line(&grok_r2, "grok r2").trim(), "ok");
    assert_eq!(wait_fifo_line(&monitor_r2, "monitor r2").trim(), "ok");

    assert_eq!(
        std::fs::read_to_string(&claude_launches).unwrap(),
        "ONCE\n",
        "launch script must not replay on plain restart"
    );
    assert_eq!(
        std::fs::read_to_string(&claude_resumed_log).unwrap().trim(),
        "--resume claude-exact-11 --model model-a"
    );
    assert_eq!(
        std::fs::read_to_string(&codex_resumed_log).unwrap().trim(),
        format!("resume {main_id} --model model-b")
    );
    assert_eq!(
        std::fs::read_to_string(&grok_resumed_log).unwrap().trim(),
        "--resume grok-exact-33 --model model-c"
    );
    assert_eq!(
        std::fs::read_to_string(&monitor_resumed_log)
            .unwrap()
            .trim(),
        "CONNECTED"
    );

    ok(&pmux(&socket, &root, &extra, &["stop"]));
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
