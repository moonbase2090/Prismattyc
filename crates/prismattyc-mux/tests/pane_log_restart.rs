//! PT-115: pane-log snapshot + tail survive a real `pmuxd` restart.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

const FIXTURE: &[u8] = include_bytes!("../../prismattyc-emulator/tests/fixtures/pt72-session.bin");

#[test]
fn small_output_appends_checkpoint_and_survives_unclean_restart() {
    let data = DataGuard(data_dir());
    let socket = socket_path();
    let server = start_server(
        &socket,
        &data.0,
        "sh",
        &["-c", "printf 'BASE READY\n'; exec cat"],
    );
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let deadline = Instant::now() + Duration::from_secs(8);
    while !read_lines(&mut client, pane)
        .join("\n")
        .contains("BASE READY")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Let the checkpoint include the initial output before measuring one edit.
    std::thread::sleep(Duration::from_secs(3));
    let path = persist_path(&data.0);
    let before = std::fs::read(&path).unwrap();
    let before_time = std::fs::metadata(&path).unwrap().modified().unwrap();
    ok_data(client.call(&ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
        pane_id: pane,
        data: "RECOVER THIS DELTA\n".into(),
    }));
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let changed = std::fs::metadata(&path).unwrap().modified().unwrap() != before_time;
        if changed {
            break;
        }
        assert!(Instant::now() < deadline, "delta checkpoint never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.starts_with(&before),
        "ordinary output rewrote the full recovery snapshot"
    );
    assert!(
        after.len() - before.len() < 32 * 1024,
        "small output rewrote saved history"
    );
    drop(client);
    drop(server); // SIGKILL: no shutdown snapshot can hide a broken journal.
    let socket = socket_path();
    let _server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let restored = read_lines(&mut client, pane).join("\n");
    assert!(
        restored.contains("BASE READY"),
        "lost the base snapshot: {restored}"
    );
    assert!(
        restored.contains("RECOVER THIS DELTA"),
        "lost committed delta: {restored}"
    );
}

#[test]
fn continuous_output_keeps_checkpoints_rate_limited() {
    let data = DataGuard(data_dir());
    let socket = socket_path();
    let _server = start_server(
        &socket,
        &data.0,
        "/bin/sh",
        &["-c", "while :; do printf .; sleep 0.02; done"],
    );
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut writes = Vec::new();
    while writes.len() < 3 {
        if let Ok(modified) = std::fs::metadata(persist_path(&data.0)).and_then(|m| m.modified()) {
            if writes.last() != Some(&modified) {
                if let Some(previous) = writes.last() {
                    // Allow scheduling and filesystem timestamp variation while
                    // rejecting a checkpoint on every output/maintenance tick.
                    assert!(
                        modified.duration_since(*previous).unwrap() >= Duration::from_secs(1),
                        "continuous output bypassed the two-second checkpoint cadence"
                    );
                }
                writes.push(modified);
            }
        }
        assert!(Instant::now() < deadline, "periodic checkpoints stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn blocked_checkpoint_writer_does_not_block_control_requests() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let data = DataGuard(data_dir());
    let socket = socket_path();
    let fifo = persist_path(&data.0).with_extension("json.tmp");
    std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    // A blank terminal snapshot must not fit into the pipe. Otherwise the
    // writer can finish and the test would exercise an ordinary idle daemon.
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) },
        4096
    );
    let _server = start_server(
        &socket,
        &data.0,
        "sh",
        &["-c", "printf 'checkpoint ready'; exec cat"],
    );
    // The snapshot exceeds the FIFO buffer. Reading one byte proves that the
    // writer started; leaving the rest unread keeps that write blocked.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut byte = [0];
        if reader.read(&mut byte).is_ok_and(|n| n == 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "checkpoint writer did not reach FIFO"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    serde_json::to_writer(
        &mut stream,
        &ControlRequest::Ping {
            version: PROTOCOL_VERSION,
            request_id: 1,
        },
    )
    .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("checkpoint I/O blocked the control lock");
    assert!(matches!(
        serde_json::from_str::<ControlResponse>(&line).unwrap().body,
        ControlResponseBody::Ok { .. }
    ));
    let mut client = Client::connect(&socket);
    client
        .stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let pane_id = first_pane(&mut client);
    ok_data(client.call(&ControlRequest::WritePane {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
        pane_id,
        data: "typing while checkpoint blocked\n".into(),
    }));
    let deadline = Instant::now() + Duration::from_secs(1);
    while !read_lines(&mut client, pane_id)
        .join("\n")
        .contains("typing while checkpoint blocked")
    {
        assert!(
            Instant::now() < deadline,
            "PTY echo stalled behind checkpoint"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn socket_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "pmux-panelog-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn data_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-panelog-data-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

struct DataGuard(PathBuf);

impl Drop for DataGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start_server(socket: &Path, data: &Path, program: &str, argv: &[&str]) -> ServerGuard {
    let mut command = Command::new(
        std::env::var_os("PMUX_CHECKPOINT_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_pmuxd").into()),
    );
    clear_command_env(&mut command);
    let guard = ServerGuard {
        child: command
            .arg("--socket")
            .arg(socket)
            .arg("--")
            .arg(program)
            .args(argv)
            .env("XDG_DATA_HOME", data)
            .env("PMUX_PANE_LOG", persist_path(data))
            .env("PMUX_SESSION_AGENTS", data.join("session-agents.json"))
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

fn shutdown_clean(client: &mut Client, mut server: ServerGuard) {
    let response = client.call(&ControlRequest::ShutdownServer {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
    });
    assert!(
        matches!(
            response.body,
            ControlResponseBody::Ok {
                response: ControlResponseData::ShutdownAccepted
            }
        ),
        "expected ShutdownAccepted: {response:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match server.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "pmuxd did not exit after ShutdownServer"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("wait pmuxd: {error}"),
        }
    }
    let _ = std::fs::remove_file(&server.socket);
    std::mem::forget(server);
}

struct Client {
    stream: UnixStream,
    next_id: u64,
    client_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        let mut client = Self {
            stream,
            next_id: 0,
            client_id: 0,
        };
        let response = client.call(&ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: 0,
        });
        let ControlResponseBody::Ok {
            response: ControlResponseData::ClientRegistered { client_id },
        } = response.body
        else {
            panic!("expected ClientRegistered: {response:?}");
        };
        client.client_id = client_id;
        client
    }

    fn call(&mut self, request: &ControlRequest) -> ControlResponse {
        self.next_id += 1;
        let mut value = serde_json::to_value(request).unwrap();
        value["request_id"] = serde_json::json!(self.next_id);
        let request: ControlRequest = serde_json::from_value(value).unwrap();
        serde_json::to_writer(&mut self.stream, &request).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.stream.flush().unwrap();
        let mut reader = BufReader::new(self.stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

fn ok_data(response: ControlResponse) -> ControlResponseData {
    match response.body {
        ControlResponseBody::Ok { response } => response,
        other => panic!("expected Ok: {other:?}"),
    }
}

fn first_pane(client: &mut Client) -> u64 {
    let data = ok_data(client.call(&ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id: 0,
    }));
    let ControlResponseData::Snapshot { snapshot } = data else {
        panic!("expected Snapshot: {data:?}");
    };
    snapshot.sessions[0].windows[0].panes[0].id
}

fn read_lines(client: &mut Client, pane_id: u64) -> Vec<String> {
    let data = ok_data(client.call(&ControlRequest::ReadPane {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
        pane_id,
    }));
    let ControlResponseData::PaneContent { content } = data else {
        panic!("expected PaneContent: {data:?}");
    };
    content.lines
}

fn has_content(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.chars().any(|c| !c.is_whitespace()))
}

fn wait_content(client: &mut Client, pane_id: u64) -> Vec<String> {
    let mut last = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let lines = read_lines(client, pane_id);
        if has_content(&lines) && last.as_ref() == Some(&lines) {
            return lines;
        }
        last = Some(lines);
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("pane never produced a stable screen");
}

fn persist_path(data: &Path) -> PathBuf {
    data.join("prismattyc").join("pane-log-default.json")
}

fn write_mismatch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, r#"{"format":99,"codec":"replay-v0","panes":[]}"#).unwrap();
}

#[test]
fn pt72_fixture_screen_survives_pmuxd_restart() {
    let data = DataGuard(data_dir());
    let fixture = data.0.join("pt72-session.bin");
    std::fs::write(&fixture, FIXTURE).unwrap();
    let socket = socket_path();
    let script = format!("cat '{}'; exec sleep 999", fixture.display());
    let server = start_server(&socket, &data.0, "/bin/sh", &["-c", &script]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let before = wait_content(&mut client, pane);
    shutdown_clean(&mut client, server);

    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let after = wait_content(&mut client, pane);
    assert_eq!(
        after, before,
        "restored screen must match the pre-restart grid"
    );
    let _ = server;
}

#[test]
fn corrupt_persist_starts_empty_with_reset_line() {
    let data = DataGuard(data_dir());
    let path = persist_path(&data.0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{not json").unwrap();
    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let lines = wait_content(&mut client, pane);
    let joined = lines.join("\n");
    assert!(
        joined.contains("pane log reset"),
        "corrupt persist must show a reset line, got {joined:?}"
    );
    let _ = server;
}

#[test]
fn foreign_instance_persist_is_not_restored() {
    let data = DataGuard(data_dir());
    write_mismatch(&data.0.join("prismattyc").join("pane-log.json"));
    write_mismatch(&data.0.join("prismattyc").join("pane-log-other.json"));
    let socket = socket_path();
    let server = start_server(
        &socket,
        &data.0,
        "/bin/sh",
        &["-c", "printf 'READY\\n'; exec sleep 999"],
    );
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let lines = wait_content(&mut client, pane);
    let joined = lines.join("\n");
    assert!(
        joined.contains("READY") && !joined.contains("pane log reset"),
        "foreign-instance persist must not restore or reset, got {joined:?}"
    );
    let _ = server;
}

#[test]
fn other_session_persist_is_not_restored() {
    let data = DataGuard(data_dir());
    let path = persist_path(&data.0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"format":1,"codec":"emulator-state-v1","panes":[{"session":"other","pane_index":0,"snapshot_seq":0,"cols":80,"rows":24,"cell_px":[8,16],"tail":[{"seq":1,"event":{"kind":"output","bytes":[70,79,82,69,73,71,78]}}]}]}"#,
    )
    .unwrap();
    let socket = socket_path();
    let server = start_server(
        &socket,
        &data.0,
        "/bin/sh",
        &["-c", "printf 'READY\\n'; exec sleep 999"],
    );
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let lines = wait_content(&mut client, pane);
    let joined = lines.join("\n");
    assert!(
        joined.contains("READY")
            && !joined.contains("FOREIGN")
            && !joined.contains("pane log reset"),
        "persist for a different session name must not restore, got {joined:?}"
    );
    let _ = server;
}

#[test]
fn bad_snapshot_replays_tail_on_pmuxd_restart() {
    let data = DataGuard(data_dir());
    let fixture = data.0.join("pt72-session.bin");
    std::fs::write(&fixture, FIXTURE).unwrap();
    let socket = socket_path();
    let script = format!("cat '{}'; exec sleep 999", fixture.display());
    let server = start_server(&socket, &data.0, "/bin/sh", &["-c", &script]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let before = wait_content(&mut client, pane);
    shutdown_clean(&mut client, server);

    let path = persist_path(&data.0);
    let raw = std::fs::read_to_string(&path).expect("persist after shutdown");
    let mut file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(file["codec"].as_str(), Some("emulator-state-v1"));
    assert!(
        file["panes"][0]["tail"]
            .as_array()
            .is_some_and(|tail| !tail.is_empty()),
        "tail must remain so restore can replay it"
    );
    file["panes"][0]["snapshot"] = serde_json::json!(b"not-emulator-state".as_slice());
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let after = wait_content(&mut client, pane);
    assert_eq!(
        after, before,
        "a bad snapshot must replay the tail, not leave a blank pane"
    );
    let _ = server;
}

#[test]
fn snapshot_without_tail_survives_pmuxd_restart() {
    let data = DataGuard(data_dir());
    let fixture = data.0.join("pt72-session.bin");
    std::fs::write(&fixture, FIXTURE).unwrap();
    let socket = socket_path();
    let script = format!("cat '{}'; exec sleep 999", fixture.display());
    let server = start_server(&socket, &data.0, "/bin/sh", &["-c", &script]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let before = wait_content(&mut client, pane);
    shutdown_clean(&mut client, server);

    let path = persist_path(&data.0);
    let raw = std::fs::read_to_string(&path).expect("persist after shutdown");
    let mut file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        file["codec"].as_str(),
        Some("emulator-state-v1"),
        "persist codec must be emulator-state-v1, got {raw}"
    );
    let snapshot = &file["panes"][0]["snapshot"];
    let has_snapshot = snapshot.as_array().is_some_and(|bytes| !bytes.is_empty())
        || snapshot.as_str().is_some_and(|text| !text.is_empty());
    assert!(
        has_snapshot,
        "snapshot must be present so restore is not tail-only: {snapshot}"
    );
    file["panes"][0]["tail"] = serde_json::json!([]);
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let after = wait_content(&mut client, pane);
    assert_eq!(
        after, before,
        "snapshot must restore the pre-restart screen after the tail is dropped"
    );
    let _ = server;
}

#[test]
fn replay_v0_persist_starts_empty_with_reset_line() {
    let data = DataGuard(data_dir());
    let path = persist_path(&data.0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"format":1,"codec":"replay-v0","panes":[]}"#).unwrap();
    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let lines = wait_content(&mut client, pane);
    let joined = lines.join("\n");
    assert!(
        joined.contains("pane log reset"),
        "replay-v0 persist must reset, got {joined:?}"
    );
    let _ = server;
}

#[test]
fn version_mismatch_starts_empty_with_reset_line() {
    let data = DataGuard(data_dir());
    let path = persist_path(&data.0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"format":99,"codec":"replay-v0","panes":[]}"#).unwrap();
    let socket = socket_path();
    let server = start_server(&socket, &data.0, "/bin/sleep", &["999"]);
    let mut client = Client::connect(&socket);
    let pane = first_pane(&mut client);
    let lines = wait_content(&mut client, pane);
    let joined = lines.join("\n");
    assert!(
        joined.contains("pane log reset"),
        "version mismatch must show a reset line, got {joined:?}"
    );
    let _ = server;
}
