//! Transparent stdio supervisor for MCP hosts that do not respawn a
//! failed server process.
//!
//! MCP stdio is a stateful JSON-RPC session. Merely starting a fresh
//! child after EOF is insufficient: the replacement must see the
//! original `initialize` request and `notifications/initialized`
//! notification before it may serve tools. This proxy keeps those two
//! lines, replays them after a child exit, and discards the duplicate
//! initialize response so the host continues to see one session.

use std::collections::BTreeMap;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use prismattyc_mux::mailbox::AgentId;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines, Stdout};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const RESTART_ATTEMPTS: usize = 5;

struct ChildSession {
    process: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

#[derive(Default)]
struct ProtocolState {
    initialize_line: Option<String>,
    initialize_id: Option<Value>,
    initialized_line: Option<String>,
    initialize_complete: bool,
    pending: BTreeMap<String, Value>,
}

#[derive(Debug)]
enum HostMessage {
    Request { id: Value, initialize: bool },
    Initialized,
    Other,
}

impl ProtocolState {
    fn record_host_message(&mut self, line: &str, message: &HostMessage) {
        match message {
            HostMessage::Request { id, initialize } => {
                self.pending.insert(id_key(id), id.clone());
                if *initialize {
                    self.initialize_line = Some(line.to_owned());
                    self.initialize_id = Some(id.clone());
                }
            }
            HostMessage::Initialized => self.initialized_line = Some(line.to_owned()),
            HostMessage::Other => {}
        }
    }

    fn record_child_message(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(id) = message.get("id") else {
            return;
        };
        self.pending.remove(&id_key(id));
        if self.initialize_id.as_ref() == Some(id) {
            self.initialize_complete = true;
        }
    }

    fn take_interrupted_requests(&mut self) -> Vec<Value> {
        let initialize_key = self.initialize_id.as_ref().map(id_key);
        let interrupted = self
            .pending
            .iter()
            .filter(|(key, _)| Some(key.as_str()) != initialize_key.as_deref())
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>();
        self.pending
            .retain(|key, _| Some(key.as_str()) == initialize_key.as_deref());
        interrupted
    }
}

pub async fn run(agent: &AgentId) -> io::Result<()> {
    let mut executable = std::env::current_exe()?;
    let mut child_version = env!("CARGO_PKG_VERSION").to_string();
    let mut host_input = BufReader::new(tokio::io::stdin()).lines();
    let mut host_output = tokio::io::stdout();
    let mut state = ProtocolState::default();
    let mut child = spawn_child(&executable, agent)?;
    let socket = crate::mail::mux_socket();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                use prismattyc_mux::component_restart as requests;
                let _=requests::register_version(&socket,"mcp",&child_version);
                if let Some(request)=requests::take(&socket,"mcp") {
                    let replacement=prismattyc_mux::release_update::installed_binary("pmux-mcp").unwrap_or_else(||executable.clone());
                    child=restart_child(child,&replacement,agent,&mut state,&mut host_output).await?;
                    executable=replacement;
                    let probe=executable.clone();
                    child_version=tokio::task::spawn_blocking(move ||prismattyc_mux::release_update::version_label(&probe)).await.ok().and_then(Result::ok).unwrap_or_else(||"unknown".into());
                    let _=requests::register_version(&socket,"mcp",&child_version);
                    let _=requests::respond(&socket,&request,"restarted","adapter restarted; in-flight operations were not replayed");
                }
            }
            host_line = host_input.next_line() => {
                let Some(line) = host_line? else {
                    stop_child(&mut child).await;
                    return Ok(());
                };
                let message = classify_host_message(&line);
                // A failed write/flush can follow partial delivery. Track the
                // request before writing so recovery reports uncertainty and
                // never repeats a tool call that may already have executed.
                state.record_host_message(&line, &message);
                if let Err(write_error) = write_line(&mut child.stdin, &line).await {
                    eprintln!("pmux-mcp supervisor: child write failed: {write_error}; restarting");
                    child = restart_child(
                        child,
                        &executable,
                        agent,
                        &mut state,
                        &mut host_output,
                    ).await?;
                }
            }
            child_line = child.stdout.next_line() => {
                match child_line? {
                    Some(line) => {
                        state.record_child_message(&line);
                        write_line(&mut host_output, &line).await?;
                    }
                    None => {
                        child = restart_child(
                            child,
                            &executable,
                            agent,
                            &mut state,
                            &mut host_output,
                        ).await?;
                    }
                }
            }
        }
    }
}

async fn restart_child(
    mut old_child: ChildSession,
    executable: &std::path::Path,
    agent: &AgentId,
    state: &mut ProtocolState,
    host_output: &mut Stdout,
) -> io::Result<ChildSession> {
    stop_child(&mut old_child).await;

    for id in state.take_interrupted_requests() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32001,
                "message": "pmux-mcp child exited; request was not replayed"
            }
        });
        write_line(host_output, &response.to_string()).await?;
    }

    let mut last_error = None;
    for attempt in 1..=RESTART_ATTEMPTS {
        match spawn_child(executable, agent) {
            Ok(mut child) => match bootstrap_child(&mut child, state, host_output).await {
                Ok(()) => {
                    eprintln!("pmux-mcp supervisor: child restarted (attempt {attempt})");
                    return Ok(child);
                }
                Err(error) => {
                    eprintln!(
                        "pmux-mcp supervisor: child bootstrap failed on attempt {attempt}: {error}"
                    );
                    stop_child(&mut child).await;
                    last_error = Some(error);
                }
            },
            Err(error) => {
                eprintln!("pmux-mcp supervisor: child spawn failed on attempt {attempt}: {error}");
                last_error = Some(error);
            }
        }
        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
    }

    Err(last_error.unwrap_or_else(|| io::Error::other("child restart failed")))
}

async fn bootstrap_child(
    child: &mut ChildSession,
    state: &ProtocolState,
    host_output: &mut Stdout,
) -> io::Result<()> {
    let Some(initialize_line) = state.initialize_line.as_deref() else {
        return Ok(());
    };
    write_line(&mut child.stdin, initialize_line).await?;

    if state.initialize_complete {
        let initialize_id = state
            .initialize_id
            .as_ref()
            .expect("initialize line and completed response require an id");
        loop {
            let line = child.stdout.next_line().await?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "child exited during initialize replay",
                )
            })?;
            if response_has_id(&line, initialize_id) {
                break;
            }
            write_line(host_output, &line).await?;
        }
        if let Some(initialized_line) = state.initialized_line.as_deref() {
            write_line(&mut child.stdin, initialized_line).await?;
        }
    }

    Ok(())
}

fn spawn_child(executable: &std::path::Path, agent: &AgentId) -> io::Result<ChildSession> {
    let mut process = Command::new(executable)
        .arg("--as")
        .arg(agent.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = process
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("child stdin was not piped"))?;
    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout was not piped"))?;
    Ok(ChildSession {
        process,
        stdin,
        stdout: BufReader::new(stdout).lines(),
    })
}

async fn stop_child(child: &mut ChildSession) {
    let _ = child.process.kill().await;
    let _ = child.process.wait().await;
}

async fn write_line<W>(writer: &mut W, line: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

fn classify_host_message(line: &str) -> HostMessage {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return HostMessage::Other;
    };
    let method = message.get("method").and_then(Value::as_str);
    if method == Some("notifications/initialized") {
        return HostMessage::Initialized;
    }
    let Some(id) = message.get("id") else {
        return HostMessage::Other;
    };
    HostMessage::Request {
        id: id.clone(),
        initialize: method == Some("initialize"),
    }
}

fn response_has_id(line: &str, expected: &Value) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| message.get("id").cloned())
        .as_ref()
        == Some(expected)
}

fn id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_initialize_and_initialized_notification() {
        let mut state = ProtocolState::default();
        let initialize = r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#;
        let message = classify_host_message(initialize);
        state.record_host_message(initialize, &message);
        assert_eq!(state.initialize_id, Some(json!(7)));
        assert_eq!(state.initialize_line.as_deref(), Some(initialize));

        state.record_child_message(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#);
        assert!(state.initialize_complete);
        assert!(state.pending.is_empty());

        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let message = classify_host_message(initialized);
        state.record_host_message(initialized, &message);
        assert_eq!(state.initialized_line.as_deref(), Some(initialized));
    }

    #[test]
    fn interrupted_requests_exclude_an_incomplete_initialize() {
        let mut state = ProtocolState::default();
        let initialize = r#"{"jsonrpc":"2.0","id":"init","method":"initialize"}"#;
        let call = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call"}"#;
        let message = classify_host_message(initialize);
        state.record_host_message(initialize, &message);
        let message = classify_host_message(call);
        state.record_host_message(call, &message);

        assert_eq!(state.take_interrupted_requests(), vec![json!(9)]);
        assert_eq!(state.pending.len(), 1);
        assert!(state.pending.contains_key("\"init\""));
    }

    #[test]
    fn only_initialized_notification_is_cached() {
        assert!(matches!(
            classify_host_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            HostMessage::Initialized
        ));
        assert!(matches!(
            classify_host_message(r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#),
            HostMessage::Other
        ));
    }

    #[test]
    fn matches_numeric_and_string_response_ids_exactly() {
        assert!(response_has_id(
            r#"{"jsonrpc":"2.0","id":4,"result":{}}"#,
            &json!(4)
        ));
        assert!(!response_has_id(
            r#"{"jsonrpc":"2.0","id":"4","result":{}}"#,
            &json!(4)
        ));
    }
}
