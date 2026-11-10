//! One Mail* round-trip on the pmux control socket.
//!
//! Each MCP tool call is one connection: RegisterClient, MailHello,
//! one mailbox op, drop. Identity is the process `--as` / env agent;
//! `MailSend` has no `from` field.

use prismattyc_mux::local_socket::UnixStream;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use prismattyc_mux::mailbox::AgentId;
use prismattyc_mux::{
    agent_transport_error, daemon_exit_report, default_socket_path, is_daemon_exit_report,
    transport_link_lost, ConnectedRole, ControlRequest, ControlResponse, ControlResponseBody,
    ControlResponseData, PROTOCOL_VERSION,
};

/// One mailbox verb after hello.
pub enum MailOp {
    Send {
        to: String,
        summary: String,
        body: String,
    },
    Claim,
    Commit {
        ids: Vec<String>,
    },
    Release {
        ids: Vec<String>,
    },
    Inbox,
    Alias {
        name: String,
    },
    Who,
    Broadcast {
        summary: String,
        body: String,
    },
}

/// Resolve the mux socket: `PMUX_SOCKET`, else `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`.
#[must_use]
pub fn mux_socket() -> PathBuf {
    if let Ok(value) = std::env::var("PMUX_SOCKET") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    default_socket_path("default").unwrap_or_else(|_| PathBuf::from("/tmp/prismattyc-pmux.sock"))
}

/// Register, hello as `agent`, run `op`, return the success payload.
///
/// # Errors
///
/// Connect, I/O, protocol, or `MailRefused` reasons.
pub fn exchange(socket: &Path, agent: &AgentId, op: MailOp) -> Result<ControlResponseData, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let mut next_id = 0_u64;
    let registered = call(&mut stream, &mut next_id, |request_id| {
        ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        }
    })?;
    let ControlResponseData::ClientRegistered { client_id } = registered else {
        return Err(format!("unexpected register reply: {registered:?}"));
    };
    let seated = call(&mut stream, &mut next_id, |request_id| {
        ControlRequest::MailHello {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            agent: agent.to_string(),
        }
    })?;
    if !matches!(seated, ControlResponseData::MailSeated { .. }) {
        return Err(format!("unexpected hello reply: {seated:?}"));
    }
    call(&mut stream, &mut next_id, |request_id| match &op {
        MailOp::Send { to, summary, body } => ControlRequest::MailSend {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            to: to.clone(),
            summary: summary.clone(),
            body: body.clone(),
        },
        MailOp::Claim => ControlRequest::MailClaim {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        },
        MailOp::Commit { ids } => ControlRequest::MailCommit {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            ids: ids.clone(),
        },
        MailOp::Release { ids } => ControlRequest::MailRelease {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            ids: ids.clone(),
        },
        MailOp::Inbox => ControlRequest::MailInbox {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        },
        MailOp::Alias { name } => ControlRequest::MailAlias {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            name: name.clone(),
        },
        MailOp::Who => ControlRequest::MailWho {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
        },
        MailOp::Broadcast { summary, body } => ControlRequest::MailBroadcast {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            summary: summary.clone(),
            body: body.clone(),
        },
    })
}

/// Format a Mail* payload the way `pmux mail` prints it.
#[must_use]
pub fn format_data(data: &ControlResponseData) -> String {
    match data {
        ControlResponseData::MailSent { id, depth } => {
            format!("sent: {id} (depth {depth})\n")
        }
        ControlResponseData::MailLetters { letters } => {
            let mut out = format!("status: held  ({} letter(s))\n", letters.len());
            for letter in letters {
                let _ = std::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "\nid:       {}\nfrom:     {}\nsummary:  {}\n---\n{}\n",
                        letter.id, letter.from, letter.summary, letter.body
                    ),
                );
            }
            out
        }
        ControlResponseData::MailCommitted { committed } => {
            format!("committed: {committed}\n")
        }
        ControlResponseData::MailReleased { released } => {
            format!("released: {released}\n")
        }
        ControlResponseData::MailDepth { open, held } => {
            format!("open: {open} held: {held}\n")
        }
        ControlResponseData::MailAliased { name, agent } => {
            format!("aliased: {name} -> {agent}\n")
        }
        ControlResponseData::MailBroadcasted {
            delivered,
            recipients,
        } => format!("broadcasted: {delivered} ({})\n", recipients.join(", ")),
        ControlResponseData::MailPeers { peers } => {
            let mut out = String::new();
            for peer in peers {
                let live = if peer.pane_live {
                    "live"
                } else {
                    "no live pane"
                };
                if peer.aliases.is_empty() {
                    let _ = std::fmt::Write::write_fmt(
                        &mut out,
                        format_args!("{}  {}  ({live})\n", peer.agent_id, peer.session),
                    );
                } else {
                    let _ = std::fmt::Write::write_fmt(
                        &mut out,
                        format_args!(
                            "{}  {}  ({live}; aliases: {})\n",
                            peer.agent_id,
                            peer.session,
                            peer.aliases.join(", ")
                        ),
                    );
                }
            }
            out
        }
        ControlResponseData::MailRefused { reason } => format!("refused: {reason}\n"),
        other => format!("{other:?}\n"),
    }
}

fn call(
    stream: &mut UnixStream,
    next_id: &mut u64,
    build: impl FnOnce(u64) -> ControlRequest,
) -> Result<ControlResponseData, String> {
    *next_id += 1;
    let request = build(*next_id);
    serde_json::to_writer(&mut *stream, &request).map_err(agent_encode_error)?;
    stream.write_all(b"\n").map_err(agent_io)?;
    stream.flush().map_err(agent_io)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(agent_io)?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(agent_io)?;
    if line.is_empty() {
        return Err(daemon_exit_report(ConnectedRole::Agent));
    }
    let response: ControlResponse =
        serde_json::from_str(&line).map_err(|error| error.to_string())?;
    match response.body {
        ControlResponseBody::Ok { response } => {
            if let ControlResponseData::MailRefused { reason } = response {
                Err(reason)
            } else {
                Ok(response)
            }
        }
        ControlResponseBody::Error { error } => {
            if is_daemon_exit_report(&error.message) {
                Err(daemon_exit_report(ConnectedRole::Agent))
            } else {
                Err(error.to_string())
            }
        }
    }
}

fn agent_io(error: io::Error) -> String {
    agent_transport_error(&error)
}

fn agent_encode_error(error: serde_json::Error) -> String {
    match error.io_error_kind() {
        Some(kind) if transport_link_lost(&io::Error::new(kind, "control link lost")) => {
            daemon_exit_report(ConnectedRole::Agent)
        }
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_socket_honors_pmux_socket() {
        let prior = std::env::var_os("PMUX_SOCKET");
        std::env::set_var("PMUX_SOCKET", "/tmp/pt21-pmux.sock");
        let path = mux_socket();
        match prior {
            Some(value) => std::env::set_var("PMUX_SOCKET", value),
            None => std::env::remove_var("PMUX_SOCKET"),
        }
        assert_eq!(path, PathBuf::from("/tmp/pt21-pmux.sock"));
    }

    #[test]
    fn send_claim_commit_over_mux_socket() {
        use prismattyc_mux::mailbox::Store;
        use prismattyc_mux::{ControlPlane, ControlServer, Domain, WindowBounds};

        let domain = Domain::bootstrap("mcp").unwrap();
        let window = domain.sessions().next().unwrap().windows[0];
        let mut plane = ControlPlane::new(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            None,
        )
        .unwrap();
        plane.set_mail_store(Store::open_in_memory().unwrap());
        let socket = std::env::temp_dir().join(format!(
            "pmux-mcp-ex-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&socket);
        let _server = ControlServer::bind(&socket, plane).unwrap();

        let sender = AgentId::new("kiro-sb").unwrap();
        let recipient = AgentId::new("kiro-pm").unwrap();
        let sent = exchange(
            &socket,
            &sender,
            MailOp::Send {
                to: "kiro-pm".into(),
                summary: "mcp".into(),
                body: "over mail star".into(),
            },
        )
        .expect("send");
        assert!(format_data(&sent).ends_with(" (depth 1)\n"));
        let ControlResponseData::MailSent { id, depth: 1 } = sent else {
            panic!("expected MailSent: {sent:?}");
        };
        let claimed = exchange(&socket, &recipient, MailOp::Claim).expect("claim");
        let text = format_data(&claimed);
        assert!(text.starts_with("status: held  (1 letter(s))\n"));
        assert!(text.contains("from:     kiro-sb\nsummary:  mcp\n---\nover mail star\n"));
        let ControlResponseData::MailLetters { letters } = claimed else {
            panic!("expected MailLetters: {claimed:?}");
        };
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].id, id);
        assert_eq!(letters[0].from, "kiro-sb");
        assert_eq!(letters[0].body, "over mail star");
        let released = exchange(
            &socket,
            &recipient,
            MailOp::Release {
                ids: vec![id.clone()],
            },
        )
        .unwrap();
        assert_eq!(format_data(&released), "released: 1\n");
        assert_eq!(
            format_data(&exchange(&socket, &recipient, MailOp::Inbox).unwrap()),
            "open: 1 held: 0\n"
        );
        exchange(&socket, &recipient, MailOp::Claim).unwrap();
        let alias = exchange(
            &socket,
            &recipient,
            MailOp::Alias {
                name: "reviewer".into(),
            },
        )
        .unwrap();
        assert_eq!(format_data(&alias), "aliased: reviewer -> kiro-pm\n");
        let committed =
            exchange(&socket, &recipient, MailOp::Commit { ids: vec![id] }).expect("commit");
        assert_eq!(format_data(&committed), "committed: 1\n");
        assert!(matches!(
            committed,
            ControlResponseData::MailCommitted { committed: 1 }
        ));
        let inbox = exchange(&socket, &recipient, MailOp::Inbox).expect("inbox");
        let inbox_text = format_data(&inbox);
        assert_eq!(inbox_text, "open: 0 held: 0\n");
        let lower = inbox_text.to_ascii_lowercase();
        assert!(
            !lower.contains("stalled") && !lower.contains("dead") && !inbox_text.contains("exited"),
            "an empty inbox is quiet mail, not a dead daemon: {inbox_text}"
        );
        assert!(matches!(
            inbox,
            ControlResponseData::MailDepth { open: 0, held: 0 }
        ));
        let _ = std::fs::remove_file(&socket);
    }

    #[cfg(unix)]
    #[test]
    fn a_dropped_daemon_socket_is_an_agent_exit() {
        use std::io::BufRead;
        use std::os::unix::net::UnixListener;

        let socket = std::env::temp_dir().join(format!(
            "pmux-mcp-exit-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            drop(reader);
        });
        let agent = AgentId::new("kiro-sb").unwrap();
        let started = std::time::Instant::now();
        let error = exchange(&socket, &agent, MailOp::Inbox).expect_err("daemon dropped");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "an agent must not wait out a mail budget after the daemon leaves"
        );
        assert_eq!(error, "pmuxd exited while this agent was connected");
        assert!(!error.to_ascii_lowercase().contains("stalled"));
        server.join().unwrap();
        let _ = std::fs::remove_file(&socket);
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_daemon_is_not_an_agent_exit() {
        use std::io::BufRead;
        use std::os::unix::net::UnixListener;

        let socket = std::env::temp_dir().join(format!(
            "pmux-mcp-slow-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = reader.read_line(&mut line);
        });
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(80)))
            .unwrap();
        let mut next_id = 0;
        let error = call(&mut stream, &mut next_id, |request_id| {
            ControlRequest::RegisterClient {
                version: PROTOCOL_VERSION,
                request_id,
            }
        })
        .expect_err("read times out");
        drop(stream);
        let lower = error.to_ascii_lowercase();
        assert!(
            !error.contains("pmuxd exited")
                && !lower.contains("stalled")
                && !lower.contains("dead"),
            "a timeout while the daemon is still there is not an exit: {error}"
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(&socket);
    }
}
