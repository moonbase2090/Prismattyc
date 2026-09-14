//! T1: the `pmux` front door — happy lifecycle plus the safety
//! claims (foreign refuse, no socket unlink, recycled pidfile never
//! signalled, lost pidfile still stops the verified server, dead-child
//! start failure is reported with the log tail).

#![cfg(unix)]

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use prismattyc_mux::{
    attach_tabs, host_ack_path_from_socket, host_pid_path_from_socket, procinfo, touch_host_ack,
    AxisWire, ControlError, ControlErrorCode, ControlRequest, ControlResponse, ControlResponseBody,
    ControlResponseData, Event, LayoutSnapshot, Snapshot, SpawnSpec, WindowSnapshot,
    PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

fn socket_path() -> PathBuf {
    // A per-process atomic counter guarantees a distinct socket per test.
    // A wall-clock timestamp is not enough: parallel test threads can start
    // within the same clock tick, collide on one path, and cross-wire their
    // servers and attach clients.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "prism-umbrella-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn umbrella(socket: &Path, args: &[&str]) -> Output {
    umbrella_vars(socket, &[], args)
}

fn umbrella_env(socket: &Path, extra: &[(&str, &Path)], args: &[&str]) -> Output {
    let vars: Vec<(&str, std::ffi::OsString)> = extra
        .iter()
        .map(|(key, value)| (*key, value.as_os_str().to_os_string()))
        .collect();
    umbrella_vars(socket, &vars, args)
}

/// Run a test client with explicit extras, then remove ambient mux settings.
/// This makes even an explicit bogus PMUX_SOCKET harmless to the test daemon.
fn umbrella_vars(socket: &Path, extra: &[(&str, std::ffi::OsString)], args: &[&str]) -> Output {
    let mut command = umbrella_command(socket, args);
    for (key, value) in extra {
        command.env(key, value);
    }
    clear_command_env(&mut command);
    command.output().expect("run pmux")
}

/// Run a test client while retaining explicit extras after the standard scrub.
/// Tests that exercise environment precedence use this lower-level helper.
fn umbrella_vars_cleared(
    socket: &Path,
    extra: &[(&str, std::ffi::OsString)],
    clear: &[&str],
    args: &[&str],
) -> Output {
    let mut command = umbrella_command(socket, args);
    clear_command_env(&mut command);
    for key in clear {
        command.env_remove(key);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().expect("run pmux")
}

fn umbrella_command(socket: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    command
        .env("PMUX_SERVER", env!("CARGO_BIN_EXE_pmuxd"))
        .env("PMUX_ATTACH", env!("CARGO_BIN_EXE_pmux-attach"))
        .arg("--socket")
        .arg(socket)
        .args(args);
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Stops the detached server and removes the test's leftovers even when an
/// assertion panics mid-test; `stop` only signals a cmdline-verified pid.
struct ServerGuard {
    socket: PathBuf,
}

impl ServerGuard {
    fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = umbrella(&self.socket, &["stop"]);
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.socket.with_extension("pid"));
        let _ = std::fs::remove_file(self.socket.with_extension("log"));
        let _ = std::fs::remove_file(attach_tabs::layout_path_from_socket(&self.socket));
        let _ = std::fs::remove_file(host_pid_path_from_socket(&self.socket));
        let _ = std::fs::remove_file(host_ack_path_from_socket(&self.socket));
    }
}

fn start_server(socket: &Path) -> ServerGuard {
    let up = umbrella(socket, &["up", "--", "/bin/sh"]);
    assert!(
        up.status.success(),
        "up failed: {}{}",
        stdout(&up),
        stderr(&up)
    );
    assert!(stdout(&up).contains("started pmuxd"));
    ServerGuard::new(socket)
}

#[test]
fn ambient_mux_env_is_cleared_before_test_server() {
    let socket = socket_path();
    let bogus = std::env::temp_dir().join(format!("pt186-bogus-{}.sock", std::process::id()));
    let output = umbrella_vars(
        &socket,
        &[
            ("PMUX_SOCKET", bogus.as_os_str().to_os_string()),
            (
                "PMUX_PANE_LOG",
                std::env::temp_dir()
                    .join(format!("pt186-bogus-{}.log", std::process::id()))
                    .as_os_str()
                    .to_os_string(),
            ),
        ],
        &["up", "--", "/bin/sh"],
    );
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        socket.exists(),
        "test daemon must bind its requested socket"
    );
    assert!(
        !bogus.exists(),
        "ambient PMUX_SOCKET must not redirect the test daemon"
    );
    let status = umbrella(&socket, &["status"]);
    assert!(
        stdout(&status).contains("status: running"),
        "{}",
        stdout(&status)
    );
    let _guard = ServerGuard::new(&socket);
}

#[test]
fn lifecycle_up_ls_new_stop() {
    let socket = socket_path();
    let _guard = start_server(&socket);

    let status = umbrella(&socket, &["status"]);
    assert!(status.status.success());
    assert!(
        stdout(&status).contains("status: running"),
        "{}",
        stdout(&status)
    );
    assert!(stdout(&status).contains("pid:"));

    let ls = umbrella(&socket, &["ls"]);
    assert!(ls.status.success(), "{}", stderr(&ls));
    let listing = stdout(&ls);
    assert!(listing.contains("session"), "{listing}");
    assert!(listing.contains("pane"), "{listing}");
    assert!(listing.contains("alive"), "{listing}");

    let created = umbrella(&socket, &["new", "work", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    assert!(
        stdout(&created).contains("created session \"work\""),
        "non-TTY new stays detached: {}",
        stdout(&created)
    );

    let ls_after = umbrella(&socket, &["ls"]);
    assert!(
        stdout(&ls_after).contains("session work"),
        "{}",
        stdout(&ls_after)
    );

    // Idempotent up must not disturb the running server or its pidfile.
    let pid_before = std::fs::read_to_string(socket.with_extension("pid")).unwrap();
    let again = umbrella(&socket, &["up"]);
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("already running"),
        "{}",
        stdout(&again)
    );
    let pid_after = std::fs::read_to_string(socket.with_extension("pid")).unwrap();
    assert_eq!(pid_before, pid_after);

    let stop = umbrella(&socket, &["stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped pid"), "{}", stdout(&stop));

    // Verb-based stop: the server unlinks its own socket after flushing
    // ShutdownAccepted. The CLI still never unlinks the path itself.
    assert!(
        !socket.exists(),
        "ShutdownServer teardown must remove the owned socket"
    );

    let after = umbrella(&socket, &["status"]);
    assert!(stdout(&after).contains("not running"), "{}", stdout(&after));
}

#[test]
fn attention_cli_raises_mux_attention_without_pty_injection() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "work", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let sent = umbrella(&socket, &["attention", "work", "hi"]);
    assert!(sent.status.success(), "{}", stderr(&sent));
    assert!(stdout(&sent).contains("attention sent to session \"work\""));

    let mut client = TestClient::connect(&socket);
    let registered = client.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let prismattyc_mux::ControlResponseData::ClientRegistered { client_id } = registered else {
        panic!("expected client registration");
    };
    let snapshot = client.snapshot();
    let pane_id = session_window(&snapshot, "work").panes[0].id;
    let pane = session_window(&snapshot, "work")
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("attention pane");
    assert_eq!(pane.attention.as_deref(), Some("hi"));

    let events = match client.request(|request_id| ControlRequest::Events {
        version: PROTOCOL_VERSION,
        request_id,
        after_sequence: 0,
        limit: None,
    }) {
        ControlResponseData::Events { batch } => batch,
        other => panic!("expected events, got {other:?}"),
    };
    assert!(events.events.iter().any(|envelope| matches!(
        &envelope.event,
        Event::PaneAttention {
            pane_id: event_pane,
            message,
        } if *event_pane == pane_id && message == "hi"
    )));
    let content = match client.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    }) {
        ControlResponseData::PaneContent { content } => content,
        other => panic!("expected pane content, got {other:?}"),
    };
    assert!(
        !content.lines.join("\n").contains("hi"),
        "attention payload leaked into visible text: {:?}",
        content.lines
    );
}

#[test]
fn stop_named_session_leaves_server_running() {
    let socket = socket_path();
    let _guard = start_server(&socket);

    let created = umbrella(&socket, &["new", "cursor-hv", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let flagged = umbrella(&socket, &["--session", "cursor-hv", "stop"]);
    assert!(flagged.status.success(), "{}", stderr(&flagged));
    assert!(
        stdout(&flagged).contains("stopped session \"cursor-hv\""),
        "{}",
        stdout(&flagged)
    );

    let status = umbrella(&socket, &["status"]);
    assert!(
        stdout(&status).contains("status: running"),
        "{}",
        stdout(&status)
    );

    let again = umbrella(&socket, &["new", "cursor-hv", "--", "/bin/sh"]);
    assert!(again.status.success(), "{}", stderr(&again));
    let positional = umbrella(&socket, &["stop", "cursor-hv"]);
    assert!(positional.status.success(), "{}", stderr(&positional));
    assert!(
        stdout(&positional).contains("stopped session \"cursor-hv\""),
        "{}",
        stdout(&positional)
    );

    let missing = umbrella(&socket, &["--session", "ghost", "stop"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("no session matching"),
        "{}",
        stderr(&missing)
    );

    let listing = stdout(&umbrella(&socket, &["ls"]));
    assert!(!listing.contains("session cursor-hv"), "{listing}");
    assert!(listing.contains("session"), "{listing}");
}

#[test]
fn stop_when_never_started_is_clean() {
    let socket = socket_path();
    let stop = umbrella(&socket, &["stop"]);
    assert!(stop.status.success());
    assert!(stdout(&stop).contains("not running"));
}

#[test]
fn foreign_path_is_refused_and_left_intact() {
    let socket = socket_path();
    std::fs::write(&socket, b"not a socket").unwrap();

    for verb in ["up", "attach", "stop"] {
        let out = umbrella(&socket, &[verb]);
        assert!(!out.status.success(), "{verb} must refuse a foreign path");
        assert!(
            stderr(&out).contains("refusing"),
            "{verb}: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        std::fs::read(&socket).unwrap(),
        b"not a socket",
        "foreign path must be left intact"
    );
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn recycled_pidfile_pid_is_never_signalled() {
    let socket = socket_path();
    let _guard = start_server(&socket);

    // Plant a live process that is not a pmuxd in the pidfile.
    let mut decoy = Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .expect("spawn decoy");
    std::fs::write(socket.with_extension("pid"), format!("{}\n", decoy.id())).unwrap();

    let stop = umbrella(&socket, &["stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped pid"), "{}", stdout(&stop));

    // The decoy must be untouched; the real server (found via the /proc
    // scan) must be the one that stopped.
    assert!(
        decoy.try_wait().expect("decoy try_wait").is_none(),
        "decoy pid from the stale pidfile was signalled"
    );
    let after = umbrella(&socket, &["status"]);
    assert!(stdout(&after).contains("not running"), "{}", stdout(&after));

    let _ = decoy.kill();
    let _ = decoy.wait();
}

#[test]
fn lost_pidfile_still_stops_the_verified_server() {
    let socket = socket_path();
    let _guard = start_server(&socket);

    std::fs::remove_file(socket.with_extension("pid")).unwrap();

    let stop = umbrella(&socket, &["stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped pid"), "{}", stdout(&stop));
    let after = umbrella(&socket, &["status"]);
    assert!(stdout(&after).contains("not running"), "{}", stdout(&after));
}

#[test]
fn stop_falls_back_to_signal_when_verb_times_out() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let pid: u32 = std::fs::read_to_string(socket.with_extension("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stopped = Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .status()
        .expect("SIGSTOP server");
    assert!(stopped.success(), "SIGSTOP pid {pid}");

    let stop = umbrella(&socket, &["stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped pid"), "{}", stdout(&stop));

    // Signal-killed server does not run Drop, so the leftover stays. The CLI
    // still must not unlink the path (T1).
    assert!(
        socket.exists(),
        "signal fallback must leave the socket for bind-side stale replace"
    );
}

#[test]
fn failed_start_reports_dead_child_with_log_tail() {
    let socket = socket_path();
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let up = command
        .env("PMUX_SERVER", "/usr/bin/false")
        .arg("--socket")
        .arg(&socket)
        .args(["up", "--", "/bin/sh"])
        .output()
        .expect("run pmux");
    assert!(!up.status.success());
    assert!(
        stderr(&up).contains("server exited during start"),
        "{}",
        stderr(&up)
    );
    let _ = std::fs::remove_file(socket.with_extension("log"));
}

fn spawn_viewer(socket: &Path, session: &str) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux-attach"));
    clear_command_env(&mut command);
    command
        .arg("--socket")
        .arg(socket)
        .arg("--session")
        .arg(session)
        .arg("--watch")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn viewer attach")
}

fn wait_for(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    pred()
}

#[test]
fn doctor_and_kick_viewer_leave_session() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "work", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let empty = umbrella(&socket, &["doctor", "work"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert!(
        stdout(&empty).contains("attach: none"),
        "{}",
        stdout(&empty)
    );

    let mut viewer = spawn_viewer(&socket, "work");
    let pid = viewer.id();
    assert!(
        wait_for(
            || stdout(&umbrella(&socket, &["doctor", "work"]))
                .contains(&format!("attach {pid} VIEWER")),
            Duration::from_secs(2)
        ),
        "doctor never saw viewer {pid}: {}",
        stdout(&umbrella(&socket, &["doctor", "work"]))
    );
    let listing = stdout(&umbrella(&socket, &["ls"]));
    assert!(listing.contains(&format!("viewers {pid}")), "{listing}");

    let kick = umbrella(&socket, &["kick", "work"]);
    assert!(kick.status.success(), "{}", stderr(&kick));
    assert!(stdout(&kick).contains("viewer"), "{}", stdout(&kick));
    assert!(
        wait_for(
            || viewer.try_wait().ok().flatten().is_some(),
            Duration::from_secs(2)
        ),
        "viewer {pid} survived kick"
    );

    let after = stdout(&umbrella(&socket, &["ls"]));
    assert!(after.contains("session work"), "{after}");
    assert!(!after.contains(&format!("viewers {pid}")), "{after}");

    let again = umbrella(&socket, &["kick", "work"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("no attach clients"),
        "{}",
        stdout(&again)
    );
}

#[test]
fn kick_missing_session_is_error() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let out = umbrella(&socket, &["kick", "ghost"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no session matching"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn attach_all_execs_host_with_one_flag_per_session() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "work", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let out = command
        .env("PMUX_SERVER", env!("CARGO_BIN_EXE_pmuxd"))
        .env("PRISMATTYC_HOST", "/bin/echo")
        .env("DISPLAY", ":0")
        .arg("--socket")
        .arg(&socket)
        .args(["attach", "--all"])
        .output()
        .expect("attach --all");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("--attach-session"),
        "host must be launched with per-session attaches: {text}"
    );
    assert!(text.contains("--attach-title"), "{text}");
    assert!(text.contains("work"), "{text}");
    assert!(
        !text.contains("--attach-title default"),
        "leftover up session must not become a tab: {text}"
    );

    let combined = umbrella(&socket, &["attach", "--all", "work"]);
    assert!(!combined.status.success());
    assert!(
        stderr(&combined).contains("cannot be combined"),
        "{}",
        stderr(&combined)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn attach_all_without_display_prints_tty_recipe_and_socket() {
    let socket = socket_path();
    let out = umbrella_vars_cleared(
        &socket,
        &[("PRISMATTYC_HOST", std::ffi::OsString::from("/bin/echo"))],
        &["DISPLAY", "WAYLAND_DISPLAY"],
        &["attach", "--all"],
    );
    assert!(!out.status.success(), "must not exec prismattyc-host");
    let err = stderr(&out);
    assert!(err.contains("pmux attach SESSION"), "TTY recipe: {err}");
    assert!(err.contains("C-\\ d"), "detach chord: {err}");
    assert!(
        err.contains(&socket.display().to_string()),
        "resolved socket: {err}"
    );
    let text = format!("{}{}", stdout(&out), err);
    assert!(
        !text.contains("--attach-session"),
        "must not exec the host stub: {text}"
    );
}

#[test]
fn attach_write_all_is_write_payload_not_all_flag() {
    let socket = socket_path();
    std::fs::write(&socket, b"not a socket").unwrap();
    let out = umbrella(&socket, &["attach", "--write", "--all"]);
    let err = stderr(&out);
    assert!(
        !err.contains("cannot be combined"),
        "pre-scan must not steal --write payload: {err}"
    );
    assert!(err.contains("refusing") || out.status.success(), "{err}");
    let _ = std::fs::remove_file(&socket);
}

#[test]
fn attach_all_passes_session_id_not_ambiguous_name() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "1", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let created_comma = umbrella(&socket, &["new", "a,b", "--", "/bin/sh"]);
    assert!(created_comma.status.success(), "{}", stderr(&created_comma));

    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let out = command
        .env("PMUX_SERVER", env!("CARGO_BIN_EXE_pmuxd"))
        .env("PRISMATTYC_HOST", "/bin/echo")
        .env("DISPLAY", ":0")
        .arg("--socket")
        .arg(&socket)
        .args(["attach", "--all"])
        .output()
        .expect("attach --all");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("--attach-title") && text.contains("a,b"),
        "comma names stay one title: {text}"
    );
    let mut ids = Vec::new();
    let tokens: Vec<&str> = text.split_whitespace().collect();
    for pair in tokens.windows(2) {
        if pair[0] == "--attach-session" {
            assert!(
                pair[1].chars().all(|c| c.is_ascii_digit()),
                "attach-session values must be ids: {text}"
            );
            ids.push(pair[1].to_string());
        }
    }
    assert!(ids.len() >= 2, "expected generated ids: {text}");

    let ambiguous = umbrella(&socket, &["attach", "1", "--json"]);
    assert!(!ambiguous.status.success(), "{}", stderr(&ambiguous));
    assert!(
        stderr(&ambiguous).contains("ambiguous"),
        "{}",
        stderr(&ambiguous)
    );

    for id in ids {
        let dump = umbrella(&socket, &["attach", "--session-id", &id, "--json"]);
        assert!(
            dump.status.success(),
            "session-id {id} failed: {}",
            stderr(&dump)
        );
        assert!(
            stdout(&dump).contains("\"pane_id\""),
            "session-id {id}: {}",
            stdout(&dump)
        );
    }
}

#[test]
fn new_no_attach_prints_hint() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "--no-attach", "hint", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let text = stdout(&created);
    assert!(text.contains("created session \"hint\""), "{text}");
    assert!(text.contains("attach: pmux attach hint"), "{text}");
}

#[test]
fn new_attach_flag_execs_attach_without_tty() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "--attach", "live", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let text = stdout(&created);
    assert!(
        !text.contains("created session"),
        "forced attach should not print the detached hint: {text}"
    );
    assert!(
        text.contains("\"pane_id\""),
        "forced attach dumps ReadPane JSON: {text}"
    );
}

#[test]
fn completions_emit_bash_zsh_fish_and_reject_unknown() {
    for (shell, needle) in [
        ("bash", "complete -F _prismattyc_mux pmux"),
        ("zsh", "#compdef pmux"),
        ("fish", "complete -c pmux"),
    ] {
        let out = umbrella(&socket_path(), &["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        let text = stdout(&out);
        assert!(text.contains(needle), "{shell}: {text}");
        for cmd in [
            "up",
            "attach",
            "ls",
            "new",
            "doctor",
            "kick",
            "status",
            "stop",
            "restart",
            "update",
            "completions",
            "config",
            "layout",
            "space",
            "session",
            "sync",
            "status-set",
        ] {
            assert!(text.contains(cmd), "{shell} missing {cmd}");
        }
        assert!(text.contains("no-attach"), "{shell} missing no-attach");
        match shell {
            "bash" => {
                assert!(
                    text.contains("COMP_WORDS[1]"),
                    "bash completions key off the command word: {text}"
                );
                assert!(text.contains("open)"), "bash space open flags: {text}");
                assert!(text.contains("--add"), "bash space open --add: {text}");
                assert!(text.contains("no-run"), "bash space open --no-run: {text}");
                assert!(text.contains("tty"), "bash space open --tty: {text}");
                assert!(text.contains("attach"), "bash space attach: {text}");
            }
            "zsh" => {
                assert!(
                    text.contains(
                        "space) _values 'verb' create save open attach ls rm delete clear add remove move rename"
                    ),
                    "zsh space verbs: {text}"
                );
                assert!(
                    text.contains("session) _values 'verb' name rename reopen suggest clear"),
                    "zsh session verbs: {text}"
                );
            }
            "fish" => {
                assert!(
                    text.contains(
                        "-a 'create save open attach ls rm delete clear add remove move rename'"
                    ),
                    "fish space verbs: {text}"
                );
                assert!(
                    text.contains("-a 'session'"),
                    "fish session command: {text}"
                );
            }
            _ => unreachable!(),
        }
    }
    let bad = umbrella(&socket_path(), &["completions", "tcsh"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("unknown completion shell"));
}

#[test]
fn version_flag_prints_package_and_exits_zero() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let out = command
        .arg("--version")
        .output()
        .expect("run pmux --version");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("pmux "), "{text:?}");
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "must contain {}: {text:?}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn top_help_opens_with_session_tab_space() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let out = command.arg("--help").output().expect("run pmux --help");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        !stderr(&out).contains("session  A pmuxd"),
        "pmux --help must go to stdout"
    );
    let lower = text.to_ascii_lowercase();
    for noun in ["session", "tab", "space"] {
        assert!(lower.contains(noun), "pmux --help names {noun}: {text}");
    }
    assert!(
        lower.contains("only one space"),
        "pmux --help describes exclusive ownership: {text}"
    );
    let first = text.lines().take(8).collect::<Vec<_>>().join("\n");
    assert!(
        first.to_ascii_lowercase().contains("session")
            && first.to_ascii_lowercase().contains("tab")
            && first.to_ascii_lowercase().contains("space"),
        "three nouns must open the help: {first}"
    );
}

#[test]
fn update_help_and_unknown_flag() {
    let help = umbrella(&socket_path(), &["update", "--help"]);
    assert!(help.status.success(), "{}", stderr(&help));
    let text = format!("{}{}", stdout(&help), stderr(&help));
    assert!(text.contains("--host"), "{text}");
    assert!(text.contains("--mux"), "{text}");
    assert!(text.contains("pmux update"), "{text}");
    assert!(text.contains("moonbase2090/Prismattyc"), "{text}");
    assert!(text.contains("--source"), "{text}");
    assert!(text.contains("--rollback"), "{text}");
    let bad = umbrella(&socket_path(), &["update", "--tty"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("unknown release update option"));
}

struct TestClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl TestClient {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect control");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
            next_id: 1,
        }
    }

    fn request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> ControlResponseData {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = make(request_id);
        serde_json::to_writer(&mut self.writer, &request).expect("write request");
        self.writer.write_all(b"\n").expect("nl");
        self.writer.flush().expect("flush");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read response");
        let response: ControlResponse = serde_json::from_str(&line).expect("parse response");
        match response.body {
            ControlResponseBody::Ok { response } => response,
            ControlResponseBody::Error { error } => panic!("control error: {error:?}"),
        }
    }

    /// Like [`Self::request`] but hands the control error back instead of
    /// panicking, for tests that expect a refusal.
    fn try_request(
        &mut self,
        make: impl FnOnce(u64) -> ControlRequest,
    ) -> Result<ControlResponseData, ControlError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = make(request_id);
        serde_json::to_writer(&mut self.writer, &request).expect("write request");
        self.writer.write_all(b"\n").expect("nl");
        self.writer.flush().expect("flush");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read response");
        let response: ControlResponse = serde_json::from_str(&line).expect("parse response");
        match response.body {
            ControlResponseBody::Ok { response } => Ok(response),
            ControlResponseBody::Error { error } => Err(error),
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        match self.request(|request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        }) {
            ControlResponseData::Snapshot { snapshot } => snapshot,
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }
}

fn sh_spawn() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        argv: vec![],
        cwd: None,
        env: BTreeMap::new(),
    }
}

fn session_window<'a>(snapshot: &'a Snapshot, name: &str) -> &'a WindowSnapshot {
    snapshot
        .sessions
        .iter()
        .find(|session| session.name == name)
        .and_then(|session| session.windows.first())
        .unwrap_or_else(|| panic!("session {name} missing a window"))
}

fn dfs_sizes(window: &WindowSnapshot) -> Vec<(u32, u32)> {
    fn walk(layout: &LayoutSnapshot, window: &WindowSnapshot) -> Vec<(u32, u32)> {
        match layout {
            LayoutSnapshot::Leaf { pane_id } => {
                let geometry = window
                    .panes
                    .iter()
                    .find(|pane| pane.id == *pane_id)
                    .map(|pane| pane.geometry)
                    .unwrap_or_else(|| panic!("pane {pane_id} missing geometry"));
                vec![(geometry.cols, geometry.rows)]
            }
            LayoutSnapshot::Split { first, second, .. } => {
                let mut out = walk(first, window);
                out.extend(walk(second, window));
                out
            }
        }
    }
    walk(&window.layout, window)
}

struct DataDirGuard(PathBuf);

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn layout_data_dir() -> DataDirGuard {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "prism-layout-xdg-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("layout xdg dir");
    DataDirGuard(dir)
}

#[test]
fn layout_save_apply_ls_and_reject_bad_name() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let created = umbrella(&socket, &["new", "work", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let window = session_window(&snapshot, "work");
    let window_id = window.id;
    let pane_id = window.panes[0].id;

    let split_v = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: pane_id,
        axis: AxisWire::Vertical,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    assert!(
        matches!(split_v, ControlResponseData::Mutation { .. }),
        "{split_v:?}"
    );
    let split_h = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: pane_id,
        axis: AxisWire::Horizontal,
        ratio: 0.3,
        spawn: sh_spawn(),
        client_id: None,
    });
    assert!(
        matches!(split_h, ControlResponseData::Mutation { .. }),
        "{split_h:?}"
    );

    let saved_snap = ctl.snapshot();
    let saved_window = session_window(&saved_snap, "work");
    assert_eq!(saved_window.panes.len(), 3);
    let saved_sizes = dfs_sizes(saved_window);

    let save = umbrella_env(&socket, xdg, &["layout", "save", "work"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let save_path = stdout(&save).trim().to_string();
    assert!(
        save_path.ends_with("work.json"),
        "save prints the file path: {save_path}"
    );

    let listed = umbrella_env(&socket, xdg, &["layout", "ls"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listing = stdout(&listed);
    assert!(listing.contains("work"), "{listing}");
    assert!(listing.contains("1 window"), "{listing}");
    assert!(listing.contains("3 panes"), "{listing}");

    let bad = umbrella_env(&socket, xdg, &["layout", "save", "work", "--name", "../x"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("layout name"), "{}", stderr(&bad));

    let stop = umbrella(&socket, &["stop", "work"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let apply = umbrella_env(&socket, xdg, &["layout", "apply", "work"]);
    assert!(apply.status.success(), "{}", stderr(&apply));

    let restored = ctl.snapshot();
    let restored_session = restored
        .sessions
        .iter()
        .find(|session| session.name == "work")
        .expect("work session after apply");
    assert_eq!(restored_session.windows.len(), 1, "fresh apply: one window");
    assert_eq!(restored_session.windows[0].panes.len(), 3);
    let restored_sizes = dfs_sizes(&restored_session.windows[0]);
    assert_eq!(restored_sizes.len(), saved_sizes.len());
    for (got, want) in restored_sizes.iter().zip(saved_sizes.iter()) {
        let dc = (got.0 as i32 - want.0 as i32).abs();
        let dr = (got.1 as i32 - want.1 as i32).abs();
        assert!(
            dc <= 1 && dr <= 1,
            "pane size {got:?} vs saved {want:?} exceeds ±1 cell"
        );
    }

    let apply_again = umbrella_env(&socket, xdg, &["layout", "apply", "work"]);
    assert!(apply_again.status.success(), "{}", stderr(&apply_again));
    let added = ctl.snapshot();
    let added_session = added
        .sessions
        .iter()
        .find(|session| session.name == "work")
        .expect("work session after second apply");
    assert_eq!(
        added_session.windows.len(),
        2,
        "apply into an existing session adds a window"
    );
    assert_eq!(added_session.windows[1].panes.len(), 3);
}

#[test]
fn layout_space_save_apply_skips_default_and_binds_agent() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let grok = umbrella(&socket, &["new", "pt56-a", "--", "/bin/sh"]);
    assert!(grok.status.success(), "{}", stderr(&grok));
    let fable = umbrella(&socket, &["new", "pt56-b", "--", "/bin/sh"]);
    assert!(fable.status.success(), "{}", stderr(&fable));

    let save = umbrella_env(&socket, xdg, &["layout", "save", "space", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let save_path = stdout(&save).trim().to_string();
    assert!(
        save_path.ends_with("spaces/today.json"),
        "space save prints the file path: {save_path}"
    );
    let raw = std::fs::read_to_string(&save_path).expect("read space file");
    assert!(raw.contains("\"pt56-a\""), "{raw}");
    assert!(raw.contains("\"pt56-b\""), "{raw}");
    assert!(
        !raw.contains("\"default\""),
        "default session must be omitted: {raw}"
    );

    let listed = umbrella_env(&socket, xdg, &["layout", "ls"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listing = stdout(&listed);
    assert!(listing.contains("space today"), "{listing}");
    assert!(listing.contains("2 sessions"), "{listing}");

    let stop_g = umbrella(&socket, &["stop", "pt56-a"]);
    assert!(stop_g.status.success(), "{}", stderr(&stop_g));
    let stop_f = umbrella(&socket, &["stop", "pt56-b"]);
    assert!(stop_f.status.success(), "{}", stderr(&stop_f));

    // `apply space` opens prismattyc-host (detached) since PT-59; tests
    // must restore only, or a developer's desktop fills with windows.
    let apply = umbrella_env(
        &socket,
        xdg,
        &["layout", "apply", "space", "today", "--no-attach"],
    );
    assert!(apply.status.success(), "{}", stderr(&apply));
    let applied = stdout(&apply);
    assert!(applied.contains("created pt56-a"), "{applied}");
    assert!(applied.contains("agent pt56-a"), "{applied}");
    assert!(applied.contains("created pt56-b"), "{applied}");

    let mut ctl = TestClient::connect(&socket);
    let restored = ctl.snapshot();
    let grok_session = restored
        .sessions
        .iter()
        .find(|session| session.name == "pt56-a")
        .expect("pt56-a after apply space");
    assert_eq!(grok_session.agent_id.as_deref(), Some("pt56-a"));
    assert_eq!(grok_session.windows.len(), 1);
    assert_eq!(grok_session.windows[0].panes.len(), 1);
    let fable_session = restored
        .sessions
        .iter()
        .find(|session| session.name == "pt56-b")
        .expect("pt56-b after apply space");
    assert_eq!(fable_session.agent_id.as_deref(), Some("pt56-b"));

    let apply_again = umbrella_env(
        &socket,
        xdg,
        &["layout", "apply", "space", "today", "--no-attach"],
    );
    assert!(apply_again.status.success(), "{}", stderr(&apply_again));
    assert!(
        stdout(&apply_again).contains("skip pt56-a (already exists)"),
        "{}",
        stdout(&apply_again)
    );
    let skipped = ctl.snapshot();
    let grok_after = skipped
        .sessions
        .iter()
        .find(|session| session.name == "pt56-a")
        .expect("pt56-a still present");
    assert_eq!(grok_after.windows.len(), 1, "skip must not add windows");

    let replace = umbrella_env(
        &socket,
        xdg,
        &[
            "layout",
            "apply",
            "space",
            "today",
            "--replace",
            "--no-attach",
        ],
    );
    assert!(replace.status.success(), "{}", stderr(&replace));
    let replaced = ctl.snapshot();
    let grok_replaced = replaced
        .sessions
        .iter()
        .find(|session| session.name == "pt56-a")
        .expect("pt56-a after --replace");
    assert_eq!(
        grok_replaced.windows.len(),
        2,
        "--replace adds windows to an existing session"
    );
}

#[test]
fn space_save_open_ls_front_door_and_usage_exit() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let grok = umbrella(&socket, &["new", "pt64-a", "--", "/bin/sh"]);
    assert!(grok.status.success(), "{}", stderr(&grok));
    let fable = umbrella(&socket, &["new", "pt64-b", "--", "/bin/sh"]);
    assert!(fable.status.success(), "{}", stderr(&fable));

    let save = umbrella_env(&socket, xdg, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let save_path = stdout(&save).trim().to_string();
    assert!(
        save_path.contains("spaces/today.json"),
        "space save prints the file path: {save_path}"
    );

    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listing = stdout(&listed);
    assert!(listing.contains("today"), "{listing}");
    assert!(listing.contains("2 sessions"), "{listing}");
    assert!(listing.contains("tabs"), "{listing}");
    assert!(
        !listing.contains("space today"),
        "space ls does not prefix with 'space ': {listing}"
    );
    let layout_listed = umbrella_env(&socket, xdg, &["layout", "ls"]);
    assert!(layout_listed.status.success(), "{}", stderr(&layout_listed));
    assert!(
        stdout(&layout_listed).contains("space today"),
        "{}",
        stdout(&layout_listed)
    );

    let stop_a = umbrella(&socket, &["stop", "pt64-a"]);
    assert!(stop_a.status.success(), "{}", stderr(&stop_a));
    let stop_b = umbrella(&socket, &["stop", "pt64-b"]);
    assert!(stop_b.status.success(), "{}", stderr(&stop_b));

    let open = umbrella_env(&socket, xdg, &["space", "open", "today", "--no-attach"]);
    assert!(open.status.success(), "{}", stderr(&open));
    let opened = stdout(&open);
    assert!(opened.contains("created pt64-a"), "{opened}");
    assert!(opened.contains("created pt64-b"), "{opened}");

    let missing = umbrella_env(&socket, xdg, &["space", "open", "no-such", "--no-attach"]);
    assert!(!missing.status.success(), "missing space file must fail");
    let missing_err = format!("{}{}", stdout(&missing), stderr(&missing));
    assert!(
        missing_err.contains("no-such") || missing_err.contains("no-such"),
        "missing file names the path: {missing_err}"
    );

    let no_verb = umbrella_env(&socket, xdg, &["space"]);
    assert_eq!(no_verb.status.code(), Some(2), "{}", stderr(&no_verb));
    assert!(
        stderr(&no_verb).contains("pmux space"),
        "{}",
        stderr(&no_verb)
    );

    let unknown = umbrella_env(&socket, xdg, &["space", "bogus"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));

    let save_help = umbrella(&socket, &["space", "save", "--help"]);
    assert!(save_help.status.success(), "{}", stderr(&save_help));
    let save_help_text = format!("{}{}", stdout(&save_help), stderr(&save_help));
    assert!(
        save_help_text.contains("pmux space"),
        "space save --help: {save_help_text}"
    );
    let open_help = umbrella(&socket, &["space", "open", "--help"]);
    assert!(open_help.status.success(), "{}", stderr(&open_help));
    assert!(
        format!("{}{}", stdout(&open_help), stderr(&open_help)).contains("pmux space"),
        "{}",
        stderr(&open_help)
    );

    let open_bogus = umbrella(&socket, &["space", "open", "--bogus"]);
    assert!(!open_bogus.status.success(), "{}", stderr(&open_bogus));
    let open_bogus_err = stderr(&open_bogus);
    assert!(
        open_bogus_err.contains("pmux space open"),
        "open flag error names the space verb: {open_bogus_err}"
    );
    assert!(
        !open_bogus_err.contains("layout apply"),
        "open flag error must not name layout: {open_bogus_err}"
    );
    let save_bogus = umbrella(&socket, &["space", "save", "--bogus"]);
    assert!(!save_bogus.status.success(), "{}", stderr(&save_bogus));
    let save_bogus_err = stderr(&save_bogus);
    assert!(
        save_bogus_err.contains("pmux space save"),
        "save flag error names the space verb: {save_bogus_err}"
    );
    assert!(
        !save_bogus_err.contains("layout save space"),
        "save flag error must not name layout: {save_bogus_err}"
    );
}

#[test]
fn space_remove_and_kill_refuses_wrong_space_and_destroys_only_target() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["kill-target", "keep-target"] {
        let result = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(result.status.success(), "{}", stderr(&result));
        let result = umbrella_env(&socket, xdg, &["space", "save", name, name]);
        assert!(result.status.success(), "{}", stderr(&result));
    }
    let mut ctl = TestClient::connect(&socket);
    let before = ctl.snapshot();
    let wrong = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "remove",
            "keep-target",
            "--session",
            "kill-target",
            "--kill",
        ],
    );
    assert!(!wrong.status.success());
    assert_eq!(ctl.snapshot().sessions.len(), before.sessions.len());
    let killed = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "remove",
            "kill-target",
            "--session",
            "kill-target",
            "--kill",
        ],
    );
    assert!(killed.status.success(), "{}", stderr(&killed));
    let after = ctl.snapshot();
    assert!(!after.sessions.iter().any(|s| s.name == "kill-target"));
    let prior = before
        .sessions
        .iter()
        .find(|s| s.name == "keep-target")
        .unwrap();
    let kept = after
        .sessions
        .iter()
        .find(|s| s.name == "keep-target")
        .unwrap();
    assert_eq!(kept.id, prior.id);
    assert_eq!(kept.space_id, prior.space_id);
    assert_eq!(
        kept.windows[0].panes[0].child_pid,
        prior.windows[0].panes[0].child_pid
    );
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data.0.join("prismattyc/spaces/kill-target.json")).unwrap(),
    )
    .unwrap();
    assert!(saved["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn saving_reduced_space_releases_omitted_sessions_without_stopping_them() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["keep", "omit", "new-seat"] {
        let created = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let initial = umbrella_env(&socket, xdg, &["space", "save", "desk", "keep", "omit"]);
    assert!(initial.status.success(), "{}", stderr(&initial));
    let mut ctl = TestClient::connect(&socket);
    let before = ctl.snapshot();
    let omitted = before.sessions.iter().find(|s| s.name == "omit").unwrap();
    // Save the reduced window and claim a newly attached session in one save.
    let saved = umbrella_env(&socket, xdg, &["space", "save", "desk", "keep", "new-seat"]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    let after = ctl.snapshot();
    let released = after.sessions.iter().find(|s| s.name == "omit").unwrap();
    assert!(
        released.space_id.is_none(),
        "removed session is still owned: {:?}",
        released.space_id
    );
    assert_eq!(released.id, omitted.id);
    assert_eq!(
        released.windows[0].panes[0].child_pid,
        omitted.windows[0].panes[0].child_pid
    );
    let kept = after.sessions.iter().find(|s| s.name == "keep").unwrap();
    let added = after
        .sessions
        .iter()
        .find(|s| s.name == "new-seat")
        .unwrap();
    assert_eq!(added.space_id, kept.space_id);
    assert!(kept.space_id.is_some());
    let reopened = umbrella_env(
        &socket,
        xdg,
        &["space", "open", "desk", "--no-attach", "--no-run", "--tty"],
    );
    assert!(reopened.status.success(), "{}", stderr(&reopened));
    assert!(ctl
        .snapshot()
        .sessions
        .iter()
        .find(|s| s.name == "omit")
        .unwrap()
        .space_id
        .is_none());

    // Replay a save interrupted before its release, and after it completed.
    let dir = data.0.join("prismattyc/spaces");
    let mut definition: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("desk.json")).unwrap()).unwrap();
    definition["sessions"]
        .as_array_mut()
        .unwrap()
        .retain(|s| s["name"] == "keep");
    definition["tabs"] = serde_json::json!([]);
    definition["focused_session"] = serde_json::json!("keep");
    let journal = serde_json::json!({
        "files": [["desk", definition]], "sessions": [], "from": null,
        "to": kept.space_id, "releases": ["new-seat"]
    });
    for _ in 0..2 {
        std::fs::write(
            dir.join(".ownership-transaction"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        let recovered = umbrella_env(
            &socket,
            xdg,
            &["space", "open", "desk", "--no-attach", "--no-run", "--tty"],
        );
        assert!(recovered.status.success(), "{}", stderr(&recovered));
        assert!(!dir.join(".ownership-transaction").exists());
        let current = ctl.snapshot();
        let released = current
            .sessions
            .iter()
            .find(|s| s.name == "new-seat")
            .unwrap();
        assert!(released.space_id.is_none());
        assert_eq!(
            released.windows[0].panes[0].child_pid,
            added.windows[0].panes[0].child_pid
        );
    }
}

#[test]
fn space_add_and_remove_transfer_exclusive_ownership() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let a = umbrella(&socket, &["new", "--no-attach", "pt182-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "--no-attach", "pt182-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));
    let save = umbrella_env(
        &socket,
        xdg,
        &["space", "save", "desk", "pt182-a", "pt182-b"],
    );
    assert!(save.status.success(), "{}", stderr(&save));

    let extra = umbrella(
        &socket,
        &["new", "--no-attach", "pt182-extra", "--", "/bin/sh"],
    );
    assert!(extra.status.success(), "{}", stderr(&extra));
    let add = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "add",
            "desk",
            "--session",
            "pt182-extra",
            "--tab",
            "Extra",
        ],
    );
    assert!(add.status.success(), "{}", stderr(&add));
    assert!(stdout(&add).contains("pt182-extra"), "{}", stdout(&add));
    let body = std::fs::read_to_string(data.0.join("prismattyc/spaces/desk.json")).unwrap();
    assert!(body.contains("pt182-extra"), "{body}");
    assert!(body.contains("Extra"), "{body}");

    let again = umbrella_env(
        &socket,
        xdg,
        &["space", "add", "desk", "--session", "pt182-extra"],
    );
    assert!(!again.status.success(), "duplicate add must fail");

    let remove = umbrella_env(
        &socket,
        xdg,
        &["space", "remove", "desk", "--session", "pt182-extra"],
    );
    assert!(remove.status.success(), "{}", stderr(&remove));
    let body = std::fs::read_to_string(data.0.join("prismattyc/spaces/desk.json")).unwrap();
    assert!(!body.contains("pt182-extra"), "{body}");

    let last = umbrella_env(
        &socket,
        xdg,
        &["space", "remove", "desk", "--session", "pt182-a"],
    );
    assert!(last.status.success(), "{}", stderr(&last));
    let refuse = umbrella_env(
        &socket,
        xdg,
        &["space", "remove", "desk", "--session", "pt182-b"],
    );
    assert!(refuse.status.success(), "{}", stderr(&refuse));
    let raw = std::fs::read_to_string(data.0.join("prismattyc/spaces/desk.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(saved["sessions"].as_array().unwrap().is_empty());
    let mut ctl = TestClient::connect(&socket);
    let snapshot = ctl.snapshot();
    let released = snapshot
        .sessions
        .iter()
        .find(|s| s.name == "pt182-b")
        .unwrap();
    assert!(released.space_id.is_none());
    assert!(released.windows[0].panes[0].child_pid.is_some());
}

#[test]
fn space_move_updates_both_owners_and_failed_add_is_noop() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["pt182-src-a", "pt182-src-b", "pt182-dst"] {
        let created = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let desk = umbrella_env(
        &socket,
        xdg,
        &["space", "save", "desk", "pt182-src-a", "pt182-src-b"],
    );
    assert!(desk.status.success(), "{}", stderr(&desk));
    let lab = umbrella_env(&socket, xdg, &["space", "save", "lab", "pt182-dst"]);
    assert!(lab.status.success(), "{}", stderr(&lab));

    let moved = umbrella_env(
        &socket,
        xdg,
        &["space", "move", "lab", "--session", "pt182-src-a"],
    );
    assert!(moved.status.success(), "{}", stderr(&moved));

    let desk_path = data.0.join("prismattyc/spaces/desk.json");
    let lab_path = data.0.join("prismattyc/spaces/lab.json");
    let desk_body = std::fs::read_to_string(&desk_path).unwrap();
    let lab_body = std::fs::read_to_string(&lab_path).unwrap();
    assert!(!desk_body.contains("pt182-src-a"), "{desk_body}");
    assert!(desk_body.contains("pt182-src-b"), "{desk_body}");
    assert!(lab_body.contains("pt182-src-a"), "{lab_body}");
    assert!(lab_body.contains("pt182-dst"), "{lab_body}");

    let before_desk = desk_body;
    let before_lab = lab_body;
    let again = umbrella_env(
        &socket,
        xdg,
        &["space", "add", "lab", "--session", "pt182-src-a"],
    );
    assert!(!again.status.success(), "duplicate add must fail");
    assert_eq!(std::fs::read_to_string(&desk_path).unwrap(), before_desk);
    assert_eq!(std::fs::read_to_string(&lab_path).unwrap(), before_lab);
}

#[test]
fn move_unassigned_seat_preserves_identity_and_undo_releases_it() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["fresh-nested", "destination"] {
        let result = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(result.status.success(), "{}", stderr(&result));
    }
    let result = umbrella_env(&socket, xdg, &["space", "save", "desk", "destination"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let mut client = TestClient::connect(&socket);
    let before = client.snapshot();
    let seat = before
        .sessions
        .iter()
        .find(|s| s.name == "fresh-nested")
        .unwrap();
    assert!(seat.space_id.is_none());
    let pane = &seat.windows[0].panes[0];
    let undo = data.0.join("move-undo.json");
    let result = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "move",
            "desk",
            "--pane",
            &pane.id.to_string(),
            "--undo-file",
            undo.to_str().unwrap(),
        ],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let after = client.snapshot();
    let moved = after.sessions.iter().find(|s| s.id == seat.id).unwrap();
    assert!(moved.space_id.is_some());
    assert_eq!(moved.name, seat.name);
    assert_eq!(moved.windows[0].panes[0].id, pane.id);
    assert_eq!(moved.windows[0].panes[0].child_pid, pane.child_pid);
    let result = umbrella_env(&socket, xdg, &["space", "undo", undo.to_str().unwrap()]);
    assert!(result.status.success(), "{}", stderr(&result));
    let after = client.snapshot();
    let restored = after.sessions.iter().find(|s| s.id == seat.id).unwrap();
    assert!(restored.space_id.is_none());
    assert_eq!(restored.windows[0].panes[0].child_pid, pane.child_pid);
}

/// Compare stable seats without terminal output or focus-ledger timestamps.
type MoveSeat = (u64, String, Option<String>, Vec<(u64, Option<u32>)>);
fn move_seats(snapshot: &Snapshot) -> Vec<MoveSeat> {
    snapshot
        .sessions
        .iter()
        .map(|s| {
            (
                s.id,
                s.name.clone(),
                s.space_id.clone(),
                s.windows
                    .iter()
                    .flat_map(|w| &w.panes)
                    .map(|p| (p.id, p.child_pid))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn space_move_explicit_id_disambiguates_owned_and_unassigned_sessions() {
    for owned in [false, true] {
        let socket = socket_path();
        let _guard = start_server(&socket);
        let data = layout_data_dir();
        let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
        let mut client = TestClient::connect(&socket);
        let collision = (client
            .snapshot()
            .sessions
            .iter()
            .map(|s| s.id)
            .max()
            .unwrap_or(0)
            + 2)
        .to_string();
        for name in [collision.as_str(), "intended", "destination"] {
            let result = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
            assert!(result.status.success(), "{}", stderr(&result));
        }
        // Use two panes so an ID-selected whole-session move cannot be mistaken
        // for the existing single-pane special case.
        let snapshot = client.snapshot();
        let window = session_window(&snapshot, "intended");
        client.request(|request_id| ControlRequest::Split {
            version: PROTOCOL_VERSION,
            request_id,
            window_id: window.id,
            target_pane_id: window.panes[0].id,
            axis: AxisWire::Vertical,
            ratio: 0.5,
            spawn: sh_spawn(),
            client_id: None,
        });
        if owned {
            let result = umbrella_env(
                &socket,
                xdg,
                &["space", "save", "source", &collision, "intended"],
            );
            assert!(result.status.success(), "{}", stderr(&result));
        }
        let result = umbrella_env(&socket, xdg, &["space", "save", "target", "destination"]);
        assert!(result.status.success(), "{}", stderr(&result));
        let before = client.snapshot();
        let intended = before
            .sessions
            .iter()
            .find(|s| s.name == "intended")
            .unwrap();
        assert_eq!(intended.id.to_string(), collision);
        let target_owner = before
            .sessions
            .iter()
            .find(|s| s.name == "destination")
            .unwrap()
            .space_id
            .clone();
        let undo = data.0.join("id-move-undo.json");
        let result = umbrella_env(
            &socket,
            xdg,
            &[
                "space",
                "move",
                "target",
                "--session-id",
                &collision,
                "--undo-file",
                undo.to_str().unwrap(),
            ],
        );
        assert!(result.status.success(), "{}", stderr(&result));
        let mut expected = move_seats(&before);
        expected.iter_mut().find(|s| s.0 == intended.id).unwrap().2 = target_owner;
        assert_eq!(
            move_seats(&client.snapshot()),
            expected,
            "only the exact session may move; all pane and process IDs must survive"
        );
        let result = umbrella_env(&socket, xdg, &["space", "undo", undo.to_str().unwrap()]);
        assert!(result.status.success(), "{}", stderr(&result));
        assert_eq!(move_seats(&client.snapshot()), move_seats(&before));
    }
}

#[test]
fn space_move_explicit_id_rejects_invalid_mixed_and_missing_ids() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["999", "destination"] {
        let result = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(result.status.success(), "{}", stderr(&result));
    }
    let result = umbrella_env(&socket, xdg, &["space", "save", "target", "destination"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let mut client = TestClient::connect(&socket);
    let before = move_seats(&client.snapshot());
    for selector in [
        vec!["--session-id", "0"],
        vec!["--session-id", "abc"],
        vec!["--session-id", "-1"],
        vec!["--session-id", "18446744073709551616"],
        vec!["--session-id", "999"],
        vec!["--session-id"],
        vec!["--session-id", "2", "--session", "999"],
        vec!["--session-id", "2", "--pane", "2"],
        vec!["--session-id", "2", "--session-id", "2"],
        vec!["--session-id", "2", "--to-session", "destination"],
    ] {
        let mut args = vec!["space", "move", "target"];
        args.extend(selector);
        let result = umbrella_env(&socket, xdg, &args);
        assert!(!result.status.success(), "must reject {args:?}");
        assert_eq!(
            move_seats(&client.snapshot()),
            before,
            "rejected selector mutated ownership: {args:?}"
        );
    }
    // The name selector still accepts a numeric session name.
    let result = umbrella_env(
        &socket,
        xdg,
        &["space", "move", "target", "--session", "999"],
    );
    assert!(result.status.success(), "{}", stderr(&result));
}

#[test]
fn space_rm_deletes_files_and_all() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let a = umbrella(&socket, &["new", "--no-attach", "pt89-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "--no-attach", "pt89-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));
    let save_one = umbrella_env(&socket, xdg, &["space", "save", "alpha", "pt89-a"]);
    assert!(save_one.status.success(), "{}", stderr(&save_one));
    let save_two = umbrella_env(&socket, xdg, &["space", "save", "beta", "pt89-b"]);
    assert!(save_two.status.success(), "{}", stderr(&save_two));
    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    let listing = stdout(&listed);
    assert!(listing.contains("alpha"), "{listing}");
    assert!(listing.contains("beta"), "{listing}");

    let rm = umbrella_env(&socket, xdg, &["space", "rm", "alpha"]);
    assert!(rm.status.success(), "{}", stderr(&rm));
    assert!(stdout(&rm).contains("removed alpha"), "{}", stdout(&rm));
    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    let listing = stdout(&listed);
    assert!(!listing.contains("alpha"), "{listing}");
    assert!(listing.contains("beta"), "{listing}");

    let missing = umbrella_env(&socket, xdg, &["space", "rm", "no-such"]);
    assert!(!missing.status.success(), "unknown space must fail");
    let missing_err = format!("{}{}", stdout(&missing), stderr(&missing));
    assert!(
        missing_err.contains("no-such"),
        "unknown names the path: {missing_err}"
    );

    let all = umbrella_env(&socket, xdg, &["space", "rm", "--all"]);
    assert!(all.status.success(), "{}", stderr(&all));
    let all_out = stdout(&all);
    assert!(all_out.contains("beta"), "{all_out}");
    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    assert!(
        stdout(&listed).trim().is_empty(),
        "ls empty after --all: {}",
        stdout(&listed)
    );
}

#[test]
fn space_clear_deletes_files_except_keep() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let a = umbrella(&socket, &["new", "--no-attach", "pt107-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "--no-attach", "pt107-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));
    let save_one = umbrella_env(&socket, xdg, &["space", "save", "alpha", "pt107-a"]);
    assert!(save_one.status.success(), "{}", stderr(&save_one));
    let save_two = umbrella_env(&socket, xdg, &["space", "save", "beta", "pt107-b"]);
    assert!(save_two.status.success(), "{}", stderr(&save_two));
    let c = umbrella(&socket, &["new", "--no-attach", "pt107-c", "--", "/bin/sh"]);
    assert!(c.status.success(), "{}", stderr(&c));
    let save_three = umbrella_env(&socket, xdg, &["space", "save", "gamma", "pt107-c"]);
    assert!(save_three.status.success(), "{}", stderr(&save_three));

    let empty_first = umbrella_env(
        &socket,
        xdg,
        &[
            "space", "clear", "--keep", "alpha", "--keep", "beta", "--keep", "gamma",
        ],
    );
    assert!(empty_first.status.success(), "{}", stderr(&empty_first));
    assert!(
        stdout(&empty_first).trim().is_empty(),
        "keep-all prints nothing: {}",
        stdout(&empty_first)
    );

    let cleared = umbrella_env(&socket, xdg, &["space", "clear", "--keep", "beta"]);
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    let out = stdout(&cleared);
    assert!(out.contains("removed alpha"), "{out}");
    assert!(out.contains("removed gamma"), "{out}");
    assert!(!out.contains("removed beta"), "{out}");
    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    let listing = stdout(&listed);
    assert!(listing.contains("beta"), "{listing}");
    assert!(!listing.contains("alpha"), "{listing}");
    assert!(!listing.contains("gamma"), "{listing}");

    let ls_sessions = umbrella(&socket, &["ls"]);
    assert!(
        stdout(&ls_sessions).contains("session pt107-a"),
        "space clear must not stop live sessions: {}",
        stdout(&ls_sessions)
    );

    let rest = umbrella_env(&socket, xdg, &["space", "clear"]);
    assert!(rest.status.success(), "{}", stderr(&rest));
    assert!(stdout(&rest).contains("removed beta"), "{}", stdout(&rest));
    let listed = umbrella_env(&socket, xdg, &["space", "ls"]);
    assert!(
        stdout(&listed).trim().is_empty(),
        "ls empty after clear: {}",
        stdout(&listed)
    );

    let empty = umbrella_env(&socket, xdg, &["space", "clear"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert!(
        stdout(&empty).trim().is_empty(),
        "empty clear prints nothing: {}",
        stdout(&empty)
    );

    let help = umbrella(&socket, &["space", "--help"]);
    assert!(help.status.success(), "{}", stderr(&help));
    let help_text = format!("{}{}", stdout(&help), stderr(&help));
    assert!(
        help_text.contains("clear"),
        "space help lists clear: {help_text}"
    );
}

#[test]
fn session_clear_exempts_caller_all_keep_and_empty() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let a = umbrella(&socket, &["new", "--no-attach", "pt107-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "--no-attach", "pt107-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));

    let mut client = TestClient::connect(&socket);
    let snapshot = client.snapshot();
    let pane_a = session_window(&snapshot, "pt107-a").panes[0].id;
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt107-a")
        .map(|session| session.id)
        .expect("pt107-a id");
    let id_b = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt107-b")
        .map(|session| session.id)
        .expect("pt107-b id");

    let cleared = umbrella_vars_cleared(
        &socket,
        &[
            ("PRISMATTYC_PANE_ID", pane_a.to_string().into()),
            ("PMUX_SOCKET", socket.as_os_str().to_os_string()),
        ],
        &[],
        &["session", "clear"],
    );
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    let out = stdout(&cleared);
    assert!(
        out.contains(&format!("stopped pt107-b (id {id_b})")),
        "{out}"
    );
    assert!(
        !out.contains(&format!("stopped pt107-a (id {id_a})")),
        "caller must stay: {out}"
    );
    let ls = umbrella(&socket, &["ls"]);
    let listing = stdout(&ls);
    assert!(listing.contains("session pt107-a"), "{listing}");
    assert!(!listing.contains("session pt107-b"), "{listing}");

    let c = umbrella(&socket, &["new", "--no-attach", "pt107-c", "--", "/bin/sh"]);
    assert!(c.status.success(), "{}", stderr(&c));
    let keep = umbrella(&socket, &["session", "clear", "--keep", "pt107-a"]);
    assert!(keep.status.success(), "{}", stderr(&keep));
    let keep_out = stdout(&keep);
    assert!(!keep_out.contains("stopped pt107-a"), "{keep_out}");
    assert!(keep_out.contains("stopped pt107-c"), "{keep_out}");
    let ls = umbrella(&socket, &["ls"]);
    let listing = stdout(&ls);
    assert!(listing.contains("session pt107-a"), "{listing}");
    assert!(!listing.contains("session pt107-c"), "{listing}");

    let d = umbrella(&socket, &["new", "--no-attach", "pt107-d", "--", "/bin/sh"]);
    assert!(d.status.success(), "{}", stderr(&d));
    let snapshot = client.snapshot();
    let pane_a = session_window(&snapshot, "pt107-a").panes[0].id;
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt107-a")
        .map(|session| session.id)
        .expect("pt107-a id");
    let all = umbrella_vars_cleared(
        &socket,
        &[
            ("PRISMATTYC_PANE_ID", pane_a.to_string().into()),
            ("PMUX_SOCKET", socket.as_os_str().to_os_string()),
        ],
        &[],
        &["session", "clear", "--all"],
    );
    assert!(all.status.success(), "{}", stderr(&all));
    let all_out = stdout(&all);
    let lines: Vec<&str> = all_out
        .lines()
        .filter(|line| line.starts_with("stopped "))
        .collect();
    let last = format!("stopped pt107-a (id {id_a})");
    assert_eq!(
        lines.last().copied(),
        Some(last.as_str()),
        "caller stops last: {all_out}"
    );
    let ls = umbrella(&socket, &["ls"]);
    assert!(
        !stdout(&ls).contains("session pt107-"),
        "all sessions gone: {}",
        stdout(&ls)
    );

    let empty = umbrella(&socket, &["session", "clear"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert!(
        stdout(&empty).trim().is_empty() || !stdout(&empty).contains("stopped pt107-"),
        "empty or leftover default only: {}",
        stdout(&empty)
    );

    let help = umbrella(&socket, &["session", "--help"]);
    assert!(help.status.success(), "{}", stderr(&help));
    let help_text = format!("{}{}", stdout(&help), stderr(&help));
    assert!(help_text.contains("pmux session"), "{help_text}");
    assert!(help_text.contains("clear"), "{help_text}");
    assert!(help_text.contains("pmux ls"), "{help_text}");
}

#[test]
fn session_clear_ignores_pane_id_from_another_socket() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let a = umbrella(&socket, &["new", "--no-attach", "pt107-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let c = umbrella(&socket, &["new", "--no-attach", "pt107-c", "--", "/bin/sh"]);
    assert!(c.status.success(), "{}", stderr(&c));

    let mut client = TestClient::connect(&socket);
    let snapshot = client.snapshot();
    let pane_c = session_window(&snapshot, "pt107-c").panes[0].id;

    let keep = umbrella_vars_cleared(
        &socket,
        &[
            ("PRISMATTYC_PANE_ID", pane_c.to_string().into()),
            (
                "PMUX_SOCKET",
                std::ffi::OsString::from("/tmp/pmux-not-this-test.sock"),
            ),
        ],
        &[],
        &["session", "clear", "--keep", "pt107-a"],
    );
    assert!(keep.status.success(), "{}", stderr(&keep));
    let keep_out = stdout(&keep);
    assert!(
        keep_out.contains("stopped pt107-c"),
        "foreign PMUX_SOCKET must not exempt a colliding pane id: {keep_out}"
    );
    assert!(!keep_out.contains("stopped pt107-a"), "{keep_out}");
    let ls = umbrella(&socket, &["ls"]);
    let listing = stdout(&ls);
    assert!(listing.contains("session pt107-a"), "{listing}");
    assert!(!listing.contains("session pt107-c"), "{listing}");
}

#[test]
fn space_save_records_foreground_and_open_runs_it() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let created = umbrella(&socket, &["new", "--no-attach", "pt93-a", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let save = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            (procinfo::TEST_FOREGROUND_COMMAND_ENV, "sleep 30".into()),
        ],
        &["space", "save", "today"],
    );
    assert!(save.status.success(), "{}", stderr(&save));
    let save_path = stdout(&save).trim().to_string();
    let raw = std::fs::read_to_string(&save_path).expect("read space");
    assert!(
        raw.contains("\"command\": \"sleep 30\"") || raw.contains("\"command\":\"sleep 30\""),
        "space file must record foreground command: {raw}"
    );

    let stop = umbrella(&socket, &["stop", "pt93-a"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let open = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "today", "--no-attach"],
    );
    assert!(open.status.success(), "{}", stderr(&open));
    let opened = stdout(&open);
    assert!(opened.contains("run:"), "{opened}");
    assert!(opened.contains("sleep 30"), "{opened}");
    assert!(opened.contains("ran sleep 30 in pt93-a"), "{opened}");

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let mut found = None;
    for _ in 0..50 {
        let snapshot = ctl.snapshot();
        let pane = session_window(&snapshot, "pt93-a").panes[0].clone();
        if let Some(pid) = pane.child_pid {
            found = procinfo::foreground_command(pid);
            if found.as_deref().is_some_and(|cmd| cmd.contains("sleep")) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let cmd = found.expect("foreground after space open");
    assert!(cmd.contains("sleep"), "{cmd}");
}

#[test]
fn space_open_no_run_and_none_skip_all_runs_unbound() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let cfg_dir = layout_data_dir();
    let none_cfg = cfg_dir.0.join("none.toml");
    std::fs::write(&none_cfg, "[mux]\nspace_open_runs_commands = \"none\"\n")
        .expect("write none config");
    let all_cfg = cfg_dir.0.join("all.toml");
    std::fs::write(&all_cfg, "[mux]\nspace_open_runs_commands = \"all\"\n")
        .expect("write all config");

    let unbound = umbrella(
        &socket,
        &[
            "new",
            "--no-attach",
            "--no-agent",
            "pt93-plain",
            "--",
            "/bin/sh",
        ],
    );
    assert!(unbound.status.success(), "{}", stderr(&unbound));
    let save = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            (procinfo::TEST_FOREGROUND_COMMAND_ENV, "sleep 30".into()),
        ],
        &["space", "save", "plain"],
    );
    assert!(save.status.success(), "{}", stderr(&save));
    let stop = umbrella(&socket, &["stop", "pt93-plain"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let skipped = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            ("PRISMATTYC_CONFIG", none_cfg.as_os_str().to_os_string()),
        ],
        &["space", "open", "plain", "--no-attach"],
    );
    assert!(skipped.status.success(), "{}", stderr(&skipped));
    let skipped_out = stdout(&skipped);
    assert!(
        skipped_out.contains("skip run"),
        "none must skip: {skipped_out}"
    );
    assert!(
        !skipped_out.contains("ran sleep"),
        "none must not run: {skipped_out}"
    );

    let stop = umbrella(&socket, &["stop", "pt93-plain"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let no_run = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "plain", "--no-attach", "--no-run"],
    );
    assert!(no_run.status.success(), "{}", stderr(&no_run));
    let no_run_out = stdout(&no_run);
    assert!(
        no_run_out.contains("skip run"),
        "--no-run must skip: {no_run_out}"
    );
    assert!(
        !no_run_out.contains("ran sleep"),
        "--no-run must not run: {no_run_out}"
    );

    let stop = umbrella(&socket, &["stop", "pt93-plain"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let agents = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "plain", "--no-attach"],
    );
    assert!(agents.status.success(), "{}", stderr(&agents));
    let agents_out = stdout(&agents);
    assert!(
        !agents_out.contains("ran sleep"),
        "agents default skips unbound: {agents_out}"
    );

    let stop = umbrella(&socket, &["stop", "pt93-plain"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let all = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            ("PRISMATTYC_CONFIG", all_cfg.as_os_str().to_os_string()),
        ],
        &["space", "open", "plain", "--no-attach"],
    );
    assert!(all.status.success(), "{}", stderr(&all));
    let all_out = stdout(&all);
    assert!(
        all_out.contains("ran sleep 30 in pt93-plain"),
        "all must run unbound: {all_out}"
    );
}

#[test]
fn arrange_main_vertical_retile_keeps_pane_count() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "--no-attach", "pt132", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let _ = register_client(&mut ctl);
    let snap = ctl.snapshot();
    let window = session_window(&snap, "pt132");
    let window_id = window.id;
    let pane_id = window.panes[0].id;
    let _ = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: pane_id,
        axis: AxisWire::Vertical,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    let _ = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: pane_id,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    assert_eq!(session_window(&ctl.snapshot(), "pt132").panes.len(), 3);
    let arranged = umbrella(&socket, &["arrange", "pt132", "main-vertical"]);
    assert!(arranged.status.success(), "{}", stderr(&arranged));
    let after_snap = ctl.snapshot();
    let after = session_window(&after_snap, "pt132");
    assert_eq!(after.panes.len(), 3);
    match &after.layout {
        LayoutSnapshot::Split { axis, first, .. } => {
            assert_eq!(*axis, AxisWire::Horizontal);
            assert!(matches!(first.as_ref(), LayoutSnapshot::Leaf { .. }));
        }
        other => panic!("expected main-vertical split, got {other:?}"),
    }
}

#[test]
fn arrange_main_vertical_puts_controller_pane_on_the_left() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt132-ctl", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let snap = ctl.snapshot();
    let window = session_window(&snap, "pt132-ctl");
    let window_id = window.id;
    let first_pane = window.panes[0].id;
    let _ = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: first_pane,
        axis: AxisWire::Vertical,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    let split_snap = ctl.snapshot();
    let after_split = session_window(&split_snap, "pt132-ctl");
    assert_eq!(after_split.panes.len(), 2);
    let second = after_split.panes[1].id;
    assert_ne!(second, first_pane);
    let _ = ctl.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id: second,
    });
    let arranged = umbrella(&socket, &["arrange", "pt132-ctl", "main-vertical"]);
    assert!(arranged.status.success(), "{}", stderr(&arranged));
    let after_snap = ctl.snapshot();
    let after = session_window(&after_snap, "pt132-ctl");
    match &after.layout {
        LayoutSnapshot::Split {
            axis,
            first,
            second: right,
            ..
        } => {
            assert_eq!(*axis, AxisWire::Horizontal);
            match first.as_ref() {
                LayoutSnapshot::Leaf { pane_id } => assert_eq!(*pane_id, second),
                other => panic!("expected controller as main leaf, got {other:?}"),
            }
            match right.as_ref() {
                LayoutSnapshot::Leaf { pane_id } => assert_eq!(*pane_id, first_pane),
                other => panic!("expected other pane on the right, got {other:?}"),
            }
        }
        other => panic!("expected main-vertical split, got {other:?}"),
    }
}

fn register_client(ctl: &mut TestClient) -> u64 {
    match ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        ControlResponseData::ClientRegistered { client_id } => client_id,
        other => panic!("expected ClientRegistered, got {other:?}"),
    }
}

fn write_pane_as(ctl: &mut TestClient, client_id: u64, pane_id: u64, data: &str) {
    let _ = ctl.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
    let _ = ctl.request(|request_id| ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        data: data.to_string(),
    });
    let _ = ctl.request(|request_id| ControlRequest::ReleaseLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
}

fn read_pane_lines(ctl: &mut TestClient, client_id: u64, pane_id: u64) -> (bool, String) {
    match ctl.request(|request_id| ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    }) {
        ControlResponseData::PaneContent { content } => {
            (content.child_alive, content.lines.join("\n"))
        }
        other => panic!("expected PaneContent, got {other:?}"),
    }
}

#[test]
fn send_keys_writes_without_holding_lease() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "--no-attach", "pt126", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt126").panes[0].id;
    let sent = umbrella(
        &socket,
        &["send", &pane_id.to_string(), "echo", "PT126MARK", "--enter"],
    );
    assert!(sent.status.success(), "{}", stderr(&sent));
    assert!(stdout(&sent).contains("sent "), "{}", stdout(&sent));
    let snap = ctl.snapshot();
    assert!(
        session_window(&snap, "pt126").panes[0]
            .controller_id
            .is_none(),
        "send must not keep the lease"
    );
    let mut found = false;
    for _ in 0..50 {
        let (_, text) = read_pane_lines(&mut ctl, client_id, pane_id);
        if text.contains("PT126MARK") {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "echo output missing from pane");
}

#[test]
fn send_keys_refuses_live_controller_unless_force() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt126-held", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut holder = TestClient::connect(&socket);
    let holder_id = register_client(&mut holder);
    let pane_id = session_window(&holder.snapshot(), "pt126-held").panes[0].id;
    let _ = holder.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: holder_id,
        pane_id,
    });
    let refused = umbrella(
        &socket,
        &["send", &pane_id.to_string(), "PT126HELD", "--enter"],
    );
    assert!(!refused.status.success(), "must refuse a live controller");
    assert!(
        stderr(&refused).contains("live controller"),
        "{}",
        stderr(&refused)
    );
    let snap = holder.snapshot();
    assert_eq!(
        session_window(&snap, "pt126-held").panes[0].controller_id,
        Some(holder_id),
        "refuse must not steal the lease"
    );

    let forced = umbrella(
        &socket,
        &[
            "send",
            &pane_id.to_string(),
            "--force",
            "PT126HELD",
            "--enter",
        ],
    );
    assert!(forced.status.success(), "{}", stderr(&forced));
    let snap = holder.snapshot();
    assert!(
        session_window(&snap, "pt126-held").panes[0]
            .controller_id
            .is_none(),
        "--force must release: {:?}",
        session_window(&snap, "pt126-held").panes[0].controller_id
    );
}

#[test]
fn send_keys_refuses_dirty_input_unless_force() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt126-dirty", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt126-dirty").panes[0].id;
    write_pane_as(&mut ctl, client_id, pane_id, "git com");
    let snap = ctl.snapshot();
    assert!(
        session_window(&snap, "pt126-dirty").panes[0]
            .ledger
            .dirty_input,
        "partial write must leave dirty_input"
    );
    let refused = umbrella(
        &socket,
        &["send", &pane_id.to_string(), "echo", "x", "--enter"],
    );
    assert!(!refused.status.success(), "must refuse dirty input");
    assert!(
        stderr(&refused).contains("unsubmitted input"),
        "{}",
        stderr(&refused)
    );
    let forced = umbrella(
        &socket,
        &[
            "send",
            &pane_id.to_string(),
            "--force",
            "echo",
            "x",
            "--enter",
        ],
    );
    assert!(forced.status.success(), "{}", stderr(&forced));
}

/// PT-140: the dirty refusal is the server's, not only the CLI's snapshot
/// read. A lease-free `WritePane` into a partial line fails with
/// `InputDirty`; taking the lease (`pmux send --force`) writes; once the
/// line is submitted the lease-free path is open again.
#[test]
fn write_pane_lease_free_is_refused_while_input_dirty_until_force() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt140-gate", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut typist = TestClient::connect(&socket);
    let typist_id = register_client(&mut typist);
    let pane_id = session_window(&typist.snapshot(), "pt140-gate").panes[0].id;
    write_pane_as(&mut typist, typist_id, pane_id, "git com");
    assert!(
        session_window(&typist.snapshot(), "pt140-gate").panes[0]
            .ledger
            .dirty_input,
        "partial write must leave dirty_input"
    );

    let mut robot = TestClient::connect(&socket);
    let robot_id = register_client(&mut robot);
    let refused = robot.try_request(|request_id| ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: robot_id,
        pane_id,
        data: "mit\r".into(),
    });
    match refused {
        Err(error) => assert_eq!(
            error.code,
            ControlErrorCode::InputDirty,
            "server must refuse the lease-free write: {error:?}"
        ),
        Ok(other) => panic!("lease-free write into dirty input must fail, got {other:?}"),
    }

    let forced = umbrella(
        &socket,
        &["send", &pane_id.to_string(), "--force", "mit", "--enter"],
    );
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(
        !session_window(&robot.snapshot(), "pt140-gate").panes[0]
            .ledger
            .dirty_input,
        "a forced write ending in CR submits the line"
    );
    let clean = robot.try_request(|request_id| ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: robot_id,
        pane_id,
        data: "echo PT140\r".into(),
    });
    assert!(clean.is_ok(), "lease-free write after submit: {clean:?}");
}

/// PT-128: `rename-pane` by session name, shown by `ls`, saved into the
/// space file, and restored by `space open` after the session is gone.
#[test]
fn rename_pane_shows_in_ls_and_round_trips_through_a_space() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let created = umbrella(&socket, &["new", "--no-attach", "pt128", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));

    let renamed = umbrella(&socket, &["rename-pane", "pt128", "build", "server"]);
    assert!(renamed.status.success(), "{}", stderr(&renamed));
    assert!(
        stdout(&renamed).contains("set title on pane"),
        "{}",
        stdout(&renamed)
    );
    let listed = umbrella(&socket, &["ls"]);
    assert!(
        stdout(&listed).contains("title \"build server\""),
        "{}",
        stdout(&listed)
    );

    let mut ctl = TestClient::connect(&socket);
    let _ = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt128").panes[0].id;
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "build server"
    );

    // --session takes the name or the opaque id (PT-148: the host knows its
    // attach panes by session id, which must not read as a pane id).
    let session_id = ctl
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.name == "pt128")
        .map(|session| session.id.to_string())
        .expect("pt128 id");
    let by_id = umbrella(
        &socket,
        &["rename-pane", "--session", &session_id, "by", "id"],
    );
    assert!(by_id.status.success(), "{}", stderr(&by_id));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "by id"
    );
    let by_name = umbrella(
        &socket,
        &["rename-pane", "--session", "pt128", "build", "server"],
    );
    assert!(by_name.status.success(), "{}", stderr(&by_name));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "build server"
    );

    let bad = umbrella(&socket, &["rename-pane", "nosuch", "x"]);
    assert!(!bad.status.success());
    assert!(
        stderr(&bad).contains("unknown pane or session"),
        "{}",
        stderr(&bad)
    );

    let save = umbrella_env(&socket, xdg, &["space", "save", "titled"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let file =
        std::fs::read_to_string(data.0.join("prismattyc").join("spaces").join("titled.json"))
            .expect("space file");
    assert!(file.contains("\"title\": \"build server\""), "{file}");

    let cleared = umbrella(&socket, &["rename-pane", &pane_id.to_string()]);
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    assert!(
        stdout(&cleared).contains("cleared title"),
        "{}",
        stdout(&cleared)
    );
    assert_eq!(session_window(&ctl.snapshot(), "pt128").panes[0].title, "");

    // Live unpinned session: `space open` reuses it and restores the saved
    // title (PT-230). A pinned live rename is not clobbered.
    let reopened = umbrella_env(&socket, xdg, &["space", "open", "titled", "--no-attach"]);
    assert!(reopened.status.success(), "{}", stderr(&reopened));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "build server"
    );
    assert!(session_window(&ctl.snapshot(), "pt128").panes[0].title_pinned);

    let live_rename = umbrella(&socket, &["rename-pane", "pt128", "live", "pin"]);
    assert!(live_rename.status.success(), "{}", stderr(&live_rename));
    let skip_pinned = umbrella_env(&socket, xdg, &["space", "open", "titled", "--no-attach"]);
    assert!(skip_pinned.status.success(), "{}", stderr(&skip_pinned));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "live pin",
        "space open must not clobber a pinned live rename"
    );

    // Gone session: `space open` recreates it with the saved title.
    let stopped = umbrella(&socket, &["stop", "pt128"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    let restored = umbrella_env(&socket, xdg, &["space", "open", "titled", "--no-attach"]);
    assert!(restored.status.success(), "{}", stderr(&restored));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt128").panes[0].title,
        "build server"
    );
}

/// PT-150: a leftover all-digit word with `--session` is a pane id, not a title.
#[test]
fn rename_pane_rejects_pane_id_with_session_flag() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt150-sid", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let pane_id = session_window(&ctl.snapshot(), "pt150-sid").panes[0].id;
    let pane = pane_id.to_string();

    let before_flag = umbrella(
        &socket,
        &["rename-pane", &pane, "--session", "pt150-sid", "oops"],
    );
    assert!(!before_flag.status.success(), "{}", stdout(&before_flag));
    assert!(
        stderr(&before_flag).contains("do not pass a pane id with --session"),
        "{}",
        stderr(&before_flag)
    );

    let after_flag = umbrella(
        &socket,
        &["rename-pane", "--session", "pt150-sid", &pane, "oops"],
    );
    assert!(!after_flag.status.success(), "{}", stdout(&after_flag));
    assert!(
        stderr(&after_flag).contains("do not pass a pane id with --session"),
        "{}",
        stderr(&after_flag)
    );
    assert_eq!(
        session_window(&ctl.snapshot(), "pt150-sid").panes[0].title,
        "",
        "a rejected extra pane id must not rename"
    );

    let words = umbrella(
        &socket,
        &["rename-pane", "--session", "pt150-sid", "build", "server"],
    );
    assert!(words.status.success(), "{}", stderr(&words));
    assert_eq!(
        session_window(&ctl.snapshot(), "pt150-sid").panes[0].title,
        "build server"
    );
}

/// PT-150: `--session` and a session-name target both require exactly one pane.
#[test]
fn rename_pane_session_target_rejects_a_split_session() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt150-multi", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let snap = ctl.snapshot();
    let window = session_window(&snap, "pt150-multi");
    let first = window.panes[0].id;
    let _ = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: window.id,
        target_pane_id: first,
        axis: AxisWire::Vertical,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    let after_snap = ctl.snapshot();
    let after = session_window(&after_snap, "pt150-multi");
    assert_eq!(after.panes.len(), 2);
    let second = after.panes[1].id;

    let by_name = umbrella(&socket, &["rename-pane", "pt150-multi", "split"]);
    assert!(!by_name.status.success(), "{}", stdout(&by_name));
    let named_err = stderr(&by_name);
    assert!(
        named_err.contains("has 2 panes") && named_err.contains(&first.to_string()),
        "{named_err}"
    );

    let by_flag = umbrella(
        &socket,
        &["rename-pane", "--session", "pt150-multi", "split"],
    );
    assert!(!by_flag.status.success(), "{}", stdout(&by_flag));
    let flag_err = stderr(&by_flag);
    assert!(
        flag_err.contains("has 2 panes") && flag_err.contains(&second.to_string()),
        "{flag_err}"
    );
    assert_eq!(
        session_window(&ctl.snapshot(), "pt150-multi").panes[0].title,
        ""
    );

    let by_id = umbrella(&socket, &["rename-pane", &first.to_string(), "left"]);
    assert!(by_id.status.success(), "{}", stderr(&by_id));
    let titled_snap = ctl.snapshot();
    let titled = session_window(&titled_snap, "pt150-multi")
        .panes
        .iter()
        .find(|pane| pane.id == first)
        .map(|pane| pane.title.as_str());
    assert_eq!(titled, Some("left"));
}

/// PT-154: the legacy `pmux mail SESSION [--pane ID]` arming verb (cmd_mail,
/// 0 % covered before) sets sticky mail attention on the session's only pane
/// and reports the inject outcome; bad targets exit nonzero without arming.
#[test]
fn mail_session_arming_sets_attention_and_rejects_bad_targets() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt154-mail", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let _ = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt154-mail").panes[0].id;
    assert!(
        session_window(&ctl.snapshot(), "pt154-mail").panes[0]
            .mail
            .is_none(),
        "fresh pane carries no mail attention"
    );

    let armed = umbrella(&socket, &["mail", "pt154-mail"]);
    assert!(armed.status.success(), "{}", stderr(&armed));
    let out = stdout(&armed);
    assert!(
        out.contains("mail armed on session \"pt154-mail\"") && out.contains("inject "),
        "{out}"
    );
    let mail = session_window(&ctl.snapshot(), "pt154-mail").panes[0]
        .mail
        .clone()
        .expect("arming sets attention");
    assert_eq!(mail.pane_id, pane_id);
    assert_eq!(mail.depth, 1);
    assert_eq!(mail.cell, prismattyc_mux::MAIL_ATTENTION_CELL);

    // Explicit --pane with the right id also works; the wrong id and an
    // unknown session are refused before anything is armed.
    let by_pane = umbrella(
        &socket,
        &["mail", "pt154-mail", "--pane", &pane_id.to_string()],
    );
    assert!(by_pane.status.success(), "{}", stderr(&by_pane));
    let wrong_pane = umbrella(&socket, &["mail", "pt154-mail", "--pane", "999999"]);
    assert!(!wrong_pane.status.success());
    assert!(
        stderr(&wrong_pane).contains("is not in session"),
        "{}",
        stderr(&wrong_pane)
    );
    let unknown = umbrella(&socket, &["mail", "pt154-nosuch"]);
    assert!(!unknown.status.success());
    assert!(
        stderr(&unknown).contains("no session matching"),
        "{}",
        stderr(&unknown)
    );
    let twice = umbrella(&socket, &["mail", "pt154-mail", "extra"]);
    assert!(!twice.status.success());
    assert!(
        stderr(&twice).contains("single SESSION"),
        "{}",
        stderr(&twice)
    );
}

/// PT-154: `pmux sync on|off|status` (cmd_sync, 0 % covered before) flips
/// every window of the session and reports it; the global `--session` flag
/// and the positional name both resolve; bad verbs and a missing verb exit
/// nonzero.
#[test]
fn sync_verbs_toggle_every_window_and_report() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt154-sync", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let _ = register_client(&mut ctl);
    assert!(!session_window(&ctl.snapshot(), "pt154-sync").sync_input);

    let status = umbrella(&socket, &["sync", "status", "pt154-sync"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("sync off"), "{}", stdout(&status));

    let on = umbrella(&socket, &["sync", "on", "pt154-sync"]);
    assert!(on.status.success(), "{}", stderr(&on));
    assert!(
        stdout(&on).contains("session pt154-sync: sync on (1 windows)"),
        "{}",
        stdout(&on)
    );
    assert!(session_window(&ctl.snapshot(), "pt154-sync").sync_input);

    // Global --session flag path.
    let status_on = umbrella(&socket, &["--session", "pt154-sync", "sync", "status"]);
    assert!(status_on.status.success(), "{}", stderr(&status_on));
    assert!(
        stdout(&status_on).contains("sync on"),
        "{}",
        stdout(&status_on)
    );

    let off = umbrella(&socket, &["sync", "off", "pt154-sync"]);
    assert!(off.status.success(), "{}", stderr(&off));
    assert!(!session_window(&ctl.snapshot(), "pt154-sync").sync_input);

    let bogus = umbrella(&socket, &["sync", "bogus", "pt154-sync"]);
    assert!(!bogus.status.success());
    assert!(
        stderr(&bogus).contains("unknown sync verb"),
        "{}",
        stderr(&bogus)
    );
    let missing = umbrella(&socket, &["sync"]);
    assert!(!missing.status.success());
    let nosuch = umbrella(&socket, &["sync", "on", "pt154-nosuch"]);
    assert!(!nosuch.status.success());
}

#[test]
fn send_keys_unknown_and_dead_pane_exit_nonzero() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let unknown = umbrella(&socket, &["send", "999999", "x"]);
    assert!(!unknown.status.success());
    assert!(
        stderr(&unknown).contains("unknown pane"),
        "{}",
        stderr(&unknown)
    );

    let created = umbrella(
        &socket,
        &[
            "new",
            "--no-attach",
            "pt126-dead",
            "--",
            "/bin/sh",
            "-c",
            "exit 0",
        ],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt126-dead").panes[0].id;
    let mut dead = false;
    for _ in 0..50 {
        let (alive, _) = read_pane_lines(&mut ctl, client_id, pane_id);
        if !alive {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(dead, "true should exit");
    let sent = umbrella(&socket, &["send", &pane_id.to_string(), "x"]);
    assert!(!sent.status.success());
    assert!(stderr(&sent).contains("dead"), "{}", stderr(&sent));
}

#[test]
fn send_keys_literal_keeps_backslash_n() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt126-lit", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt126-lit").panes[0].id;
    let sent = umbrella(
        &socket,
        &["send", &pane_id.to_string(), "--literal", r"PT126LIT\n"],
    );
    assert!(sent.status.success(), "{}", stderr(&sent));
    let mut found = false;
    for _ in 0..50 {
        let (_, text) = read_pane_lines(&mut ctl, client_id, pane_id);
        if text.contains(r"PT126LIT\n") {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "literal backslash-n missing from pane");
}

#[test]
fn break_pane_moves_into_a_new_window_and_join_pane_puts_it_back() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(&socket, &["new", "--no-attach", "pt131", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let _ = register_client(&mut ctl);
    let snap = ctl.snapshot();
    let window = session_window(&snap, "pt131");
    let from_window = window.id;
    let first = window.panes[0].id;
    let _ = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: from_window,
        target_pane_id: first,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    let snap = ctl.snapshot();
    let window = session_window(&snap, "pt131");
    assert_eq!(window.panes.len(), 2);
    let second = window
        .panes
        .iter()
        .find(|pane| pane.id != first)
        .expect("split pane")
        .id;
    let broke = umbrella(&socket, &["break-pane", &second.to_string()]);
    assert!(broke.status.success(), "{}", stderr(&broke));
    let snap = ctl.snapshot();
    let session = snap
        .sessions
        .iter()
        .find(|session| session.name == "pt131")
        .expect("session");
    assert_eq!(session.windows.len(), 2, "break-pane must add a window");
    let dest = session
        .windows
        .iter()
        .find(|window| window.panes.iter().any(|pane| pane.id == second))
        .expect("broken window");
    assert_eq!(dest.panes.len(), 1, "broken window holds only the pane");
    let already = umbrella(&socket, &["break-pane", &second.to_string()]);
    assert!(already.status.success(), "{}", stderr(&already));
    assert!(
        stdout(&already).contains("already its own window"),
        "{}",
        stdout(&already)
    );

    let joined = umbrella(
        &socket,
        &[
            "join-pane",
            &second.to_string(),
            "--to",
            &from_window.to_string(),
            "-v",
        ],
    );
    assert!(joined.status.success(), "{}", stderr(&joined));
    let snap = ctl.snapshot();
    let session = snap
        .sessions
        .iter()
        .find(|session| session.name == "pt131")
        .expect("session");
    assert_eq!(session.windows.len(), 1);
    assert_eq!(session.windows[0].panes.len(), 2);

    let other = umbrella(&socket, &["new", "--no-attach", "pt131-b", "--", "/bin/sh"]);
    assert!(other.status.success(), "{}", stderr(&other));
    let snap = ctl.snapshot();
    let other_window = session_window(&snap, "pt131-b").id;
    let pane = session_window(&snap, "pt131").panes[0].id;
    let cross = umbrella(
        &socket,
        &[
            "join-pane",
            &pane.to_string(),
            "--to",
            &other_window.to_string(),
        ],
    );
    assert!(!cross.status.success(), "cross-session --to must fail");
    assert!(
        stderr(&cross).contains("not a window in pane"),
        "{}",
        stderr(&cross)
    );
    let unknown = umbrella(&socket, &["join-pane", &pane.to_string(), "--to", "999999"]);
    assert!(!unknown.status.success());
    assert!(
        stderr(&unknown).contains("unknown window"),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn space_open_skips_existing_foreground_lease_and_linebreak() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt93-live", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            (procinfo::TEST_FOREGROUND_COMMAND_ENV, "sleep 30".into()),
        ],
        &["space", "save", "live"],
    );
    assert!(save.status.success(), "{}", stderr(&save));
    let save_path = stdout(&save).trim().to_string();

    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt93-live").panes[0].id;
    write_pane_as(&mut ctl, client_id, pane_id, "sleep 30\r");
    let mut ready = false;
    for _ in 0..40 {
        let snapshot = ctl.snapshot();
        let pid = session_window(&snapshot, "pt93-live").panes[0].child_pid;
        if pid
            .and_then(procinfo::live_foreground_command)
            .is_some_and(|cmd| cmd.contains("sleep"))
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "live sleep did not start");

    let open_live = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "live", "--no-attach"],
    );
    assert!(open_live.status.success(), "{}", stderr(&open_live));
    let live_out = stdout(&open_live);
    assert!(
        live_out.contains("already exists"),
        "existing foreground must skip: {live_out}"
    );
    assert!(
        !live_out.contains("ran sleep 30 in pt93-live"),
        "must not type over a live CLI: {live_out}"
    );

    let stop = umbrella(&socket, &["stop", "pt93-live"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt93-live", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));

    let mut holder = TestClient::connect(&socket);
    let holder_id = register_client(&mut holder);
    let pane_id = session_window(&holder.snapshot(), "pt93-live").panes[0].id;
    let _ = holder.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: holder_id,
        pane_id,
    });
    let lease_open = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "live", "--no-attach"],
    );
    assert!(lease_open.status.success(), "{}", stderr(&lease_open));
    let lease_out = format!("{}{}", stdout(&lease_open), stderr(&lease_open));
    assert!(
        lease_out.contains("already exists"),
        "held lease must skip write: {lease_out}"
    );
    assert!(
        !lease_out.contains("ran sleep 30 in pt93-live"),
        "must not write while lease is held: {lease_out}"
    );
    // Claiming the recreated session revokes its previous unscoped controller.
    let snapshot = holder.snapshot();
    assert!(session_window(&snapshot, "pt93-live").panes[0]
        .controller_id
        .is_none());

    let raw = std::fs::read_to_string(&save_path).expect("read space");
    let broken = raw.replace(
        "\"command\": \"sleep 30\"",
        "\"command\": \"sleep 30\\nbad\"",
    );
    std::fs::write(&save_path, broken).expect("write linebreak command");
    let stop = umbrella(&socket, &["stop", "pt93-live"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    let linebreak = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "live", "--no-attach"],
    );
    assert!(linebreak.status.success(), "{}", stderr(&linebreak));
    let br_out = format!("{}{}", stdout(&linebreak), stderr(&linebreak));
    assert!(
        br_out.contains("line break"),
        "CR/LF in saved command must skip: {br_out}"
    );
    assert!(
        !br_out.contains("ran sleep"),
        "must not run linebreak: {br_out}"
    );
}

#[test]
fn space_open_reuses_owned_session_without_replaying_command() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "pt93-race", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_vars(
        &socket,
        &[
            ("XDG_DATA_HOME", data.0.as_os_str().to_os_string()),
            (procinfo::TEST_FOREGROUND_COMMAND_ENV, "sleep 30".into()),
        ],
        &["space", "save", "race"],
    );
    assert!(save.status.success(), "{}", stderr(&save));

    let socket_thread = socket.clone();
    let started = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        let mut ctl = TestClient::connect(&socket_thread);
        let client_id = register_client(&mut ctl);
        let pane_id = session_window(&ctl.snapshot(), "pt93-race").panes[0].id;
        write_pane_as(&mut ctl, client_id, pane_id, "sleep 30\r");
    });

    let open = umbrella_env(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_path())],
        &["space", "open", "race", "--no-attach"],
    );
    let _ = started.join();
    assert!(open.status.success(), "{}", stderr(&open));
    let out = stdout(&open);
    assert!(
        out.contains("already exists"),
        "existing session must skip command replay: {out}"
    );
    assert!(
        !out.contains("ran sleep 30 in pt93-race"),
        "must not type after a late start: {out}"
    );
}

#[test]
fn space_open_reuses_fake_host_and_skips_host_line_without_pid() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let created = umbrella(&socket, &["new", "--no-attach", "pt65", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_env(&socket, xdg, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let none = umbrella_env(&socket, xdg, &["space", "open", "today", "--no-attach"]);
    assert!(none.status.success(), "{}", stderr(&none));
    let none_out = stdout(&none);
    assert!(
        !none_out.contains("reused host pid"),
        "no live host: {none_out}"
    );
    assert!(
        !none_out.contains("opened host pid"),
        "no-attach must not spawn: {none_out}"
    );

    let pid_path = host_pid_path_from_socket(&socket);
    let ack_path = host_ack_path_from_socket(&socket);
    std::fs::write(&pid_path, format!("{}\n", std::process::id())).unwrap();
    let ack = ack_path.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_waiter = Arc::clone(&stop);
    let waiter = std::thread::spawn(move || {
        while !stop_waiter.load(Ordering::Relaxed) {
            let _ = touch_host_ack(&ack);
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    // Act has no display. Reuse must still win over the TTY recipe.
    let reused = umbrella_vars_cleared(
        &socket,
        &[("XDG_DATA_HOME", data.0.as_os_str().to_os_string())],
        &["DISPLAY", "WAYLAND_DISPLAY"],
        &["space", "open", "today"],
    );
    stop.store(true, Ordering::Relaxed);
    let _ = waiter.join();
    assert!(reused.status.success(), "{}", stderr(&reused));
    let reused_out = stdout(&reused);
    assert!(
        reused_out.contains("reused host pid "),
        "fake live pid must reuse: {reused_out}"
    );
    assert!(
        !reused_out.contains("opened host pid"),
        "must not spawn a second host: {reused_out}"
    );
    assert!(
        !reused_out
            .lines()
            .any(|line| line.starts_with("pmux attach ")),
        "live host must not print the TTY recipe: {reused_out}"
    );
}

#[test]
fn space_open_switch_rejects_shared_save_and_cross_space_add() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let a = umbrella(&socket, &["new", "--no-attach", "pt213-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "--no-attach", "pt213-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));
    let save_a = umbrella_env(&socket, xdg, &["space", "save", "alpha", "pt213-a"]);
    assert!(save_a.status.success(), "{}", stderr(&save_a));
    let save_b = umbrella_env(&socket, xdg, &["space", "save", "beta", "pt213-b"]);
    assert!(save_b.status.success(), "{}", stderr(&save_b));

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt213-a")
        .map(|session| session.id.to_string())
        .expect("pt213-a");
    let id_b = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt213-b")
        .map(|session| session.id.to_string())
        .expect("pt213-b");
    attach_tabs::save(
        &attach_tabs::layout_path_from_socket(&socket),
        &attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "alpha".into(),
                sessions: vec![id_a.clone()],
            }],
            space: Some("alpha".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let pid_path = host_pid_path_from_socket(&socket);
    let ack_path = host_ack_path_from_socket(&socket);
    std::fs::write(&pid_path, format!("{}\n", std::process::id())).unwrap();
    let ack = ack_path.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_waiter = Arc::clone(&stop);
    let waiter = std::thread::spawn(move || {
        while !stop_waiter.load(Ordering::Relaxed) {
            let _ = touch_host_ack(&ack);
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let switched = umbrella_env(&socket, xdg, &["space", "open", "beta", "--no-attach"]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    let switched_out = stdout(&switched);
    assert!(
        switched_out.contains("switched from alpha to beta"),
        "switch report: {switched_out}"
    );
    let cache = attach_tabs::load(&attach_tabs::layout_path_from_socket(&socket)).expect("cache");
    assert_eq!(cache.space.as_deref(), Some("beta"));
    assert_eq!(cache.mode, attach_tabs::AttachTabsMode::Switch);

    let probe = umbrella_env(&socket, xdg, &["space", "save", "probe"]);
    assert!(
        !probe.status.success(),
        "Save As must not alias owned sessions"
    );
    assert!(!data.0.join("prismattyc/spaces/probe.json").exists());
    let added = umbrella_env(
        &socket,
        xdg,
        &["space", "open", "alpha", "--add", "--no-attach"],
    );
    stop.store(true, Ordering::Relaxed);
    let _ = waiter.join();
    assert!(!added.status.success(), "cross-Space --add must fail");
    let cache = attach_tabs::load(&attach_tabs::layout_path_from_socket(&socket)).expect("cache");
    assert_eq!(cache.space.as_deref(), Some("beta"));
    assert_eq!(cache.mode, attach_tabs::AttachTabsMode::Switch);
    let cache_ids: Vec<String> = cache
        .tabs
        .iter()
        .flat_map(|tab| tab.sessions.iter().cloned())
        .collect();
    assert_eq!(cache_ids, vec![id_b]);
    assert!(!cache_ids.contains(&id_a));
}

#[cfg(target_os = "linux")]
#[test]
fn space_open_headless_without_host_prints_attach_recipe() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let extra = [("XDG_DATA_HOME", data.0.as_os_str().to_os_string())];
    let created = umbrella(&socket, &["new", "--no-attach", "pt137", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_vars(&socket, &extra, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let open = umbrella_vars_cleared(
        &socket,
        &extra,
        &["DISPLAY", "WAYLAND_DISPLAY"],
        &["space", "open", "today"],
    );
    assert!(open.status.success(), "{}", stderr(&open));
    let text = format!("{}{}", stdout(&open), stderr(&open));
    assert!(
        text.lines()
            .any(|line| line.starts_with("pmux attach pt137")),
        "no live host must print the TTY recipe: {text}"
    );
    assert!(!text.contains("reused host pid"), "no pid file: {text}");
    assert!(
        !text.contains("opened host pid"),
        "headless must not spawn a host: {text}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn space_open_new_window_wins_over_no_display_recipe() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let extra = [("XDG_DATA_HOME", data.0.as_os_str().to_os_string())];
    let created = umbrella(&socket, &["new", "--no-attach", "pt137w", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_vars(&socket, &extra, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let open = umbrella_vars_cleared(
        &socket,
        &extra,
        &["DISPLAY", "WAYLAND_DISPLAY"],
        &["space", "open", "today", "--new-window"],
    );
    assert!(open.status.success(), "{}", stderr(&open));
    let text = format!("{}{}", stdout(&open), stderr(&open));
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("pmux attach pt137w")),
        "--new-window must not print the space TTY recipe: {text}"
    );
    assert!(
        text.contains("not opening a window") || text.contains("opened host pid"),
        "--new-window takes the host spawn path: {text}"
    );
}

#[test]
fn space_save_copies_fake_attach_tabs_cache() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let grok = umbrella(&socket, &["new", "pt67-a", "--", "/bin/sh"]);
    assert!(grok.status.success(), "{}", stderr(&grok));
    let fable = umbrella(&socket, &["new", "pt67-b", "--", "/bin/sh"]);
    assert!(fable.status.success(), "{}", stderr(&fable));

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt67-a")
        .map(|session| session.id.to_string())
        .expect("pt67-a");
    let id_b = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt67-b")
        .map(|session| session.id.to_string())
        .expect("pt67-b");

    let cache = attach_tabs::layout_path_from_socket(&socket);
    attach_tabs::save(
        &cache,
        &attach_tabs::AttachTabsFile {
            tabs: vec![
                attach_tabs::AttachTabRecord {
                    title: "seats".into(),
                    sessions: vec![id_a.clone(), id_b.clone()],
                },
                attach_tabs::AttachTabRecord {
                    title: "solo".into(),
                    sessions: vec![id_b.clone()],
                },
            ],
            active_tab: 1,
            focused_session: Some(id_b),
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        },
    )
    .expect("write attach-tabs cache");

    let save = umbrella_env(&socket, xdg, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let save_out = stdout(&save);
    let save_path = save_out
        .lines()
        .find(|line| line.contains("spaces/today.json"))
        .unwrap_or(save_out.trim())
        .to_string();
    let raw = std::fs::read_to_string(&save_path).expect("read space file");
    assert!(raw.contains("\"seats\""), "{raw}");
    assert!(raw.contains("\"pt67-a\""), "{raw}");
    assert!(raw.contains("\"pt67-b\""), "{raw}");
    let space: serde_json::Value = serde_json::from_str(&raw).expect("space json");
    let tabs = space["tabs"].as_array().expect("tabs array");
    assert_eq!(tabs.len(), 2, "{raw}");
    assert_eq!(tabs[0]["title"], "seats");
    assert_eq!(tabs[0]["sessions"][0], "pt67-a");
    assert_eq!(tabs[0]["sessions"][1], "pt67-b");
    assert_eq!(tabs[1]["title"], "solo");
    assert_eq!(space["active_tab"], 1);
    assert_eq!(space["focused_session"], "pt67-b");
}

#[test]
fn space_save_tabs_a_live_session_the_cache_omits() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let a = umbrella(&socket, &["new", "pt108-a", "--", "/bin/sh"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = umbrella(&socket, &["new", "pt108-b", "--", "/bin/sh"]);
    assert!(b.status.success(), "{}", stderr(&b));

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt108-a")
        .map(|session| session.id.to_string())
        .expect("pt108-a");

    let cache = attach_tabs::layout_path_from_socket(&socket);
    attach_tabs::save(
        &cache,
        &attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "one".into(),
                sessions: vec![id_a],
            }],
            active_tab: 0,
            focused_session: None,
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        },
    )
    .expect("write attach-tabs cache");

    let save = umbrella_env(&socket, xdg, &["space", "save", "partial"]);
    assert!(save.status.success(), "{}", stderr(&save));
    // No-list save follows the live window (cache). A live session the
    // cache omits is not saved (PT-213). Explicit SESSION still includes it.
    let err = stderr(&save);
    assert!(
        !err.contains("pt108-b"),
        "window-scoped save must not mention the omitted session: {err}"
    );
    let save_out = stdout(&save);
    assert!(
        save_out.contains("saved 1 session from the live window"),
        "window-scoped save line: {save_out}"
    );
    let save_path = save_out
        .lines()
        .find(|line| line.contains("spaces/partial.json"))
        .unwrap_or(save_out.trim())
        .to_string();
    let raw = std::fs::read_to_string(&save_path).expect("read space file");
    let space: serde_json::Value = serde_json::from_str(&raw).expect("space json");
    let tabs = space["tabs"].as_array().expect("tabs array");
    assert_eq!(tabs.len(), 1, "only the cache tab: {raw}");
    assert_eq!(tabs[0]["title"], "one");
    assert_eq!(tabs[0]["sessions"][0], "pt108-a");
    let names: Vec<&str> = space["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pt108-a"], "{raw}");

    let both = umbrella_env(
        &socket,
        xdg,
        &["space", "save", "partial", "pt108-a", "pt108-b"],
    );
    assert!(both.status.success(), "{}", stderr(&both));
    let both_err = stderr(&both);
    assert!(
        both_err.contains("note:") && both_err.contains("pt108-b") && both_err.contains("own tab"),
        "explicit list still notes the untabbed session: {both_err}"
    );
}

#[test]
fn space_open_restores_two_tabs_three_plus_two() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let names = ["pt108-t1", "pt108-t2", "pt108-t3", "pt108-t4", "pt108-t5"];
    for name in names {
        let created = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let ids: Vec<String> = names
        .iter()
        .map(|name| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.name == *name)
                .map(|session| session.id.to_string())
                .unwrap_or_else(|| panic!("{name} id"))
        })
        .collect();

    let cache = attach_tabs::layout_path_from_socket(&socket);
    attach_tabs::save(
        &cache,
        &attach_tabs::AttachTabsFile {
            tabs: vec![
                attach_tabs::AttachTabRecord {
                    title: "PRISMATTYC".into(),
                    sessions: vec![ids[0].clone(), ids[1].clone(), ids[2].clone()],
                },
                attach_tabs::AttachTabRecord {
                    title: "WEBSITE".into(),
                    sessions: vec![ids[3].clone(), ids[4].clone()],
                },
            ],
            active_tab: 1,
            focused_session: Some(ids[4].clone()),
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        },
    )
    .expect("write two-tab cache");

    let save = umbrella_env(&socket, xdg, &["space", "save", "web"]);
    assert!(save.status.success(), "{}", stderr(&save));
    attach_tabs::save(&cache, &attach_tabs::AttachTabsFile::default()).expect("clear cache");

    let open = umbrella_env(&socket, xdg, &["space", "open", "web", "--no-attach"]);
    assert!(open.status.success(), "{}", stderr(&open));
    let loaded = attach_tabs::load(&cache).expect("cache restored");
    assert_eq!(loaded.tabs.len(), 2, "{loaded:?}");
    assert_eq!(loaded.tabs[0].title, "PRISMATTYC");
    assert_eq!(
        loaded.tabs[0].sessions,
        vec![ids[0].clone(), ids[1].clone(), ids[2].clone()]
    );
    assert_eq!(loaded.tabs[1].title, "WEBSITE");
    assert_eq!(
        loaded.tabs[1].sessions,
        vec![ids[3].clone(), ids[4].clone()]
    );
    assert_eq!(loaded.active_tab, 1);
    assert_eq!(loaded.focused_session.as_deref(), Some(ids[4].as_str()));
}

#[test]
fn space_open_tabless_space_replaces_stale_cache_with_one_tab_per_session() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let grok = umbrella(&socket, &["new", "pt66-empty-a", "--", "/bin/sh"]);
    assert!(grok.status.success(), "{}", stderr(&grok));
    let save = umbrella_env(&socket, xdg, &["space", "save", "notabs"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let cache = attach_tabs::layout_path_from_socket(&socket);
    attach_tabs::save(
        &cache,
        &attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "stale".into(),
                sessions: vec!["99".into()],
            }],
            active_tab: 1,
            focused_session: Some("99".into()),
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        },
    )
    .expect("write stale cache");

    let open = umbrella_env(&socket, xdg, &["space", "open", "notabs", "--no-attach"]);
    assert!(open.status.success(), "{}", stderr(&open));
    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let live_id = ctl
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.name == "pt66-empty-a")
        .map(|session| session.id.to_string())
        .expect("pt66-empty-a is live");
    // PT-142: a space with no tabs section opens one tab per session, so a
    // live host regroups to what a fresh host would spawn; the stale record
    // is gone.
    let loaded = attach_tabs::load(&cache).expect("cache rewritten");
    assert_eq!(
        loaded.tabs,
        vec![attach_tabs::AttachTabRecord {
            title: "pt66-empty-a".into(),
            sessions: vec![live_id],
        }],
        "{loaded:?}"
    );
    assert_eq!(loaded.active_tab, 0);
    assert_eq!(loaded.focused_session, None);
    assert_eq!(loaded.space.as_deref(), Some("notabs"));
}

#[test]
fn space_open_tty_prints_attach_recipe_in_tab_order() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["pt99-a", "pt99-b"] {
        let created = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let snapshot = ctl.snapshot();
    let id_a = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt99-a")
        .map(|session| session.id.to_string())
        .expect("pt99-a");
    let id_b = snapshot
        .sessions
        .iter()
        .find(|session| session.name == "pt99-b")
        .map(|session| session.id.to_string())
        .expect("pt99-b");
    let cache = attach_tabs::layout_path_from_socket(&socket);
    attach_tabs::save(
        &cache,
        &attach_tabs::AttachTabsFile {
            tabs: vec![attach_tabs::AttachTabRecord {
                title: "seats".into(),
                sessions: vec![id_b.clone(), id_a.clone()],
            }],
            active_tab: 0,
            focused_session: Some(id_a),
            space: None,
            mode: attach_tabs::AttachTabsMode::Add,
            ..Default::default()
        },
    )
    .expect("write cache");
    let save = umbrella_env(&socket, xdg, &["space", "save", "today"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let open = umbrella_env(
        &socket,
        xdg,
        &["space", "open", "today", "--tty", "--no-attach"],
    );
    assert!(open.status.success(), "{}", stderr(&open));
    let text = stdout(&open);
    let recipe: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("pmux attach "))
        .collect();
    assert_eq!(
        recipe,
        vec!["pmux attach pt99-b", "pmux attach pt99-a  # active"],
        "{text}"
    );
}

#[test]
fn space_attach_selects_session_and_refuses_without_tty() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["pt99-x", "pt99-y"] {
        let created = umbrella(&socket, &["new", "--no-attach", name, "--", "/bin/sh"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let save = umbrella_env(&socket, xdg, &["space", "save", "ssh"]);
    assert!(save.status.success(), "{}", stderr(&save));

    let missing = umbrella_env(
        &socket,
        xdg,
        &["space", "attach", "ssh", "--session", "nope"],
    );
    assert!(!missing.status.success(), "unknown session must fail");
    let err = format!("{}{}", stdout(&missing), stderr(&missing));
    assert!(
        err.contains("nope") && err.contains("ssh"),
        "must name the missing session: {err}"
    );

    let ok_session = umbrella_env(
        &socket,
        xdg,
        &["space", "attach", "ssh", "--session", "pt99-y"],
    );
    assert!(
        !ok_session.status.success(),
        "space attach off a TTY must fail"
    );
    let err = format!("{}{}", stdout(&ok_session), stderr(&ok_session));
    assert!(
        err.contains("requires a TTY"),
        "must refuse non-TTY attach: {err}"
    );
    assert!(
        !err.contains("reused host pid"),
        "TTY attach must not regroup a live host: {err}"
    );
    let cache = attach_tabs::layout_path_from_socket(&socket);
    if cache.exists() {
        let loaded = attach_tabs::load(&cache).expect("cache");
        assert_ne!(
            loaded.space.as_deref(),
            Some("ssh"),
            "space attach must not write the attach-tabs cache: {loaded:?}"
        );
    }
}

#[test]
fn space_cli_termwright_save_ls_open_no_attach() {
    let termwright = std::env::var_os("TERMWRIGHT_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path).find_map(|dir| {
                    let candidate = dir.join("termwright");
                    candidate.is_file().then_some(candidate)
                })
            })
        })
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let candidate = PathBuf::from(home).join(".cargo/bin/termwright");
            candidate.is_file().then_some(candidate)
        })
        .filter(|path| path.is_file());
    let Some(termwright) = termwright else {
        eprintln!("skip: termwright not installed (CI image); umbrella_cli covers save/ls/open");
        return;
    };

    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let grok = umbrella(&socket, &["new", "pt64-tw-a", "--", "/bin/sh"]);
    assert!(grok.status.success(), "{}", stderr(&grok));
    let fable = umbrella(&socket, &["new", "pt64-tw-b", "--", "/bin/sh"]);
    assert!(fable.status.success(), "{}", stderr(&fable));

    let pmux = env!("CARGO_BIN_EXE_pmux");
    let script = format!(
        "set -e; {pmux} --socket {sock} space save today; {pmux} --socket {sock} space ls; {pmux} --socket {sock} space open today --no-attach",
        sock = socket.display(),
    );
    // `termwright run-steps` hits a local IPC spawn race in 0.2.0; `run`
    // still wraps a PTY and is the gate. Wait for the last command's skip
    // line so the capture includes save, ls, and open, not only save.
    let mut command = Command::new(&termwright);
    clear_command_env(&mut command);
    let out = command
        .args([
            "run",
            "--cols",
            "100",
            "--rows",
            "24",
            "--wait-for",
            "skip pt64-tw-a (already exists)",
            "--format",
            "text",
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .env("XDG_DATA_HOME", &data.0)
        .env("PMUX_SERVER", env!("CARGO_BIN_EXE_pmuxd"))
        .env("PMUX_ATTACH", env!("CARGO_BIN_EXE_pmux-attach"))
        .output()
        .expect("run termwright");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "termwright failed: {text}");
    assert!(text.contains("spaces/today.json"), "{text}");
    assert!(text.contains("today"), "{text}");
    assert!(
        text.contains("skip pt64-tw-a (already exists)"),
        "termwright must capture open, not only save: {text}"
    );
}

#[test]
fn help_termwright_names_session_tab_space() {
    let termwright = std::env::var_os("TERMWRIGHT_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path).find_map(|dir| {
                    let candidate = dir.join("termwright");
                    candidate.is_file().then_some(candidate)
                })
            })
        })
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let candidate = PathBuf::from(home).join(".cargo/bin/termwright");
            candidate.is_file().then_some(candidate)
        })
        .filter(|path| path.is_file());
    let Some(termwright) = termwright else {
        eprintln!("skip: termwright not installed (CI image); top_help_opens_with_session_tab_space covers --help");
        return;
    };

    let pmux = env!("CARGO_BIN_EXE_pmux");
    let script = format!("set -e; {pmux} --help; {pmux} space --help");
    let mut command = Command::new(&termwright);
    clear_command_env(&mut command);
    let out = command
        .args([
            "run",
            "--cols",
            "80",
            "--rows",
            "40",
            "--wait-for",
            "only one Space",
            "--format",
            "text",
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .output()
        .expect("run termwright");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "termwright failed: {text}");
    let lower = text.to_ascii_lowercase();
    for noun in ["session", "tab", "space"] {
        assert!(lower.contains(noun), "help PTY missing {noun}: {text}");
    }
    assert!(
        lower.contains("only one space"),
        "help PTY must describe exclusive ownership: {text}"
    );
}

#[test]
fn layout_apply_all_binds_agent_and_skips_existing() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];

    let created = umbrella(&socket, &["new", "pt56-all", "--", "/bin/sh"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let save = umbrella_env(&socket, xdg, &["layout", "save", "pt56-all"]);
    assert!(save.status.success(), "{}", stderr(&save));
    let stop = umbrella(&socket, &["stop", "pt56-all"]);
    assert!(stop.status.success(), "{}", stderr(&stop));

    let apply = umbrella_env(&socket, xdg, &["layout", "apply", "--all"]);
    assert!(apply.status.success(), "{}", stderr(&apply));
    assert!(
        stdout(&apply).contains("created pt56-all"),
        "{}",
        stdout(&apply)
    );

    let mut ctl = TestClient::connect(&socket);
    let restored = ctl.snapshot();
    let alpha = restored
        .sessions
        .iter()
        .find(|session| session.name == "pt56-all")
        .expect("pt56-all after apply --all");
    assert_eq!(alpha.agent_id.as_deref(), Some("pt56-all"));

    let again = umbrella_env(&socket, xdg, &["layout", "apply", "--all"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("skip pt56-all (already exists)"),
        "{}",
        stdout(&again)
    );
    let skipped = ctl.snapshot();
    let alpha_after = skipped
        .sessions
        .iter()
        .find(|session| session.name == "pt56-all")
        .expect("pt56-all still present");
    assert_eq!(alpha_after.windows.len(), 1);
}

/// PT-116: `pmux save-buffer` writes ReadPane screen text; `-` is stdout;
/// `--history` is accepted; a missing target and a missing file fail.
#[test]
fn save_buffer_writes_readpane_text() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &[
            "new",
            "--no-attach",
            "pt116-save",
            "--",
            "/bin/sh",
            "-c",
            "printf 'PT116-SAVE\\n'; exec sleep 30",
        ],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt116-save").panes[0].id;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let content = match ctl.request(|request_id| ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        }) {
            ControlResponseData::PaneContent { content } => content,
            other => panic!("expected pane content, got {other:?}"),
        };
        if content.lines.join("\n").contains("PT116-SAVE") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("pane never printed PT116-SAVE: {:?}", content.lines);
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let dir = std::env::temp_dir().join(format!("pt116-save-{}-{}", std::process::id(), pane_id));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("buffer.txt");
    let saved = umbrella(
        &socket,
        &[
            "save-buffer",
            "pt116-save",
            file.to_str().expect("utf8 temp path"),
        ],
    );
    assert!(saved.status.success(), "{}", stderr(&saved));
    let dumped = std::fs::read_to_string(&file).expect("read save-buffer file");
    assert!(
        dumped.contains("PT116-SAVE"),
        "file missing screen text: {dumped:?}"
    );

    let stdout_dump = umbrella(&socket, &["save-buffer", "pt116-save", "-"]);
    assert!(stdout_dump.status.success(), "{}", stderr(&stdout_dump));
    assert!(
        stdout(&stdout_dump).contains("PT116-SAVE"),
        "{}",
        stdout(&stdout_dump)
    );

    let with_history = umbrella(
        &socket,
        &[
            "save-buffer",
            "--history",
            "pt116-save",
            file.to_str().expect("utf8 temp path"),
        ],
    );
    assert!(with_history.status.success(), "{}", stderr(&with_history));

    let missing = umbrella(&socket, &["save-buffer"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("save-buffer"),
        "{}",
        stderr(&missing)
    );
    let nosuch = umbrella(&socket, &["save-buffer", "pt116-nosuch", "-"]);
    assert!(!nosuch.status.success());
    let _ = std::fs::remove_dir_all(&dir);

    let pipe_help = umbrella(&socket, &["pipe-pane", "--help"]);
    assert!(pipe_help.status.success(), "{}", stderr(&pipe_help));
    assert!(
        stdout(&pipe_help).contains("Subscribe from the current pane-log seq"),
        "{}",
        stdout(&pipe_help)
    );
    let pipe_missing = umbrella(&socket, &["pipe-pane", "pt116-save"]);
    assert!(!pipe_missing.status.success());
}

fn umbrella_spawn(socket: &Path, args: &[&str]) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    command
        .env("PMUX_SERVER", env!("CARGO_BIN_EXE_pmuxd"))
        .env("PMUX_ATTACH", env!("CARGO_BIN_EXE_pmux-attach"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pmux")
}

fn wait_file_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        if Instant::now() >= deadline {
            panic!("{} never contained {needle:?}: {text:?}", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct SpawnGuard(Option<std::process::Child>);

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// PT-116: pipe-pane streams from the current seq (BEFORE is absent,
/// AFTER is present), stops when the pane exits, and `--exec` cleanup
/// does not hang on a child that ignores stdin EOF.
#[test]
fn pipe_pane_streams_from_current_seq_and_reaps_exec() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &[
            "new",
            "--no-attach",
            "pt116-pipe",
            "--",
            "/bin/sh",
            "-c",
            "printf 'PT116-BEFORE\\n'; while IFS= read -r line; do printf 'ECHO:%s\\n' \"$line\"; done",
        ],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client_id = register_client(&mut ctl);
    let pane_id = session_window(&ctl.snapshot(), "pt116-pipe").panes[0].id;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = read_pane_lines(&mut ctl, client_id, pane_id).1;
        if text.contains("PT116-BEFORE") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("pane never printed PT116-BEFORE: {text}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let dir = std::env::temp_dir().join(format!("pt116-pipe-{}-{}", std::process::id(), pane_id));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("stream.log");
    let mut pipe = SpawnGuard(Some(umbrella_spawn(
        &socket,
        &[
            "pipe-pane",
            "pt116-pipe",
            file.to_str().expect("utf8 temp path"),
        ],
    )));
    std::thread::sleep(Duration::from_millis(150));
    write_pane_as(&mut ctl, client_id, pane_id, "PT116-AFTER\n");
    let dumped = wait_file_contains(&file, "ECHO:PT116-AFTER", Duration::from_secs(3));
    assert!(
        !dumped.contains("PT116-BEFORE"),
        "pipe-pane must start at current seq, not dump retained Output: {dumped:?}"
    );

    let exec_out = dir.join("exec.log");
    let exec_cmd = format!("cat > {}", exec_out.display());
    let mut exec_pipe = SpawnGuard(Some(umbrella_spawn(
        &socket,
        &["pipe-pane", "pt116-pipe", "--exec", &exec_cmd],
    )));
    std::thread::sleep(Duration::from_millis(150));
    write_pane_as(&mut ctl, client_id, pane_id, "PT116-EXEC\n");
    let _ = wait_file_contains(&exec_out, "ECHO:PT116-EXEC", Duration::from_secs(3));

    let stopped = umbrella(&socket, &["--session", "pt116-pipe", "stop"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));

    let started = Instant::now();
    let status = pipe
        .0
        .take()
        .expect("pipe child")
        .wait()
        .expect("wait pipe-pane");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "pipe-pane must exit after pane stop"
    );
    assert!(status.success(), "pipe-pane file sink exit {status}");

    let exec_started = Instant::now();
    let exec_status = exec_pipe
        .0
        .take()
        .expect("exec child")
        .wait()
        .expect("wait pipe-pane --exec");
    assert!(
        exec_started.elapsed() < Duration::from_secs(3),
        "pipe-pane --exec cat must exit on pane stop"
    );
    assert!(
        exec_status.success(),
        "pipe-pane --exec cat exit {exec_status}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exclusive_spaces_create_add_move_and_reject_stale_writer() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let a_view = data.0.join("a-view.json");
    let b_view = data.0.join("b-view.json");
    for (name, view) in [("alpha", &a_view), ("beta", &b_view)] {
        let result = umbrella_env(
            &socket,
            xdg,
            &[
                "space",
                "create",
                name,
                "--no-attach",
                "--view-path",
                view.to_str().unwrap(),
            ],
        );
        assert!(
            result.status.success(),
            "{}{}",
            stdout(&result),
            stderr(&result)
        );
    }
    let dir = data.0.join("prismattyc/spaces");
    let alpha = prismattyc_mux::load_space(&dir, "alpha").unwrap();
    let beta = prismattyc_mux::load_space(&dir, "beta").unwrap();
    assert_ne!(alpha.id, beta.id);
    let mut ctl = TestClient::connect(&socket);
    let client = register_client(&mut ctl);
    let snap = ctl.snapshot();
    let a = snap
        .sessions
        .iter()
        .find(|s| s.space_id == alpha.id)
        .unwrap();
    let b = snap
        .sessions
        .iter()
        .find(|s| s.space_id == beta.id)
        .unwrap();
    let ap = &a.windows[0].panes[0];
    let bp = &b.windows[0].panes[0];
    assert_ne!(a.id, b.id);
    assert_ne!(ap.id, bp.id);
    assert_ne!(ap.child_pid, bp.child_pid);
    write_pane_as(&mut ctl, client, ap.id, "printf 'ONLY_ALPHA_366\\n'\r");
    let until = Instant::now() + Duration::from_secs(3);
    while !read_pane_lines(&mut ctl, client, ap.id)
        .1
        .contains("ONLY_ALPHA_366")
    {
        assert!(Instant::now() < until, "alpha output never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!read_pane_lines(&mut ctl, client, bp.id)
        .1
        .contains("ONLY_ALPHA_366"));
    let before_a_view = std::fs::read(&a_view).unwrap();
    let reopen = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "open",
            "beta",
            "--no-run",
            "--no-attach",
            "--view-path",
            b_view.to_str().unwrap(),
        ],
    );
    assert!(reopen.status.success(), "{}", stderr(&reopen));
    assert_eq!(std::fs::read(&a_view).unwrap(), before_a_view);
    let refused = umbrella_env(
        &socket,
        xdg,
        &["space", "add", "beta", "--session", &a.name],
    );
    assert!(!refused.status.success(), "cross-Space add must fail");
    for _ in 0..2 {
        let add = umbrella_env(&socket, xdg, &["space", "add", "alpha"]);
        assert!(add.status.success(), "{}", stderr(&add));
    }
    assert_eq!(
        prismattyc_mux::load_space(&dir, "alpha")
            .unwrap()
            .sessions
            .len(),
        3
    );
    let mut old_writer = TestClient::connect(&socket);
    let old_client = register_client(&mut old_writer);
    old_writer.request(|request_id| ControlRequest::SetClientSpace {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: old_client,
        space_id: alpha.id.clone().unwrap(),
    });
    let moved = umbrella_env(
        &socket,
        xdg,
        &["space", "move", "beta", "--session", &a.name],
    );
    assert!(moved.status.success(), "{}", stderr(&moved));
    let after = ctl.snapshot();
    let moved = after.sessions.iter().find(|s| s.id == a.id).unwrap();
    assert_eq!(moved.space_id, beta.id);
    assert_eq!(moved.windows[0].panes[0].id, ap.id);
    assert_eq!(moved.windows[0].panes[0].child_pid, ap.child_pid);
    let stale = old_writer.try_request(|request_id| ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: old_client,
        pane_id: ap.id,
        data: "STALE_WRITE".into(),
    });
    assert!(
        stale.is_err(),
        "source view must not type into moved session"
    );
    let stale_resize = old_writer.try_request(|request_id| ControlRequest::Resize {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: Some(old_client),
        window_id: a.windows[0].id,
        cols: 12,
        rows: 6,
        cell_width_px: None,
        cell_height_px: None,
        fit: true,
        host: true,
    });
    assert!(
        stale_resize.is_err(),
        "source view must not resize moved session"
    );
    let source = after
        .sessions
        .iter()
        .find(|s| s.space_id == alpha.id)
        .unwrap();
    let split = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: source.windows[0].id,
        target_pane_id: source.windows[0].panes[0].id,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    let _ = split;
    let split_snapshot = ctl.snapshot();
    let split_source = split_snapshot
        .sessions
        .iter()
        .find(|s| s.id == source.id)
        .unwrap();
    let moving = &split_source.windows[0].panes[1];
    let moved = umbrella_env(
        &socket,
        xdg,
        &["space", "move", "beta", "--pane", &moving.id.to_string()],
    );
    assert!(moved.status.success(), "{}", stderr(&moved));
    let after_pane = ctl.snapshot();
    let destination = after_pane
        .sessions
        .iter()
        .find(|s| {
            s.windows
                .iter()
                .any(|w| w.panes.iter().any(|p| p.id == moving.id))
        })
        .unwrap();
    assert_eq!(destination.space_id, beta.id);
    assert_eq!(destination.windows[0].panes[0].child_pid, moving.child_pid);
    assert_eq!(
        after_pane
            .sessions
            .iter()
            .find(|s| s.id == source.id)
            .unwrap()
            .windows[0]
            .panes
            .len(),
        1
    );
    assert!(
        read_pane_lines(&mut ctl, client, moving.id).0,
        "moved process must be alive"
    );
    write_pane_as(&mut ctl, client, moving.id, "exit\r");
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = ctl.snapshot();
        let pane = snapshot
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .find(|p| p.id == moving.id)
            .unwrap();
        if pane.child_pid.is_none() {
            break;
        }
        assert!(
            Instant::now() < until,
            "exited pane must not remain live in chip data"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exclusive_spaces_empty_save_delete_and_legacy_conflict() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let view = data.0.join("view.json");
    let view = view.to_str().unwrap();
    for name in ["alpha", "beta"] {
        let result = umbrella_env(
            &socket,
            xdg,
            &["space", "create", name, "--no-attach", "--view-path", view],
        );
        assert!(result.status.success(), "{}", stderr(&result));
    }
    let dir = data.0.join("prismattyc/spaces");
    let alpha = prismattyc_mux::load_space(&dir, "alpha").unwrap();
    let name = &alpha.sessions[0].name;
    let result = umbrella_env(&socket, xdg, &["space", "move", "beta", "--session", name]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(prismattyc_mux::load_space(&dir, "alpha")
        .unwrap()
        .sessions
        .is_empty());
    let result = umbrella_env(
        &socket,
        xdg,
        &["space", "open", "alpha", "--no-attach", "--view-path", view],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    let result = umbrella_env(
        &socket,
        xdg,
        &["space", "save", "alpha", "--view-path", view],
    );
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(
        prismattyc_mux::load_space(&dir, "alpha")
            .unwrap()
            .sessions
            .is_empty(),
        "empty save must not adopt every daemon session"
    );
    let result = umbrella_env(&socket, xdg, &["space", "rm", "beta"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let mut ctl = TestClient::connect(&socket);
    let snapshot = ctl.snapshot();
    let released = snapshot.sessions.iter().find(|s| &s.name == name).unwrap();
    assert!(released.space_id.is_none());
    assert!(released.windows[0].panes[0].child_pid.is_some());
    let mut legacy = prismattyc_mux::from_sessions(&[released]);
    legacy.id = None;
    legacy.version = 1;
    prismattyc_mux::save_space(&dir, "legacy-one", &legacy).unwrap();
    prismattyc_mux::save_space(&dir, "legacy-two", &legacy).unwrap();
    let result = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "open",
            "legacy-one",
            "--no-attach",
            "--view-path",
            view,
        ],
    );
    assert!(
        !result.status.success(),
        "duplicate legacy session must not be reused as an isolated Space"
    );
    let result = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "create",
            "unrelated",
            "--no-attach",
            "--view-path",
            view,
        ],
    );
    assert!(
        result.status.success(),
        "unrelated fresh create must work: {}",
        stderr(&result)
    );
    let unrelated = prismattyc_mux::load_space(&dir, "unrelated").unwrap();
    assert_ne!(unrelated.sessions[0].name, *name);
}

#[test]
fn exclusive_spaces_rejected_pane_move_does_not_block_later_work() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let view = data.0.join("view.json");
    for name in ["alpha", "beta"] {
        let result = umbrella_env(
            &socket,
            xdg,
            &[
                "space",
                "create",
                name,
                "--no-attach",
                "--view-path",
                view.to_str().unwrap(),
            ],
        );
        assert!(result.status.success(), "{}", stderr(&result));
    }
    let dir = data.0.join("prismattyc/spaces");
    let alpha = prismattyc_mux::load_space(&dir, "alpha").unwrap();
    let beta = prismattyc_mux::load_space(&dir, "beta").unwrap();
    let mut ctl = TestClient::connect(&socket);
    let snapshot = ctl.snapshot();
    let source = snapshot
        .sessions
        .iter()
        .find(|s| s.space_id == alpha.id)
        .unwrap();
    let target = snapshot
        .sessions
        .iter()
        .find(|s| s.space_id == beta.id)
        .unwrap();
    ctl.request(|request_id| ControlRequest::Resize {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: target.windows[0].id,
        cols: 2,
        rows: 2,
        cell_width_px: None,
        cell_height_px: None,
        client_id: None,
        fit: true,
        host: false,
    });
    let result = umbrella_env(
        &socket,
        xdg,
        &[
            "space",
            "move",
            "beta",
            "--pane",
            &source.windows[0].panes[0].id.to_string(),
            "--to-session",
            &target.name,
        ],
    );
    assert!(
        !result.status.success(),
        "a two-column destination cannot hold a split"
    );
    assert!(
        !dir.join(".ownership-transaction").exists(),
        "rejected move must not strand a journal"
    );
    let after = ctl.snapshot();
    assert_eq!(
        after
            .sessions
            .iter()
            .find(|s| s.id == source.id)
            .unwrap()
            .space_id,
        alpha.id
    );
    let result = umbrella_env(&socket, xdg, &["space", "add", "alpha"]);
    assert!(
        result.status.success(),
        "later work must remain possible: {}",
        stderr(&result)
    );
}

#[test]
fn exclusive_spaces_rename_preserves_owner_and_rejects_collision() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    for name in ["alpha", "beta"] {
        let created = umbrella_env(&socket, xdg, &["space", "create", name, "--no-attach"]);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let dir = data.0.join("prismattyc/spaces");
    let before = std::fs::read(dir.join("alpha.json")).unwrap();
    let mut ctl = TestClient::connect(&socket);
    let snapshot = ctl.snapshot();
    let renamed = umbrella_env(&socket, xdg, &["space", "rename", "alpha", "gamma"]);
    assert!(renamed.status.success(), "{}", stderr(&renamed));
    assert!(!dir.join("alpha.json").exists());
    assert_eq!(std::fs::read(dir.join("gamma.json")).unwrap(), before);
    let after = ctl.snapshot();
    for session in snapshot.sessions {
        let live = after.sessions.iter().find(|s| s.id == session.id).unwrap();
        assert_eq!(live.space_id, session.space_id);
        assert_eq!(
            live.windows[0].panes[0].child_pid,
            session.windows[0].panes[0].child_pid
        );
    }
    let collision = umbrella_env(&socket, xdg, &["space", "rename", "gamma", "beta"]);
    assert!(!collision.status.success());
    assert_eq!(std::fs::read(dir.join("gamma.json")).unwrap(), before);
    assert!(!dir.join(".ownership-transaction").exists());
    let added = umbrella_env(&socket, xdg, &["space", "add", "gamma"]);
    assert!(added.status.success(), "{}", stderr(&added));
}

#[test]
fn space_undo_preserves_processes_and_refuses_newer_changes() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let data = layout_data_dir();
    let xdg = &[("XDG_DATA_HOME", data.0.as_path())];
    let run = |args: &[&str]| {
        let output = umbrella_env(&socket, xdg, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    };
    run(&["new", "--no-attach", "undo-a", "--", "/bin/sh"]);
    run(&["new", "--no-attach", "undo-b", "--", "/bin/sh"]);
    run(&["space", "save", "source", "undo-a"]);
    run(&["space", "save", "target", "undo-b"]);
    let receipt = data.0.join("undo.json");
    let receipt = receipt.to_str().unwrap();
    let mut ctl = TestClient::connect(&socket);
    let before = ctl
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.name == "undo-a")
        .unwrap();
    for args in [
        vec![
            "space",
            "remove",
            "source",
            "--session",
            "undo-a",
            "--undo-file",
            receipt,
        ],
        vec![
            "space",
            "move",
            "target",
            "--session",
            "undo-a",
            "--undo-file",
            receipt,
        ],
    ] {
        run(&args);
        run(&["space", "undo", receipt]);
        let after = ctl
            .snapshot()
            .sessions
            .into_iter()
            .find(|s| s.name == "undo-a")
            .unwrap();
        assert_eq!(before.id, after.id);
        assert_eq!(before.space_id, after.space_id);
        assert_eq!(
            before.windows[0].panes[0].child_pid,
            after.windows[0].panes[0].child_pid
        );
    }
    ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: before.windows[0].id,
        target_pane_id: before.windows[0].panes[0].id,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    run(&["space", "save", "source", "undo-a"]);
    let split = ctl
        .snapshot()
        .sessions
        .into_iter()
        .find(|s| s.name == "undo-a")
        .unwrap();
    let moving = split.windows[0].panes[1].id.to_string();
    run(&[
        "space",
        "move",
        "target",
        "--pane",
        &moving,
        "--undo-file",
        receipt,
    ]);
    run(&["space", "undo", receipt]);
    let after = ctl.snapshot();
    let restored = after.sessions.iter().find(|s| s.name == "undo-a").unwrap();
    assert_eq!(restored.windows[0].panes.len(), 2);
    for pane in &split.windows[0].panes {
        assert!(restored.windows[0]
            .panes
            .iter()
            .any(|p| p.id == pane.id && p.child_pid == pane.child_pid));
    }
    assert!(after
        .sessions
        .iter()
        .all(|s| !s.windows.is_empty() || s.space_id.is_none()));
    run(&[
        "space",
        "remove",
        "source",
        "--session",
        "undo-a",
        "--undo-file",
        receipt,
    ]);
    run(&["space", "add", "source", "--name", "newer"]);
    let output = umbrella_env(&socket, xdg, &["space", "undo", receipt]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("newer changes"),
        "{}",
        stderr(&output)
    );
    assert!(!data
        .0
        .join("prismattyc/spaces/.ownership-transaction")
        .exists());
    drop(_guard);
    let _restarted = start_server(&socket);
    let output = umbrella_env(&socket, xdg, &["space", "undo", receipt]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("daemon restarted"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn pane_write_cli_targets_one_pane_even_with_sync_enabled() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    let created = umbrella(
        &socket,
        &["new", "--no-attach", "intentional", "--", "/bin/sh"],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let mut ctl = TestClient::connect(&socket);
    let client = register_client(&mut ctl);
    let window = session_window(&ctl.snapshot(), "intentional").clone();
    let pane = window.panes[0].id;
    ctl.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: client,
        pane_id: pane,
    });
    ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id: window.id,
        target_pane_id: pane,
        axis: AxisWire::Horizontal,
        ratio: 0.5,
        spawn: SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![],
            cwd: None,
            env: BTreeMap::new(),
        },
        client_id: Some(client),
    });
    ctl.request(|request_id| ControlRequest::ReleaseLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: client,
        pane_id: pane,
    });
    let sibling = session_window(&ctl.snapshot(), "intentional")
        .panes
        .iter()
        .find(|p| p.id != pane)
        .unwrap()
        .id;
    let synced = umbrella(&socket, &["sync", "on", "intentional"]);
    assert!(synced.status.success(), "{}", stderr(&synced));
    let text = "printf '__TARGET_%s__\\n' 'ONE'";
    let sent = umbrella(
        &socket,
        &[
            "pane-write",
            &pane.to_string(),
            "--text",
            text,
            "--submit",
            "enter",
            "--json",
        ],
    );
    assert!(sent.status.success(), "{}", stderr(&sent));
    let receipt: serde_json::Value = serde_json::from_str(&stdout(&sent)).unwrap();
    assert_eq!(receipt["status"], "queued");
    assert_eq!(receipt["response"]["pane_id"], pane);
    assert_eq!(receipt["response"]["nbytes"], text.len() + 1);
    assert_eq!(receipt["response"]["complete"], true);
    let mut found = false;
    for _ in 0..50 {
        let (_, text) = read_pane_lines(&mut ctl, client, pane);
        if text.contains("__TARGET_ONE__") {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(found, "target command did not execute");
    let (_, sibling_text) = read_pane_lines(&mut ctl, client, sibling);
    assert!(
        !sibling_text.contains("TARGET"),
        "intentional input leaked to sync sibling: {sibling_text}"
    );
    assert!(session_window(&ctl.snapshot(), "intentional")
        .panes
        .iter()
        .all(|p| p.controller_id.is_none()));
}

#[test]
fn pane_write_cli_refuses_unknown_agent_then_leaves_literal_text_unsubmitted() {
    let socket = socket_path();
    let _guard = start_server(&socket);
    assert!(umbrella(
        &socket,
        &["new", "--no-attach", "pane-write-text", "--", "/bin/sh"]
    )
    .status
    .success());
    let mut ctl = TestClient::connect(&socket);
    let client = register_client(&mut ctl);
    let pane = session_window(&ctl.snapshot(), "pane-write-text").panes[0].id;
    let auto = umbrella(
        &socket,
        &[
            "pane-write",
            &pane.to_string(),
            "--text",
            "must-not-run",
            "--json",
        ],
    );
    assert!(!auto.status.success());
    let error: serde_json::Value = serde_json::from_str(&stdout(&auto)).unwrap();
    assert_eq!(error["code"], "invalid_request");
    let literal = umbrella(
        &socket,
        &[
            "pane-write",
            &pane.to_string(),
            "--text",
            "LITERAL\\nλ",
            "--submit",
            "none",
            "--json",
        ],
    );
    assert!(literal.status.success(), "{}", stderr(&literal));
    let receipt: serde_json::Value = serde_json::from_str(&stdout(&literal)).unwrap();
    assert_eq!(receipt["response"]["nbytes"], "LITERAL\\nλ".len());
    assert!(
        session_window(&ctl.snapshot(), "pane-write-text").panes[0]
            .ledger
            .dirty_input
    );
    let refused = umbrella(
        &socket,
        &[
            "pane-write",
            &pane.to_string(),
            "--text",
            "other",
            "--submit",
            "enter",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    let error: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(error["code"], "input_dirty");
    let (_, text) = read_pane_lines(&mut ctl, client, pane);
    assert!(
        !text.contains("must-not-run") && !text.contains("other"),
        "rejected input reached PTY: {text}"
    );
}

#[test]
fn pane_write_wire_rejects_recycled_child_and_foreign_connection() {
    use prismattyc_mux::PaneWriteSubmit;
    let socket = socket_path();
    let _guard = start_server(&socket);
    assert!(umbrella(
        &socket,
        &["new", "--no-attach", "pane-write-wire", "--", "/bin/sh"]
    )
    .status
    .success());
    let mut ctl = TestClient::connect(&socket);
    let client = register_client(&mut ctl);
    let pane = session_window(&ctl.snapshot(), "pane-write-wire").panes[0].clone();
    let request = |request_id, expected_child_pid| ControlRequest::PaneWrite {
        version: PROTOCOL_VERSION,
        request_id,
        client_id: client,
        pane_id: pane.id,
        expected_child_pid,
        data: "must-not-arrive".into(),
        submit: PaneWriteSubmit::Enter,
    };
    let stale = ctl
        .try_request(|id| request(id, pane.child_pid.unwrap() + 1))
        .unwrap_err();
    assert_eq!(stale.code, ControlErrorCode::StaleId);
    let mut stranger = TestClient::connect(&socket);
    register_client(&mut stranger);
    let foreign = stranger
        .try_request(|id| request(id, pane.child_pid.unwrap()))
        .unwrap_err();
    assert_eq!(foreign.code, ControlErrorCode::StaleId);
    assert!(
        !session_window(&ctl.snapshot(), "pane-write-wire").panes[0]
            .ledger
            .dirty_input
    );
}
