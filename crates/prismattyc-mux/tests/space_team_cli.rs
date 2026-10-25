//! `pmux space` team and template commands against a private daemon.
//! Space files stay under this test's `XDG_DATA_HOME`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prismattyc_mux::space_team::TeamClient;

mod support;
use support::clear_command_env;

fn socket_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "pmux-team-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn data_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-team-data-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct ServerGuard {
    child: Child,
    socket: PathBuf,
    data: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn start_server(socket: &Path, data: &Path) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    let guard = ServerGuard {
        child: command
            .arg("--socket")
            .arg(socket)
            .args(["--", "/bin/sh", "-c", "exec sleep 999"])
            .env("SHELL", "/bin/sh")
            .env("HOME", data)
            .env("XDG_CONFIG_HOME", data.join("config"))
            .env("XDG_DATA_HOME", data)
            .env("PMUX_SESSION_AGENTS", data.join("session-agents.json"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pmuxd"),
        socket: socket.to_path_buf(),
        data: data.to_path_buf(),
    };
    for _ in 0..100 {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not publish {}", socket.display());
}

fn pmux(socket: &Path, data: &Path, args: &[&str]) -> Output {
    pmux_env(socket, data, &[], args)
}

fn pmux_env(socket: &Path, data: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    command
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env("SHELL", "/bin/sh")
        .env("HOME", data)
        .env("XDG_CONFIG_HOME", data.join("config"))
        .env("XDG_DATA_HOME", data)
        .env("PMUX_SESSION_AGENTS", data.join("session-agents.json"))
        .env_remove("PMUX_AGENT")
        .env_remove("PRISMATTYC_PANE_ID")
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

fn find_named(root: &Path, name: &str) -> PathBuf {
    let file = format!("{name}.json");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|s| s.to_str()) == Some(file.as_str()) {
                return path;
            }
        }
    }
    panic!("missing {file} under {}", root.display());
}

#[test]
fn team_and_template_commands_change_the_space() {
    let socket = socket_path();
    let data = data_dir();
    let mut server = start_server(&socket, &data);

    let created = ok(&pmux(
        &socket,
        &data,
        &["new", "--no-attach", "seat-a", "--", "/bin/sh"],
    ));
    assert!(created.contains("seat-a"), "{created}");

    let saved = ok(&pmux(&socket, &data, &["space", "save", "src"]));
    assert!(saved.contains("src.json"), "{saved}");
    let space_path = find_named(&data, "src");
    let space: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&space_path).unwrap()).unwrap();
    let id = space["id"]
        .as_str()
        .expect("saved space has an id")
        .to_string();

    fail(
        &pmux(&socket, &data, &["space", "undo"]),
        "usage: pmux space undo RECEIPT",
    );
    fail(
        &pmux(&socket, &data, &["space", "undo", "a", "b"]),
        "usage: pmux space undo RECEIPT",
    );
    fail(
        &pmux(&socket, &data, &["space", "undo", "/no/such-receipt"]),
        "No such file or directory",
    );

    let empty = ok(&pmux(&socket, &data, &["space", "template", "ls"]));
    assert!(!empty.contains("kit"), "{empty}");
    fail(
        &pmux(&socket, &data, &["space", "template"]),
        "usage: pmux space template",
    );
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "template", "save", "ghost", "--space", "missing"],
        ),
        "missing",
    );

    let listed = ok(&pmux(&socket, &data, &["space", "result", "src"]));
    assert!(
        listed.contains("No open result recorded for src"),
        "{listed}"
    );
    let empty_json = ok(&pmux(&socket, &data, &["space", "result", "src", "--json"]));
    assert!(empty_json.contains("[]"), "{empty_json}");

    let results = space_path.parent().unwrap().join("team-results").join(&id);
    std::fs::create_dir_all(&results).unwrap();
    std::fs::write(
        results.join("0001.json"),
        r#"{"name":"src","view":"applied","launch":"not observed","recorded_at_ms":2,"sequence":1}"#,
    )
    .unwrap();
    let recorded = ok(&pmux(&socket, &data, &["space", "result", "src"]));
    assert!(recorded.contains("src: applied"), "{recorded}");
    assert!(recorded.contains("Launch: not observed"), "{recorded}");
    let recorded_json = ok(&pmux(&socket, &data, &["space", "result", "src", "--json"]));
    assert!(recorded_json.contains("\"applied\""), "{recorded_json}");

    let details = ok(&pmux(&socket, &data, &["space", "details", "src"]));
    assert!(details.contains("src —"), "{details}");
    assert!(details.contains("seat-a"), "{details}");
    let details_json = ok(&pmux(
        &socket,
        &data,
        &["space", "details", "src", "--json"],
    ));
    assert!(details_json.contains("\"seat-a\""), "{details_json}");
    let attention = ok(&pmux(&socket, &data, &["space", "attention", "src"]));
    assert!(attention.contains("seat-a"), "{attention}");
    let attention_json = ok(&pmux(
        &socket,
        &data,
        &["space", "attention", "src", "--json"],
    ));
    assert!(attention_json.contains("\"space_id\""), "{attention_json}");

    let role = ok(&pmux(
        &socket,
        &data,
        &["space", "role", "src", "seat-a", "Design"],
    ));
    assert!(role.contains("seat-a: Design"), "{role}");
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "role", "src", "missing", "Design"],
        ),
        "session is not saved in this Space",
    );
    fail(
        &pmux(&socket, &data, &["space", "role", "src", "seat-a", " "]),
        "printable characters",
    );
    let link = ok(&pmux(
        &socket,
        &data,
        &[
            "space",
            "link",
            "src",
            "Design",
            "https://example.com/design",
        ],
    ));
    assert!(
        link.contains("Design: https://example.com/design"),
        "{link}"
    );
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "link", "src", "Docs", "relative/path"],
        ),
        "HTTP(S) link",
    );
    let linked = ok(&pmux(&socket, &data, &["space", "details", "src"]));
    assert!(
        linked.contains("Design: https://example.com/design"),
        "{linked}"
    );

    fail(
        &pmux(&socket, &data, &["space", "role"]),
        "usage: pmux space details",
    );
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "attention", "resolve", "src", "missing", "1", "1"],
        ),
        "session is not saved in this Space",
    );
    fail(
        &pmux(
            &socket,
            &data,
            &[
                "space",
                "attention",
                "resolve",
                "src",
                "seat-a",
                "nope",
                "1",
            ],
        ),
        "invalid digit",
    );

    let raised = ok(&pmux(
        &socket,
        &data,
        &["attention", "seat-a", "review the diff"],
    ));
    assert!(raised.contains("attention sent"), "{raised}");
    let report: serde_json::Value = serde_json::from_str(&ok(&pmux(
        &socket,
        &data,
        &["space", "details", "src", "--json"],
    )))
    .unwrap();
    let request = report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "seat-a")
        .expect("seat-a")["attention"]
        .as_array()
        .expect("attention list");
    assert!(
        !request.is_empty(),
        "raised attention is on the seat: {report}"
    );
    let pane = request[0]["pane_id"].as_u64().unwrap().to_string();
    let revision = request[0]["revision"].as_u64().unwrap().to_string();
    fail(
        &pmux(
            &socket,
            &data,
            &[
                "space",
                "attention",
                "resolve",
                "src",
                "seat-a",
                &pane,
                "99999",
            ],
        ),
        "attention or ownership changed",
    );
    let resolved = ok(&pmux(
        &socket,
        &data,
        &[
            "space",
            "attention",
            "resolve",
            "src",
            "seat-a",
            &pane,
            &revision,
        ],
    ));
    assert!(
        resolved.contains("attention updated; mail unchanged"),
        "{resolved}"
    );

    let requests = TeamClient::connect(&socket).unwrap().requests().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.pane_id.to_string() != pane),
        "resolved attention survived in daemon state: {requests:?}"
    );

    ok(&pmux(
        &socket,
        &data,
        &["attention", "seat-a", "check later"],
    ));
    let again: serde_json::Value = serde_json::from_str(&ok(&pmux(
        &socket,
        &data,
        &["space", "details", "src", "--json"],
    )))
    .unwrap();
    let request = again["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == "seat-a")
        .unwrap()["attention"]
        .as_array()
        .unwrap();
    assert!(
        !request.is_empty(),
        "second attention is on the seat: {again}"
    );
    let pane = request[0]["pane_id"].as_u64().unwrap().to_string();
    let revision = request[0]["revision"].as_u64().unwrap().to_string();
    let before_snooze = unix_ms();
    let snoozed = ok(&pmux(
        &socket,
        &data,
        &[
            "space",
            "attention",
            "snooze",
            "src",
            "seat-a",
            &pane,
            &revision,
        ],
    ));
    assert!(snoozed.contains("attention updated"), "{snoozed}");
    let after_snooze = unix_ms();
    let requests = TeamClient::connect(&socket).unwrap().requests().unwrap();
    let retained = requests
        .iter()
        .find(|request| request.pane_id.to_string() == pane)
        .expect("snooze must retain the request, unlike resolve");
    assert_eq!(retained.message, "check later");
    assert_eq!(retained.revision.to_string(), revision);
    assert!(retained.snoozed_until_ms >= before_snooze + 600_000);
    assert!(retained.snoozed_until_ms <= after_snooze + 600_000);
    assert!(retained.snoozed_until_ms > after_snooze);

    let saved_template = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "save", "kit", "--space", "src"],
    ));
    assert!(saved_template.contains("kit.json"), "{saved_template}");
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "template", "save", "kit", "--space", "src"],
        ),
        "already exists",
    );
    let names = ok(&pmux(&socket, &data, &["space", "template", "ls"]));
    assert!(names.contains("kit"), "{names}");

    let preview = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "preview", "kit", "fresh"],
    ));
    assert!(preview.contains("Create fresh with"), "{preview}");
    assert!(
        preview.contains("Preview only. No sessions created"),
        "{preview}"
    );
    let preview_json = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "preview", "kit", "fresh", "--json"],
    ));
    assert!(preview_json.contains("\"destination\""), "{preview_json}");
    let conflict = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "preview", "kit", "src"],
    ));
    assert!(
        conflict.contains("Conflict: Space src already exists"),
        "{conflict}"
    );

    fail(
        &pmux(
            &socket,
            &data,
            &["space", "template", "create", "kit", "bad", "--bogus"],
        ),
        "usage: pmux space template create",
    );
    let prepared = ok(&pmux(
        &socket,
        &data,
        &[
            "space",
            "template",
            "create",
            "kit",
            "prepared",
            "--prepare-only",
        ],
    ));
    assert!(
        prepared.contains("shells only; saved commands not started"),
        "{prepared}"
    );
    assert!(find_named(&data, "prepared").is_file());
    let quiet = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "create", "kit", "quiet", "--no-attach"],
    ));
    assert!(quiet.contains("shells only"), "{quiet}");
    assert!(!quiet.contains("opened host"), "{quiet}");
    // Attach would exec prismattyc-host. A no-op binary keeps that off this machine.
    let host = if Path::new("/usr/bin/true").is_file() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let shown = ok(&pmux_env(
        &socket,
        &data,
        &[("PRISMATTYC_HOST", host)],
        &["space", "template", "create", "kit", "shown"],
    ));
    // macOS spawns the host. Linux without a display prints the attach recipe.
    assert!(
        shown.contains("opened host") || shown.contains("pmux attach shown-1"),
        "{shown}"
    );

    let _ = server.child.kill();
    let _ = server.child.wait();
    let offline = ok(&pmux(
        &socket,
        &data,
        &["space", "template", "preview", "kit", "later"],
    ));
    assert!(offline.contains("Daemon unavailable"), "{offline}");
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fixture state"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn launch_template(socket: &Path, data: &Path) -> PathBuf {
    ok(&pmux(
        socket,
        data,
        &["new", "--no-attach", "origin", "--", "/bin/sh"],
    ));
    ok(&pmux(socket, data, &["space", "save", "source", "origin"]));
    ok(&pmux(
        socket,
        data,
        &[
            "space",
            "template",
            "save",
            "launch-kit",
            "--space",
            "source",
        ],
    ));
    let template_path = find_named(data, "launch-kit");
    let mut template: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&template_path).unwrap()).unwrap();
    assert_eq!(
        template["definition"]["sessions"].as_array().unwrap().len(),
        1
    );
    let marker = data.join("launched.txt");
    let root = &mut template["definition"]["sessions"][0]["windows"][0]["root"];
    assert_eq!(root["kind"], "leaf");
    root["program"] = serde_json::Value::Null;
    root["command"] = format!("printf 'template-executed' > '{}'", marker.display()).into();
    std::fs::write(&template_path, serde_json::to_vec(&template).unwrap()).unwrap();
    assert!(!marker.exists());
    marker
}

#[test]
fn template_launch_runs_the_saved_command() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);
    let marker = launch_template(&socket, &data);
    let output = pmux(
        &socket,
        &data,
        &[
            "space",
            "template",
            "create",
            "launch-kit",
            "destination",
            "--launch",
            "--prepare-only",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    wait_for(|| marker.exists());
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "template-executed"
    );
}

#[test]
fn template_launch_refuses_occupied_pane_without_replacing_work() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);
    let marker = launch_template(&socket, &data);
    let occupied_pid = data.join("occupied.pid");
    let shell = data.join("occupied-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec sleep 60\n",
            occupied_pid.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = pmux_env(
        &socket,
        &data,
        &[("SHELL", shell.to_str().unwrap())],
        &[
            "space",
            "template",
            "create",
            "launch-kit",
            "destination",
            "--launch",
            "--prepare-only",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("already has foreground work; launch was not repeated"));
    assert!(!marker.exists(), "refused launch ran the saved command");
    let pid: u32 = std::fs::read_to_string(occupied_pid)
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    let current = &snapshot
        .sessions
        .iter()
        .find(|s| s.name == "destination-1")
        .unwrap()
        .windows[0]
        .panes[0];
    assert_eq!(current.child_pid, Some(pid));
    assert!(prismattyc_mux::procinfo::pid_alive(pid));
    assert!(prismattyc_mux::procinfo::live_foreground_command(pid)
        .is_some_and(|command| command.contains("sleep 60")));
}

#[test]
fn reset_saved_spaces_backs_up_bytes_and_preserves_live_sessions() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);
    for (space, session) in [("alpha", "reset-a"), ("beta", "reset-b")] {
        ok(&pmux(
            &socket,
            &data,
            &[
                "space",
                "create",
                space,
                "--session-name",
                session,
                "--no-attach",
            ],
        ));
    }
    let alpha = find_named(&data, "alpha");
    let beta = find_named(&data, "beta");
    let mut broken: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&alpha).unwrap()).unwrap();
    broken["sessions"][0]["windows"] = serde_json::json!([]);
    std::fs::write(&alpha, serde_json::to_vec(&broken).unwrap()).unwrap();
    let original = std::fs::read(&alpha).unwrap();
    let before = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    ok(&pmux(
        &socket,
        &data,
        &[
            "mail",
            "--as",
            "reset-a",
            "send",
            "reset-a",
            "--summary",
            "retained",
            "--body",
            "mail survives reset",
        ],
    ));
    fail(
        &pmux(
            &socket,
            &data,
            &["space", "reset", "alpha", "--confirm", "wrong"],
        ),
        "confirmation",
    );
    assert_eq!(std::fs::read(&alpha).unwrap(), original);
    assert!(!alpha.parent().unwrap().join("reset-backups").exists());
    let result = ok(&pmux(
        &socket,
        &data,
        &["space", "reset", "alpha", "--confirm", "alpha"],
    ));
    let receipt: serde_json::Value = serde_json::from_str(&result).unwrap();
    let backup = Path::new(receipt["backup"].as_str().unwrap());
    assert_eq!(std::fs::read(backup.join("alpha.json")).unwrap(), original);
    assert!(!alpha.exists());
    assert!(beta.exists());
    let claimed = ok(&pmux(
        &socket,
        &data,
        &["mail", "--as", "reset-a", "claim", "--json"],
    ));
    let letters: serde_json::Value = serde_json::from_str(&claimed).unwrap();
    assert_eq!(letters["letters"][0]["body"], "mail survives reset");
    let after = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    for name in ["reset-a", "reset-b"] {
        let old = before.sessions.iter().find(|s| s.name == name).unwrap();
        let live = after.sessions.iter().find(|s| s.name == name).unwrap();
        assert_eq!(live.id, old.id);
        assert_eq!(
            live.windows[0].panes[0].child_pid,
            old.windows[0].panes[0].child_pid
        );
        assert!(prismattyc_mux::procinfo::pid_alive(
            live.windows[0].panes[0].child_pid.unwrap()
        ));
        if name == "reset-a" {
            assert_eq!(live.space_id, None);
        } else {
            assert_eq!(live.space_id, old.space_id);
            assert!(live.space_id.is_some());
        }
    }
    ok(&pmux(
        &socket,
        &data,
        &["space", "reset", "alpha", "--confirm", "alpha"],
    ));
    assert_eq!(std::fs::read(backup.join("alpha.json")).unwrap(), original);
}

#[test]
fn reset_all_handles_corrupt_definitions_and_orphan_ownership() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);
    for (space, session) in [("alpha", "orphan-a"), ("beta", "orphan-b")] {
        ok(&pmux(
            &socket,
            &data,
            &[
                "space",
                "create",
                space,
                "--session-name",
                session,
                "--no-attach",
            ],
        ));
    }
    let alpha = find_named(&data, "alpha");
    let beta = find_named(&data, "beta");
    std::fs::remove_file(alpha).unwrap();
    std::fs::write(&beta, b"broken json\n").unwrap();
    let result = ok(&pmux(
        &socket,
        &data,
        &["space", "reset", "--all", "--confirm", "RESET ALL"],
    ));
    let receipt: serde_json::Value = serde_json::from_str(&result).unwrap();
    let backup = Path::new(receipt["backup"].as_str().unwrap());
    assert_eq!(
        std::fs::read(backup.join("beta.json")).unwrap(),
        b"broken json\n"
    );
    assert!(!beta.exists());
    let snapshot = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    for name in ["orphan-a", "orphan-b"] {
        let live = snapshot.sessions.iter().find(|s| s.name == name).unwrap();
        assert_eq!(live.space_id, None);
        assert!(prismattyc_mux::procinfo::pid_alive(
            live.windows[0].panes[0].child_pid.unwrap()
        ));
    }
    ok(&pmux(
        &socket,
        &data,
        &["space", "reset", "--all", "--confirm", "RESET ALL"],
    ));
    assert_eq!(
        std::fs::read(backup.join("beta.json")).unwrap(),
        b"broken json\n"
    );
}

#[test]
fn reset_backup_failure_preserves_definition_and_ownership() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);
    ok(&pmux(
        &socket,
        &data,
        &[
            "space",
            "create",
            "alpha",
            "--session-name",
            "protected",
            "--no-attach",
        ],
    ));
    let alpha = find_named(&data, "alpha");
    let original = std::fs::read(&alpha).unwrap();
    let before = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    let owner = before
        .sessions
        .iter()
        .find(|s| s.name == "protected")
        .unwrap()
        .space_id
        .clone();
    std::fs::write(alpha.parent().unwrap().join("reset-backups"), b"blocked").unwrap();
    let output = pmux(
        &socket,
        &data,
        &["space", "reset", "alpha", "--confirm", "alpha"],
    );
    fail(&output, "backup");
    assert_eq!(std::fs::read(&alpha).unwrap(), original);
    let after = TeamClient::connect(&socket).unwrap().snapshot().unwrap();
    assert_eq!(
        after
            .sessions
            .iter()
            .find(|s| s.name == "protected")
            .unwrap()
            .space_id,
        owner
    );
}
