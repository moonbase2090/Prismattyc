//! (story 1b.1): end-to-end mail dogfood over the mux socket.
//!
//! Two agent-bound sessions on a real `pmuxd`: send → sticky attention
//! armed + `PMUX_MAIL` injected into the recipient's live pane →
//! wait wakes → claim returns the letter → commit drains the mailbox →
//! attention clears. No external watcher, no doorbell script, no shell
//! loop — the whole loop is in-process.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, PaneEvent,
    PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

fn socket_path() -> PathBuf {
    // A per-process atomic counter guarantees a distinct socket per test.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "pmux-maile2e-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn data_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-maile2e-data-{}-{}",
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
            // The pane child is a plain shell; the debug-only seam makes the
            // doorbell see an agent in the terminal foreground (PT-94).
            .env(
                prismattyc_mux::procinfo::TEST_FOREGROUND_COMMAND_ENV,
                "claude",
            )
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

/// One control connection with request-id bookkeeping.
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
        let request = with_request_id(request, self.next_id);
        serde_json::to_writer(&mut self.stream, &request).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.stream.flush().unwrap();
        let mut reader = BufReader::new(self.stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn hello(&mut self, agent: &str) {
        let response = self.call(&ControlRequest::MailHello {
            version: PROTOCOL_VERSION,
            request_id: 0,
            client_id: self.client_id,
            agent: agent.into(),
        });
        assert!(
            matches!(
                response.body,
                ControlResponseBody::Ok {
                    response: ControlResponseData::MailSeated { .. }
                }
            ),
            "expected MailSeated: {response:?}"
        );
    }
}

/// Re-stamp the request id (clients pass 0 and let `call` assign).
fn with_request_id(request: &ControlRequest, id: u64) -> ControlRequest {
    let mut value = serde_json::to_value(request).unwrap();
    value["request_id"] = serde_json::json!(id);
    serde_json::from_value(value).unwrap()
}

fn ok_data(response: ControlResponse) -> ControlResponseData {
    match response.body {
        ControlResponseBody::Ok { response } => response,
        other => panic!("expected Ok: {other:?}"),
    }
}

#[test]
fn mail_full_loop_two_sessions_no_external_watcher() {
    let socket = socket_path();
    let data = data_dir();
    let _server = start_server(&socket, &data);

    let mut kiro_sb = Client::connect(&socket);
    kiro_sb.hello("kiro-sb");
    let mut kiro_pm = Client::connect(&socket);
    kiro_pm.hello("kiro-pm");

    // kiro-pm's session: the pane consumes exactly one doorbell payload
    // (PMUX_MAIL\r = 10 bytes), then prints ATE as proof.
    let created = ok_data(kiro_sb.call(&ControlRequest::CreateSession {
        version: PROTOCOL_VERSION,
        request_id: 0,
        name: "pm".into(),
        spawn: prismattyc_mux::SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![
                "-c".into(),
                "printf 'READY\\n'; dd bs=1 count=10 of=/dev/null 2>/dev/null; printf 'ATE\\n'; exec sleep 999".into(),
            ],
            cwd: Some(std::env::temp_dir()),
            env: Default::default(),
        },
        cols: None,
        rows: None,
        agent_id: Some("kiro-pm".into()),
        headless: false,
    }));
    let ControlResponseData::Session {
        pane_id: pm_pane, ..
    } = created
    else {
        panic!("expected Session: {created:?}");
    };

    // Wait for the pane to spawn and go quiet: the doorbell defers on
    // recent output (MAIL_INJECT_QUIET_MS), so send only after the pane
    // has been silent past the quiet window.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = kiro_sb.call(&ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 0,
            client_id: kiro_sb.client_id,
            pane_id: pm_pane,
        });
        if let ControlResponseData::PaneContent { content } = ok_data(read) {
            if content.lines.join("\n").contains("READY") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "pane never printed READY");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(3_200));

    // Send. The in-process doorbell must arm attention and inject.
    let sent = ok_data(kiro_sb.call(&ControlRequest::MailSend {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_sb.client_id,
        to: "kiro-pm".into(),
        summary: "e2e dogfood".into(),
        body: "full loop, no watcher".into(),
    }));
    let ControlResponseData::MailSent { id, depth: 1 } = sent else {
        panic!("expected MailSent depth 1: {sent:?}");
    };

    // MailWait wakes on the condvar, well under the timeout.
    let started = Instant::now();
    let waited = ok_data(kiro_pm.call(&ControlRequest::MailWait {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_pm.client_id,
        timeout_ms: 10_000,
    }));
    assert!(
        matches!(waited, ControlResponseData::MailDepth { open: 1, .. }),
        "wait must report the new letter: {waited:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "condvar wait must wake on send, not run to timeout"
    );

    // Attention armed on kiro-pm's pane, and the pane consumed the
    // injected token — with no external watcher process anywhere.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attention_armed = false;
    let mut ate = false;
    while Instant::now() < deadline && !(attention_armed && ate) {
        let snapshot = ok_data(kiro_sb.call(&ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 0,
        }));
        let ControlResponseData::Snapshot { snapshot } = snapshot else {
            panic!("expected Snapshot: {snapshot:?}");
        };
        let pane = snapshot
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .find(|p| p.id == pm_pane)
            .expect("pm pane in snapshot");
        attention_armed = pane
            .mail
            .as_ref()
            .is_some_and(|m| m.cell == "mail" && m.depth == 1);
        let read = ok_data(kiro_sb.call(&ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id: 0,
            client_id: kiro_sb.client_id,
            pane_id: pm_pane,
        }));
        let ControlResponseData::PaneContent { content } = read else {
            panic!("expected PaneContent: {read:?}");
        };
        ate = content.lines.join("\n").contains("ATE");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(attention_armed, "sticky mail attention must arm");
    assert!(ate, "pane must consume the PMUX_MAIL doorbell");

    // Claim returns the letter; the indicator clears on read.
    let claimed = ok_data(kiro_pm.call(&ControlRequest::MailClaim {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_pm.client_id,
    }));
    let ControlResponseData::MailLetters { letters } = claimed else {
        panic!("expected MailLetters: {claimed:?}");
    };
    assert_eq!(letters.len(), 1);
    assert_eq!(letters[0].id, id);
    assert_eq!(letters[0].from, "kiro-sb");
    assert_eq!(letters[0].to, "kiro-pm");
    assert_eq!(letters[0].summary, "e2e dogfood");
    assert_eq!(letters[0].body, "full loop, no watcher");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = ok_data(kiro_pm.call(&ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 0,
        }));
        let ControlResponseData::Snapshot { snapshot } = snapshot else {
            panic!("expected Snapshot: {snapshot:?}");
        };
        let pane = snapshot
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .find(|p| p.id == pm_pane)
            .expect("pm pane in snapshot");
        if pane.mail.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attention must clear after claim"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Commit drains the mailbox for good.
    let committed = ok_data(kiro_pm.call(&ControlRequest::MailCommit {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_pm.client_id,
        ids: vec![id],
    }));
    assert!(
        matches!(
            committed,
            ControlResponseData::MailCommitted { committed: 1 }
        ),
        "{committed:?}"
    );
    let inbox = ok_data(kiro_pm.call(&ControlRequest::MailInbox {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_pm.client_id,
    }));
    assert!(
        matches!(inbox, ControlResponseData::MailDepth { open: 0, held: 0 }),
        "mailbox must be empty after commit: {inbox:?}"
    );

    // PT-293: the pane log, not only the Events channel, must carry depth 0.
    // The windowed host reads mail_depth from MailDepth log frames.
    let subscribed = ok_data(kiro_sb.call(&ControlRequest::SubscribePane {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: kiro_sb.client_id,
        pane_id: pm_pane,
        from_seq: 0,
        timeout_ms: 0,
    }));
    let ControlResponseData::PaneSubscribe { events, .. } = subscribed else {
        panic!("expected PaneSubscribe: {subscribed:?}");
    };
    let depths: Vec<u32> = events
        .iter()
        .filter_map(|frame| match frame.event {
            PaneEvent::MailDepth { depth } => Some(depth),
            _ => None,
        })
        .collect();
    assert!(
        depths.contains(&1),
        "raise must log MailDepth 1, got {depths:?}"
    );
    assert_eq!(
        depths.last().copied(),
        Some(0),
        "clear must log MailDepth 0, got {depths:?}"
    );
}
