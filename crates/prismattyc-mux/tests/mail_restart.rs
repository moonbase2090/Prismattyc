//! (story 1b.2): mailbox durability across a real `pmuxd` restart.
//!
//! Open and held letters live in `$XDG_DATA_HOME/prismattyc/mail.db`. After
//! the daemon exits and a new process opens the same path, `MailClaim`
//! recovers every uncommitted letter. No sessions or doorbells — mail is
//! keyed by agent id.

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

fn socket_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "pmux-mailrestart-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn data_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pmux-mailrestart-data-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Owns the `pmuxd` child and its socket. Data dir cleanup is separate so
/// a restart can reopen the same mailbox.
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
    };
    for _ in 0..100 {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not publish {}", socket.display());
}

/// Ask the daemon to exit, then wait so SQLite can close the mailbox.
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
    // Prevent Drop from SIGKILL after a clean exit.
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

fn send(client: &mut Client, to: &str, summary: &str, body: &str) -> (String, u32) {
    let sent = ok_data(client.call(&ControlRequest::MailSend {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
        to: to.into(),
        summary: summary.into(),
        body: body.into(),
    }));
    let ControlResponseData::MailSent { id, depth } = sent else {
        panic!("expected MailSent: {sent:?}");
    };
    (id, depth)
}

fn inbox(client: &mut Client) -> (u32, u32) {
    let depth = ok_data(client.call(&ControlRequest::MailInbox {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
    }));
    let ControlResponseData::MailDepth { open, held } = depth else {
        panic!("expected MailDepth: {depth:?}");
    };
    (open, held)
}

fn claim(client: &mut Client) -> Vec<prismattyc_mux::mailbox::Letter> {
    let claimed = ok_data(client.call(&ControlRequest::MailClaim {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: client.client_id,
    }));
    let ControlResponseData::MailLetters { letters } = claimed else {
        panic!("expected MailLetters: {claimed:?}");
    };
    letters
}

#[test]
fn open_and_held_letters_survive_pmuxd_restart() {
    let data = DataGuard(data_dir());
    let socket = socket_path();
    let server = start_server(&socket, &data.0);

    let mut sender = Client::connect(&socket);
    sender.hello("kiro-sb");
    let mut open_agent = Client::connect(&socket);
    open_agent.hello("kiro-pm");
    let mut held_agent = Client::connect(&socket);
    held_agent.hello("kiro-xx");

    let (open_a, depth) = send(&mut sender, "kiro-pm", "open-a", "first open");
    assert_eq!(depth, 1);
    let (open_b, depth) = send(&mut sender, "kiro-pm", "open-b", "second open");
    assert_eq!(depth, 2);
    assert_eq!(inbox(&mut open_agent), (2, 0));

    let (held_id, depth) = send(&mut sender, "kiro-xx", "held-one", "claimed before restart");
    assert_eq!(depth, 1);
    let held = claim(&mut held_agent);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, held_id);
    assert_eq!(inbox(&mut held_agent), (0, 1));

    shutdown_clean(&mut sender, server);

    let socket = socket_path();
    let _server = start_server(&socket, &data.0);

    let mut open_agent = Client::connect(&socket);
    open_agent.hello("kiro-pm");
    let mut held_agent = Client::connect(&socket);
    held_agent.hello("kiro-xx");

    assert_eq!(
        inbox(&mut open_agent),
        (2, 0),
        "open letters must survive restart"
    );
    let opened = claim(&mut open_agent);
    assert_eq!(opened.len(), 2);
    assert_eq!(opened[0].id, open_a);
    assert_eq!(opened[0].summary, "open-a");
    assert_eq!(opened[0].body, "first open");
    assert_eq!(opened[0].from, "kiro-sb");
    assert_eq!(opened[0].to, "kiro-pm");
    assert_eq!(opened[1].id, open_b);
    assert_eq!(opened[1].summary, "open-b");
    assert_eq!(opened[1].body, "second open");

    assert_eq!(
        inbox(&mut held_agent),
        (0, 1),
        "held letters must survive restart"
    );
    let held = claim(&mut held_agent);
    assert_eq!(held.len(), 1, "claim re-lists held letters after restart");
    assert_eq!(held[0].id, held_id);
    assert_eq!(held[0].summary, "held-one");
    assert_eq!(held[0].body, "claimed before restart");
    assert_eq!(held[0].from, "kiro-sb");
    assert_eq!(held[0].to, "kiro-xx");

    let committed = ok_data(held_agent.call(&ControlRequest::MailCommit {
        version: PROTOCOL_VERSION,
        request_id: 0,
        client_id: held_agent.client_id,
        ids: vec![held_id],
    }));
    assert!(
        matches!(
            committed,
            ControlResponseData::MailCommitted { committed: 1 }
        ),
        "{committed:?}"
    );
    assert_eq!(inbox(&mut held_agent), (0, 0));
}
