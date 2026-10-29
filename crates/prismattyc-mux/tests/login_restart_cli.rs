//! `pmux restart` and `pmux login` against a private socket and config.
//! macOS login registration is not loaded here: that path calls launchctl.

#![cfg(unix)]

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

struct ServerGuard {
    child: Child,
    socket: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn start_server(socket: &Path, root: &Path) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    apply_isolation(&mut command, root);
    let guard = ServerGuard {
        child: command
            .arg("--socket")
            .arg(socket)
            .args(["--", "/bin/sh", "-c", "exec sleep 999"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pmuxd"),
        socket: socket.to_path_buf(),
    };
    for _ in 0..100 {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not publish {}", socket.display());
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
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
    command.output().expect("run pmux")
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
    assert!(
        restarted.contains("\"status\":\"restarted\""),
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
    while Instant::now() < deadline {
        let status = pmux(&server_socket, &root, &[], &["status"]);
        if status.status.success() && text(&status).contains("running") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
fn session_configuration_restores_names_directories_and_commands_without_replay() {
    let root = temp_root("session-configuration");
    let socket = socket_path(&root);
    let server = start_server(&socket, &root);
    let cwd = root.join("project");
    std::fs::create_dir(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();
    let mut create = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut create);
    apply_isolation(&mut create, &root);
    ok(&create
        .current_dir(&cwd)
        .arg("--socket")
        .arg(&socket)
        .args(["new", "--no-attach", "work"])
        .output()
        .unwrap());
    let file = root.join("configuration.json");
    let launch = "printf restored >> launches".to_string();
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
    let deadline = Instant::now() + Duration::from_secs(8);
    while !cwd.join("launches").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
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

    let restarted = ok(&pmux(
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
    ));
    assert!(
        restarted.contains("\"status\":\"restarted\""),
        "{restarted}"
    );
    let sessions = ok(&pmux(&socket, &root, &[], &["restart", "--plan", "--json"]));
    assert!(!sessions.contains("\"work\""), "{sessions}");
    ok(&pmux(&socket, &root, &[], &["stop"]));
    drop(server);
}
