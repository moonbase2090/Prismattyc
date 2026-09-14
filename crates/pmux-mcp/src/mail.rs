//! One Mail* round-trip on the pmux control socket.
//!
//! Each MCP tool call is one connection: RegisterClient, MailHello,
//! one mailbox op, drop. Identity is the process `--as` / env agent;
//! `MailSend` has no `from` field.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use prismattyc_mux::mailbox::AgentId;
use prismattyc_mux::{
    default_socket_path, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData,
    PROTOCOL_VERSION,
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
    serde_json::to_writer(&mut *stream, &request).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(io_err)?;
    stream.flush().map_err(io_err)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(io_err)?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(io_err)?;
    if line.is_empty() {
        return Err("mux closed without a reply".into());
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
        ControlResponseBody::Error { error } => Err(error.to_string()),
    }
}

fn io_err(error: io::Error) -> String {
    error.to_string()
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
        let ControlResponseData::MailSent { id, depth: 1 } = sent else {
            panic!("expected MailSent: {sent:?}");
        };
        let claimed = exchange(&socket, &recipient, MailOp::Claim).expect("claim");
        let ControlResponseData::MailLetters { letters } = claimed else {
            panic!("expected MailLetters: {claimed:?}");
        };
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].id, id);
        assert_eq!(letters[0].from, "kiro-sb");
        assert_eq!(letters[0].body, "over mail star");
        let committed =
            exchange(&socket, &recipient, MailOp::Commit { ids: vec![id] }).expect("commit");
        assert!(matches!(
            committed,
            ControlResponseData::MailCommitted { committed: 1 }
        ));
        let inbox = exchange(&socket, &recipient, MailOp::Inbox).expect("inbox");
        assert!(matches!(
            inbox,
            ControlResponseData::MailDepth { open: 0, held: 0 }
        ));
        let _ = std::fs::remove_file(&socket);
    }
}
