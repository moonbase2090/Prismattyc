//! (story 2.1): `pmux mail <verb>` against a real `pmuxd` — the
//! Mailbox CLI UX on the Mail* protocol. Drives the actual
//! `pmux` binary so parsing, identity, wire format, and output rendering
//! are all exercised the way agents and scripts will call it.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use prismattyc_core::test_time_budget;
use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

fn socket_path() -> PathBuf {
    // A per-process atomic counter guarantees a distinct socket per test.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "pmux-mailcli-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn data_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-mailcli-data-{}-{}",
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

/// Run `pmux --socket <socket> <args>` with a scrubbed identity
/// environment: no inherited PMUX_AGENT / PRISMATTYC_PANE_ID may leak into
/// the resolution order under test.
fn pmux(socket: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    command
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env_remove("PMUX_AGENT")
        .env_remove("PRISMATTYC_PANE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("run pmux")
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "pmux failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn mail_verbs_roundtrip_through_the_cli() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);

    // An agent-bound session to receive mail.
    let out = ok(&pmux(
        &socket,
        &[
            "new",
            "--no-attach",
            "--agent",
            "kiro-pm",
            "pm",
            "--",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    assert!(out.contains("created session \"pm\""), "{out}");

    // No identity anywhere: a usage error, not a guess.
    let output = pmux(&socket, &["mail", "inbox"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no identity"),
        "{output:?}"
    );

    // send → inbox → claim --ids → commit → inbox drained.
    let out = ok(&pmux(
        &socket,
        &[
            "mail",
            "--as",
            "kiro-sb",
            "send",
            "kiro-pm",
            "--summary",
            "cli dogfood",
            "--body",
            "over the wire",
        ],
    ));
    assert!(out.contains("sent: msg:"), "{out}");
    assert!(out.contains("(depth 1)"), "{out}");

    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "inbox"]));
    assert!(out.contains("open: 1 held: 0"), "{out}");

    let out = ok(&pmux(
        &socket,
        &["mail", "--as", "kiro-pm", "claim", "--ids"],
    ));
    let id = out.trim().to_string();
    assert!(id.starts_with("msg:"), "{out}");

    // claim --json renders one JSON line with the letter fields.
    let output = pmux(&socket, &["mail", "--as", "kiro-pm", "claim", "--json"]);
    let out = ok(&output);
    assert!(out.trim_start().starts_with('{'), "{out}");

    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "commit", &id]));
    assert!(out.contains("committed: 1"), "{out}");

    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "inbox"]));
    assert!(out.contains("open: 0 held: 0"), "{out}");

    // alias + who + send-via-alias.
    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "alias", "pm"]));
    assert!(out.contains("aliased: pm -> kiro-pm"), "{out}");

    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-sb", "who"]));
    assert!(out.contains("kiro-pm"), "{out}");
    assert!(out.contains("aliases: pm"), "{out}");

    let out = ok(&pmux(
        &socket,
        &[
            "mail",
            "--as",
            "kiro-sb",
            "send",
            "pm",
            "--summary",
            "via alias",
        ],
    ));
    assert!(out.contains("sent: msg:"), "{out}");

    // release returns a held letter to open.
    let out = ok(&pmux(
        &socket,
        &["mail", "--as", "kiro-pm", "claim", "--ids"],
    ));
    let id = out.trim().to_string();
    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "release", &id]));
    assert!(out.contains("released: 1"), "{out}");
    let out = ok(&pmux(&socket, &["mail", "--as", "kiro-pm", "inbox"]));
    assert!(out.contains("open: 1 held: 0"), "{out}");

    // status needs no identity and lists bound agents.
    let out = ok(&pmux(&socket, &["mail", "status"]));
    assert!(out.contains("daemon:  reachable"), "{out}");
    assert!(out.contains("kiro-pm"), "{out}");

    // broadcast reaches every bound agent but the sender.
    let out = ok(&pmux(
        &socket,
        &[
            "mail",
            "--as",
            "kiro-sb",
            "broadcast",
            "--summary",
            "all hands",
        ],
    ));
    assert!(out.contains("broadcasted: 1 (kiro-pm)"), "{out}");
}

#[test]
fn status_set_requires_pane_and_roundtrips() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);

    let missing = pmux(&socket, &["status-set", "hello"]);
    assert!(!missing.status.success());
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(err.contains("not inside a pmux pane"), "{err}");

    let created = ok(&pmux(
        &socket,
        &[
            "new",
            "--no-attach",
            "work",
            "--",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    let pane = created
        .split("pane ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("pane id in create output");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let set = command
        .arg("--socket")
        .arg(&socket)
        .args(["status-set", "build ok"])
        .env_remove("PMUX_AGENT")
        .env("PRISMATTYC_PANE_ID", pane)
        .stdin(Stdio::null())
        .output()
        .expect("status-set");
    assert!(
        set.status.success(),
        "{}{}",
        String::from_utf8_lossy(&set.stdout),
        String::from_utf8_lossy(&set.stderr)
    );
    let stream = UnixStream::connect(&socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut rid = 1u64;
    let mut req = |make: fn(u64) -> ControlRequest| {
        let request = make(rid);
        rid += 1;
        serde_json::to_writer(&mut writer, &request).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: ControlResponse = serde_json::from_str(&line).unwrap();
        match response.body {
            ControlResponseBody::Ok { response } => response,
            ControlResponseBody::Error { error } => panic!("{error:?}"),
        }
    };
    let _ = req(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let ControlResponseData::Snapshot { snapshot } = req(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }) else {
        panic!("expected Snapshot");
    };
    let pane_id: u64 = pane.parse().unwrap();
    let status = snapshot
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .find(|p| p.id == pane_id)
        .and_then(|p| p.status.as_deref());
    assert_eq!(status, Some("build ok"));
}

#[test]
fn mail_watch_blocks_server_side_and_wakes_on_send() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);

    let out = ok(&pmux(
        &socket,
        &[
            "new",
            "--no-attach",
            "--agent",
            "kiro-pm",
            "pm",
            "--",
            "/bin/sh",
            "-c",
            "exec sleep 999",
        ],
    ));
    assert!(out.contains("created session"), "{out}");

    // Timeout path: no mail, exit 1.
    let output = pmux(
        &socket,
        &["mail", "--as", "kiro-pm", "watch", "--timeout", "1"],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("open: 0"),
        "{output:?}"
    );

    // Wake path: a send 200ms in must wake the parked wait (exit 0) well
    // before the 5s timeout — the condvar, not the clock, ends the wait.
    let wake_socket = socket.clone();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        pmux(
            &wake_socket,
            &[
                "mail",
                "--as",
                "kiro-sb",
                "send",
                "kiro-pm",
                "--summary",
                "wake",
            ],
        )
    });
    let started = std::time::Instant::now();
    let output = pmux(
        &socket,
        &["mail", "--as", "kiro-pm", "watch", "--timeout", "5"],
    );
    let elapsed = started.elapsed();
    sender.join().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("open: 1"),
        "{output:?}"
    );
    assert!(
        elapsed < test_time_budget(Duration::from_secs(4)),
        "watch must wake on the ring, not run to timeout ({elapsed:?})"
    );
}
