//! Exercises the pmuxd-owned webhook worker against a local fake receiver.

#![cfg(unix)]

use prismattyc_core::activity::{ActivityInput, ActivityKind, ActivitySource};
use prismattyc_core::AgentId;
use prismattyc_mux::{
    activity, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData,
    PROTOCOL_VERSION,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;
use support::clear_command_env;

struct Daemon {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(config: String) -> Daemon {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-webhook-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let socket = dir.join("s.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    let child = command
        .arg("--socket")
        .arg(&socket)
        .args(["--", "/bin/sh", "-c", "exec sleep 999"])
        .env("PRISMATTYC_CONFIG", &config_path)
        .env("XDG_DATA_HOME", &dir)
        .env("PMUX_SESSION_AGENTS", dir.join("session-agents.json"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pmuxd");
    let daemon = Daemon { child, dir, socket };
    for _ in 0..250 {
        if UnixStream::connect(&daemon.socket).is_ok() {
            return daemon;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pmuxd did not listen");
}

fn receive_request(listener: TcpListener) -> Vec<u8> {
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let started = Instant::now();
    let (mut stream, _) = loop {
        if let Ok(connection) = listener.accept() {
            break connection;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "webhook was not delivered"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    stream
        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    bytes
}

fn control_request(socket: &std::path::Path, request: ControlRequest) -> ControlResponse {
    let mut stream = UnixStream::connect(socket).unwrap();
    let mut frame = serde_json::to_vec(&request).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn configured_daemon_posts_activity_with_verifiable_signature() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = std::thread::spawn(move || receive_request(listener));
    let dir = std::env::temp_dir().join(format!("pmux-webhook-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret_path = dir.join("key");
    let secret = b"01234567890123456789012345678901";
    std::fs::write(&secret_path, secret).unwrap();
    let daemon = start(format!(
        "[mux]\nactivity_webhook_url = \"http://{address}/notify\"\nactivity_webhook_secret_file = {:?}\n",
        secret_path.display().to_string()
    ));
    std::thread::sleep(Duration::from_millis(600));
    let event = ActivityInput {
        schema_version: 1,
        seat: AgentId::new("worker-a").unwrap(),
        run_id: "run-1".into(),
        event_id: "event-1".into(),
        source: ActivitySource::Notify,
        kind: ActivityKind::Waiting,
    };
    let response = activity::exchange(
        &daemon.socket,
        ControlRequest::PublishActivity {
            version: PROTOCOL_VERSION,
            request_id: 7,
            event,
        },
    )
    .unwrap();
    assert!(matches!(
        response,
        ControlResponseData::ActivityPublished { .. }
    ));
    let request = String::from_utf8_lossy(&receiver.join().unwrap()).to_string();
    assert!(request.contains("X-Prismattyc-Timestamp:"));
    assert!(request.contains("\"seat\":\"worker-a\""));
    assert!(request.contains("\"state\":\"waiting\""));
    assert!(request.contains("\"pane_id\":null"));
    assert!(request.contains("\"space\":null"));
    assert!(!request.contains("01234567890123456789012345678901"));
    drop(daemon);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn agent_sessions_are_added_to_activity_without_harness_hooks() {
    let daemon = start("[mux]\nexperimental_activity = true\n".into());
    std::thread::sleep(Duration::from_millis(400));
    let snapshot = control_request(
        &daemon.socket,
        ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 10,
        },
    );
    let ControlResponseBody::Ok {
        response: ControlResponseData::Snapshot { snapshot },
    } = snapshot.body
    else {
        panic!("snapshot request failed");
    };
    let session_id = snapshot.sessions[0].id;
    let renamed = control_request(
        &daemon.socket,
        ControlRequest::NameSession {
            version: PROTOCOL_VERSION,
            request_id: 11,
            session_id,
            name: "worker-a".into(),
            agent_id: Some("worker-a".into()),
        },
    );
    assert!(matches!(renamed.body, ControlResponseBody::Ok { .. }));

    let after_rename = control_request(
        &daemon.socket,
        ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 13,
        },
    );
    let ControlResponseBody::Ok {
        response: ControlResponseData::Snapshot { snapshot },
    } = after_rename.body
    else {
        panic!("post-rename snapshot request failed");
    };
    assert_eq!(snapshot.sessions[0].agent_id.as_deref(), Some("worker-a"));

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut last_response = String::new();
    loop {
        let response = control_request(
            &daemon.socket,
            ControlRequest::ReadActivity {
                version: PROTOCOL_VERSION,
                request_id: 12,
                cursor: None,
                limit: 256,
            },
        );
        if let ControlResponseBody::Ok {
            response: ControlResponseData::Activity { page },
        } = response.body
        {
            last_response = format!(
                "{} records, current {}",
                page.records.len(),
                page.current_sequence
            );
            if page.records.iter().any(|record| {
                record.event.seat.as_str() == "worker-a"
                    && record.event.source == ActivitySource::Runtime
                    && record.event.kind == ActivityKind::Waiting
            }) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "agent session was not projected; last read: {last_response}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
