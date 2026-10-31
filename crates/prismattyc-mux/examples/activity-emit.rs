// SPDX-License-Identifier: MIT
//! Opt-in, metadata-only activity adapter. Never prints hook-control output.

use std::io::{self, Read};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use prismattyc_mux::activity::{self, ActivityInput, ActivityKind, ActivitySource};
use prismattyc_mux::mailbox::AgentId;
use prismattyc_mux::{ControlRequest, ControlResponseData, PROTOCOL_VERSION};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_INPUT: usize = 1024 * 1024;

enum Mode {
    Hook,
    Notify(String),
    Event(String),
}

struct Cli {
    socket: PathBuf,
    seat: AgentId,
    mode: Mode,
}

fn parse() -> Result<Cli> {
    let mut args = std::env::args().skip(1);
    let mut socket = None;
    let mut seat = None;
    let mut mode = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(args.next().context("missing socket")?)),
            "--seat" => seat = Some(AgentId::new(args.next().context("missing seat")?)?),
            "--hook" if mode.is_none() => mode = Some(Mode::Hook),
            "--notify" if mode.is_none() => {
                mode = Some(Mode::Notify(args.next().context("missing notification")?));
            }
            "--event" if mode.is_none() => {
                mode = Some(Mode::Event(args.next().context("missing event")?));
            }
            "--help" | "-h" => {
                println!("activity-emit --socket PATH --seat ID --hook|--notify JSON|--event JSON");
                std::process::exit(0);
            }
            _ => bail!("unknown or repeated activity adapter option"),
        }
    }
    Ok(Cli {
        socket: socket.context("--socket is required")?,
        seat: seat.context("--seat is required")?,
        mode: mode.context("an input mode is required")?,
    })
}

fn opaque(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("missing activity correlation identifier")
}

fn hook_kind(name: &str, tool: Option<&str>, failed: bool) -> Option<ActivityKind> {
    use ActivityKind::*;
    match name {
        "SessionStart" => Some(SessionStarted),
        "SessionEnd" => Some(SessionFinished),
        "UserPromptSubmit" => Some(TurnStarted),
        "Stop" => Some(TurnFinished),
        "PermissionRequest" => Some(Waiting),
        "Interrupt" => Some(Stopped),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            let start = name == "PreToolUse";
            match tool {
                Some("Bash" | "exec_command" | "shell_command") => Some(if start {
                    CommandStarted
                } else if failed {
                    CommandFailed
                } else {
                    CommandFinished
                }),
                Some("apply_patch" | "Edit" | "Write") => Some(if start {
                    EditStarted
                } else if failed {
                    EditFailed
                } else {
                    EditFinished
                }),
                _ => Some(if start {
                    ToolStarted
                } else if failed {
                    ToolFailed
                } else {
                    ToolFinished
                }),
            }
        }
        _ => None,
    }
}

fn normalize(seat: AgentId, input: &Value, notify: bool) -> Result<Option<ActivityInput>> {
    let (source, session, kind, key) = if notify {
        if input.get("type").and_then(Value::as_str) != Some("agent-turn-complete") {
            return Ok(None);
        }
        (
            ActivitySource::Notify,
            string(input, "thread-id")?,
            ActivityKind::TurnFinished,
            format!("notify:{}", string(input, "turn-id")?),
        )
    } else {
        let name = string(input, "hook_event_name")?;
        let failed = name == "PostToolUseFailure"
            || input
                .pointer("/tool_response/exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
        let Some(kind) = hook_kind(name, input.get("tool_name").and_then(Value::as_str), failed)
        else {
            return Ok(None);
        };
        let correlation = input
            .get("tool_use_id")
            .or_else(|| input.get("turn_id"))
            .and_then(Value::as_str);
        // Lifecycle events can repeat in the same session without a turn id.
        let key = match correlation {
            Some(id) => format!("hook:{name}:{id}"),
            None => {
                let mut nonce = [0u8; 16];
                getrandom::fill(&mut nonce)
                    .map_err(|_| anyhow::anyhow!("activity nonce unavailable"))?;
                format!("hook:{name}:{nonce:x?}")
            }
        };
        (
            ActivitySource::Hooks,
            string(input, "session_id")?,
            kind,
            key,
        )
    };
    Ok(Some(ActivityInput {
        schema_version: 1,
        seat,
        run_id: opaque(session),
        event_id: opaque(&key),
        source,
        kind,
    }))
}

fn publish(cli: &Cli) -> Result<()> {
    let event = match &cli.mode {
        Mode::Hook => {
            let mut bytes = Vec::new();
            io::stdin()
                .take((MAX_INPUT + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_INPUT {
                bail!("activity input exceeds limit");
            }
            let input: Value = serde_json::from_slice(&bytes).context("invalid hook input")?;
            normalize(cli.seat.clone(), &input, false)?
        }
        Mode::Notify(raw) => {
            if raw.len() > MAX_INPUT {
                bail!("activity input exceeds limit");
            }
            let input: Value = serde_json::from_str(raw).context("invalid notification")?;
            normalize(cli.seat.clone(), &input, true)?
        }
        Mode::Event(raw) => {
            if raw.len() > MAX_INPUT {
                bail!("activity input exceeds limit");
            }
            let mut event: ActivityInput =
                serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid activity event"))?;
            event.seat = cli.seat.clone();
            Some(event)
        }
    };
    let Some(event) = event else { return Ok(()) };
    match activity::exchange(
        &cli.socket,
        ControlRequest::PublishActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            event,
        },
    )? {
        ControlResponseData::ActivityPublished { .. } => Ok(()),
        _ => bail!("unexpected activity response"),
    }
}

fn main() -> Result<()> {
    let cli = parse()?;
    let result = publish(&cli);
    if matches!(cli.mode, Mode::Hook | Mode::Notify(_)) {
        if result.is_err() {
            // Observation must not reject a tool or disclose its input.
            eprintln!("activity observation unavailable");
        }
        Ok(())
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_omits_content_and_reports_only_turn_completion() {
        let event = normalize(
            AgentId::new("worker-a").unwrap(),
            &json!({"type":"agent-turn-complete", "thread-id":"session-a",
                "turn-id":"turn-a", "cwd":"private-path",
                "input-messages":["private-prompt"], "last-assistant-message":"private-output"}),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(event.kind, ActivityKind::TurnFinished);
        assert_eq!(event.source, ActivitySource::Notify);
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 6);
        let serialized = value.to_string();
        for private in ["private", "session-a", "turn-a"] {
            assert!(!serialized.contains(private));
        }
    }

    #[test]
    fn hook_classifies_operation_and_explicit_failure() {
        let event = normalize(
            AgentId::new("worker-b").unwrap(),
            &json!({"hook_event_name":"PostToolUseFailure", "session_id":"session-b",
                "tool_use_id":"call-b", "tool_name":"Bash",
                "tool_input":{"command":"private-command"}, "error":"private-error"}),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(event.kind, ActivityKind::CommandFailed);
        assert_eq!(event.source, ActivitySource::Hooks);
        assert!(!serde_json::to_string(&event).unwrap().contains("private"));
        assert_eq!(
            hook_kind("PreToolUse", Some("apply_patch"), false),
            Some(ActivityKind::EditStarted)
        );
        assert_eq!(
            hook_kind("PostToolUse", Some("other"), false),
            Some(ActivityKind::ToolFinished)
        );
        assert_eq!(hook_kind("Unknown", None, false), None);
    }
}
