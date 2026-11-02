// SPDX-License-Identifier: MIT
//! Opt-in, metadata-only activity adapter. Never prints hook-control output.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use prismattyc_mux::activity::{
    self, ActivityInput, ActivityKind, ActivityRejected, ActivitySource,
};
use prismattyc_mux::mailbox::AgentId;
use prismattyc_mux::{ControlErrorCode, ControlRequest, ControlResponseData, PROTOCOL_VERSION};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_INPUT: usize = 1024 * 1024;
pub(super) const MANAGED_MARKER: &str = "--managed-hook=pmux-activity-v1";

#[path = "activity_install.rs"]
mod install;

enum Mode {
    Hook,
    Notify(String),
    Event(String),
    Install { file: Option<PathBuf>, remove: bool },
}

struct Cli {
    socket: PathBuf,
    seat: AgentId,
    dynamic_seat: bool,
    all_harnesses: bool,
    hook_event: Option<String>,
    mode: Mode,
    schema_version: bool,
    failure_hook: bool,
}

fn parse(socket: &Path, args: impl Iterator<Item = String>) -> Result<Cli> {
    let mut args = args.peekable();
    let mut seat = None;
    let mut mode = None;
    let mut schema_version = false;
    let mut failure_hook = false;
    let mut all_harnesses = false;
    let mut hook_event = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seat" if seat.is_none() => {
                seat = Some(AgentId::new(args.next().context("missing seat")?)?);
            }
            "--hook" if mode.is_none() => mode = Some(Mode::Hook),
            "--notify" if mode.is_none() => {
                mode = Some(Mode::Notify(args.next().context("missing notification")?));
            }
            "--event" if mode.is_none() => {
                mode = Some(Mode::Event(args.next().context("missing event")?));
            }
            "--install-hooks" if mode.is_none() => {
                mode = Some(Mode::Install {
                    file: args
                        .peek()
                        .filter(|next| !next.starts_with('-'))
                        .cloned()
                        .map(|_| PathBuf::from(args.next().expect("peeked path exists"))),
                    remove: false,
                });
            }
            "--remove-hooks" if mode.is_none() => {
                mode = Some(Mode::Install {
                    file: Some(PathBuf::from(args.next().context("missing hook file")?)),
                    remove: true,
                });
            }
            "--all-harnesses" if !all_harnesses => all_harnesses = true,
            "--hook-event" if hook_event.is_none() => {
                hook_event = Some(args.next().context("missing hook event")?);
            }
            "--schema-version" if !schema_version => {
                if args.next().as_deref() != Some("1") {
                    bail!("hook settings schema must be 1");
                }
                schema_version = true;
            }
            "--failure-hook" if !failure_hook => failure_hook = true,
            MANAGED_MARKER => {}
            "--help" | "-h" => {
                println!("pmux [--socket PATH] activity-emit --seat ID --hook|--notify JSON|--event JSON\npmux [--socket PATH] activity-emit --seat ID --install-hooks FILE [--schema-version 1] [--failure-hook]\npmux [--socket PATH] activity-emit --install-hooks --all-harnesses\npmux [--socket PATH] activity-emit --seat ID --remove-hooks FILE");
                std::process::exit(0);
            }
            _ => bail!("unknown or repeated activity adapter option"),
        }
    }
    if (schema_version || failure_hook) && !matches!(mode, Some(Mode::Install { .. })) {
        bail!("hook settings options require an install or removal operation");
    }
    let mode = mode.context("an input mode is required")?;
    if all_harnesses
        && !matches!(
            mode,
            Mode::Install {
                file: None,
                remove: false
            }
        )
    {
        bail!("--all-harnesses requires --install-hooks without a file");
    }
    if matches!(mode, Mode::Install { file: None, .. }) && !all_harnesses {
        bail!("--install-hooks requires a hook file or --all-harnesses");
    }
    if all_harnesses && seat.is_some() {
        bail!("--all-harnesses uses PMUX_AGENT and does not accept --seat");
    }
    if hook_event.is_some() && !matches!(mode, Mode::Hook) {
        bail!("--hook-event requires --hook");
    }
    let dynamic_seat = seat.is_none();
    let seat = match seat {
        Some(seat) => seat,
        None => AgentId::new(
            std::env::var("PMUX_AGENT").context("--seat is required (or set PMUX_AGENT)")?,
        )?,
    };
    Ok(Cli {
        socket: socket.to_path_buf(),
        seat,
        dynamic_seat,
        all_harnesses,
        hook_event,
        mode,
        schema_version,
        failure_hook,
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
                Some("Bash" | "PowerShell" | "shell" | "exec_command" | "shell_command") => {
                    Some(if start {
                        CommandStarted
                    } else if failed {
                        CommandFailed
                    } else {
                        CommandFinished
                    })
                }
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
        let name = input
            .get("hook_event_name")
            .and_then(Value::as_str)
            .or_else(|| input.get("event").and_then(Value::as_str))
            .context("missing activity hook event")?;
        let name = match name {
            "sessionStart" => "SessionStart",
            "sessionEnd" => "SessionEnd",
            "beforeSubmitPrompt" | "PromptSubmit" => "UserPromptSubmit",
            "preToolUse" => "PreToolUse",
            "postToolUse" => "PostToolUse",
            "stop" | "agentStop" => "Stop",
            "AgentStop" => "Stop",
            other => other,
        };
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
            input
                .get("session_id")
                .or_else(|| input.get("conversation_id"))
                .or_else(|| input.get("thread_id"))
                .or_else(|| input.get("sessionId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .context("missing activity correlation identifier")?,
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
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = io::stdin()
                    .take((MAX_INPUT + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = sender.send(result);
            });
            let bytes = receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .context("hook input deadline exceeded")??;
            if bytes.len() > MAX_INPUT {
                bail!("activity input exceeds limit");
            }
            let mut input: Value = serde_json::from_slice(&bytes).context("invalid hook input")?;
            if input.get("hook_event_name").is_none() {
                if let (Some(event), Some(object)) =
                    (cli.hook_event.as_ref(), input.as_object_mut())
                {
                    object.insert("hook_event_name".into(), Value::String(event.clone()));
                }
            }
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
        Mode::Install { file, remove } => {
            return install::run(cli, file.as_ref(), *remove);
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

pub(super) fn run(socket: &Path, args: Vec<String>) -> Result<()> {
    let observing = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--hook" | "--notify"));
    let result = parse(socket, args.into_iter()).and_then(|cli| publish(&cli));
    if observing {
        if result.as_ref().is_err_and(|error| {
            error.downcast_ref::<ActivityRejected>().map(|e| e.code)
                != Some(ControlErrorCode::ActivityDisabled)
        }) {
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
