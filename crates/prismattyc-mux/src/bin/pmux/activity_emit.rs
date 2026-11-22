// SPDX-License-Identifier: MIT
//! Opt-in, metadata-only activity adapter. Never prints hook-control output.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

fn display_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn secret_label(value: &str) -> bool {
    let key = value
        .trim_matches(['\'', '"'])
        .split(['=', ':'])
        .next()
        .unwrap_or(value)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "api_key",
        "api-key",
        "apikey",
        "auth",
    ]
    .iter()
    .any(|label| key.contains(label))
}

fn redacted_command(value: &str) -> String {
    let words: Vec<&str> = value.split_whitespace().collect();
    let mut output = Vec::new();
    let mut redact_next = false;
    let mut index = 0;
    while index < words.len() {
        let part = words[index];
        let token = part.trim_matches(['\'', '"']);
        if let Some(end) = private_key_end(&words, index) {
            output.push("[redacted private key]".to_owned());
            redact_next = false;
            index = end + 1;
            continue;
        }
        if redact_next {
            output.push("[redacted]".to_owned());
            redact_next = auth_scheme(token);
        } else if auth_scheme(token) {
            output.push(token.to_owned());
            redact_next = true;
        } else if secret_label(token) {
            if token.contains(['=', ':']) {
                let key = token.split(['=', ':']).next().unwrap_or_default();
                output.push(format!("{key}=[redacted]"));
                redact_next = token.ends_with(':');
            } else {
                output.push(part.to_owned());
                redact_next = true;
            }
        } else if matches!(token, "-u" | "--user")
            && words.get(index + 1).is_some_and(|next| next.contains(':'))
        {
            // `user:password` after a basic-auth flag.
            output.push(part.to_owned());
            output.push("[redacted]".to_owned());
            index += 1;
        } else if token
            .strip_prefix("--user=")
            .is_some_and(|value| value.contains(':'))
        {
            output.push("--user=[redacted]".to_owned());
        } else {
            output.push(redacted_shapes(part));
        }
        index += 1;
    }
    display_text(&output.join(" "), 200)
}

/// An HTTP authorization scheme whose next word is the credential.
fn auth_scheme(token: &str) -> bool {
    ["bearer", "basic", "digest"]
        .iter()
        .any(|scheme| token.eq_ignore_ascii_case(scheme))
}

/// When `words[start]` opens a PEM private key block, the index of the last
/// word of the block: the end of its END line, or the last word when the
/// block is not closed. Certificates and public keys are left alone.
fn private_key_end(words: &[&str], start: usize) -> Option<usize> {
    let opens = |word: &str, marker: &str| {
        word.find(marker)
            .map(|at| word[at + marker.len()..].contains("-----"))
    };
    opens(words[start], "-----BEGIN")?;
    // The BEGIN line runs to the word that closes it with `-----`.
    let header_end = (start..words.len().min(start + 6)).find(|&at| {
        if at == start {
            opens(words[at], "-----BEGIN") == Some(true)
        } else {
            words[at].contains("-----")
        }
    })?;
    if !words[start..=header_end].join(" ").contains("PRIVATE KEY") {
        return None;
    }
    let last = words.len() - 1;
    let Some(end) = (header_end + 1..words.len()).find(|&at| words[at].contains("-----END")) else {
        return Some(last);
    };
    Some(
        (end..words.len())
            .find(|&at| {
                if at == end {
                    opens(words[at], "-----END") == Some(true)
                } else {
                    words[at].contains("-----")
                }
            })
            .unwrap_or(last),
    )
}

/// One command word with credential-shaped values replaced: the userinfo of a
/// URL, a whole base64 secret key, and token-shaped pieces between
/// punctuation (a query value, a header value, an argument).
fn redacted_shapes(part: &str) -> String {
    let token = part.trim_matches(['\'', '"']);
    if base64_secret(token) {
        return "[redacted]".to_owned();
    }
    let part = redacted_userinfo(part);
    let mut output = String::with_capacity(part.len());
    let mut piece = String::new();
    let flush = |piece: &mut String, output: &mut String| {
        if piece != "[redacted]" && token_shape(piece) {
            output.push_str("[redacted]");
        } else {
            output.push_str(piece);
        }
        piece.clear();
    };
    for ch in part.chars() {
        if matches!(
            ch,
            '=' | ':'
                | '/'
                | '@'
                | ','
                | ';'
                | '&'
                | '?'
                | '#'
                | '\''
                | '"'
                | '('
                | ')'
                | '<'
                | '>'
                | '|'
        ) {
            flush(&mut piece, &mut output);
            output.push(ch);
        } else {
            piece.push(ch);
        }
    }
    flush(&mut piece, &mut output);
    output
}

/// `scheme://user:password@host/...` with the userinfo replaced.
fn redacted_userinfo(part: &str) -> String {
    let Some(scheme_end) = part.find("://") else {
        return part.to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = part[authority_start..]
        .find(['/', '?', '#'])
        .map_or(part.len(), |at| authority_start + at);
    match part[authority_start..authority_end].rfind('@') {
        Some(at) => format!(
            "{}[redacted]{}",
            &part[..authority_start],
            &part[authority_start + at..]
        ),
        None => part.to_owned(),
    }
}

/// A value shaped like an access token: a well-known provider prefix, an AWS
/// access key ID, a JSON Web Token, or a long opaque mix of upper case, lower
/// case and digits. Plain hex, such as a commit hash, is not a token.
fn token_shape(piece: &str) -> bool {
    let token_char = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.');
    const PREFIXES: [&str; 20] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "xoxr-",
        "xapp-",
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "npm_",
        "hf_",
        "AIza",
    ];
    let prefixed = PREFIXES.iter().any(|prefix| {
        piece
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.len() >= 16 && rest.chars().all(token_char))
    });
    let aws_key_id = ["AKIA", "ASIA"].iter().any(|prefix| {
        piece.strip_prefix(prefix).is_some_and(|rest| {
            rest.len() == 16
                && rest
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        })
    });
    let jwt = piece.starts_with("eyJ") && {
        let parts: Vec<&str> = piece.split('.').collect();
        parts.len() == 3
            && parts.iter().all(|part| {
                part.len() >= 8
                    && part
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            })
    };
    let opaque = piece.len() >= 32
        && piece
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        && mixed_case_and_digits(piece);
    prefixed || aws_key_id || jwt || opaque
}

/// A whole word that is a long base64 secret, such as an AWS secret access
/// key, which can contain `/` and `+`. Paths start with `/` or carry `.`,
/// `-` or `_`, so they do not match.
fn base64_secret(token: &str) -> bool {
    token.len() >= 40
        && !token.starts_with('/')
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
        && mixed_case_and_digits(token)
}

fn mixed_case_and_digits(value: &str) -> bool {
    value.chars().any(|ch| ch.is_ascii_uppercase())
        && value.chars().any(|ch| ch.is_ascii_lowercase())
        && value.chars().any(|ch| ch.is_ascii_digit())
}

fn relative_file(value: &str, cwd: Option<&str>) -> Option<String> {
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        cwd.and_then(|cwd| path.strip_prefix(cwd).ok())
            .or_else(|| path.file_name().map(Path::new))?
    } else if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        path.file_name().map(Path::new)?
    } else {
        path
    };
    let shown = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    let shown = display_text(&shown, 256);
    (!shown.is_empty()).then_some(shown)
}

fn ticket_id(input: &Value) -> Option<String> {
    input
        .get("ticket")
        .or_else(|| input.get("task_id"))
        .and_then(Value::as_str)
        .and_then(valid_ticket)
}

/// A board ticket key such as `PT-175`, upper-cased, or nothing.
fn valid_ticket(value: &str) -> Option<String> {
    let (project, sequence) = value.split_once('-')?;
    (!project.is_empty()
        && project.chars().all(|ch| ch.is_ascii_alphanumeric())
        && !sequence.is_empty()
        && sequence.chars().all(|ch| ch.is_ascii_digit())
        && value.len() <= 32)
        .then(|| value.to_ascii_uppercase())
}

/// Where each seat's explicit current-ticket marker lives. It sits beside the
/// control socket, so it follows that instance's runtime directory and a
/// reboot cannot leave a stale ticket behind.
fn marker_path(socket: &Path, seat: &AgentId) -> PathBuf {
    socket.with_extension("tickets").join(seat.as_str())
}

fn read_marker(socket: &Path, seat: &AgentId) -> Option<String> {
    let text = std::fs::read_to_string(marker_path(socket, seat)).ok()?;
    valid_ticket(text.trim())
}

fn write_marker(socket: &Path, seat: &AgentId, ticket: &str) -> Result<()> {
    let path = marker_path(socket, seat);
    let directory = path.parent().context("marker directory")?;
    std::fs::create_dir_all(directory).context("cannot create ticket marker directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let staging = path.with_extension("new");
    std::fs::write(&staging, format!("{ticket}\n")).context("cannot write ticket marker")?;
    std::fs::rename(&staging, &path).context("cannot store ticket marker")
}

/// Attach the seat's deliberately set ticket to an event that names none.
/// A ticket the producer sent in the event itself wins.
fn attach_marker(socket: &Path, event: &mut ActivityInput) {
    let has_ticket = event.detail.as_ref().is_some_and(|d| d.task.is_some());
    if has_ticket {
        return;
    }
    if let Some(ticket) = read_marker(socket, &event.seat) {
        event.detail.get_or_insert_with(Default::default).task = Some(ticket);
    }
}

const TICKET_USAGE: &str = "pmux activity ticket set [--seat ID] TICKET\npmux activity ticket clear [--seat ID] [TICKET]\npmux activity ticket show [--seat ID]";

/// `pmux activity ticket set|clear|show`: the seat's current-ticket marker.
/// `clear TICKET` removes the marker only while it still names that ticket.
pub(super) fn run_activity(socket: &Path, args: Vec<String>) -> Result<()> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("ticket") {
        bail!("usage:\n{TICKET_USAGE}");
    }
    let action = args.next().unwrap_or_default();
    let mut seat = None;
    let mut ticket = None;
    while let Some(arg) = args.next() {
        if arg == "--seat" && seat.is_none() {
            seat = Some(AgentId::new(args.next().context("missing seat")?)?);
        } else if !arg.starts_with('-') && ticket.is_none() {
            ticket = Some(valid_ticket(&arg).context("not a ticket key like PT-175")?);
        } else {
            bail!("usage:\n{TICKET_USAGE}");
        }
    }
    let seat = match seat {
        Some(seat) => seat,
        None => AgentId::new(
            std::env::var("PMUX_AGENT").context("--seat is required (or set PMUX_AGENT)")?,
        )?,
    };
    match (action.as_str(), ticket) {
        ("set", Some(ticket)) => write_marker(socket, &seat, &ticket),
        ("clear", wanted) => {
            if wanted.is_some_and(|wanted| read_marker(socket, &seat) != Some(wanted)) {
                return Ok(());
            }
            match std::fs::remove_file(marker_path(socket, &seat)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    Err(error).context("cannot clear ticket marker")
                }
                _ => Ok(()),
            }
        }
        ("show", None) => {
            println!("{}", read_marker(socket, &seat).unwrap_or_default());
            Ok(())
        }
        _ => bail!("usage:\n{TICKET_USAGE}"),
    }
}

fn hook_detail(input: &Value) -> Option<prismattyc_mux::activity::ActivityDetail> {
    let tool = input
        .get("tool_name")
        .and_then(Value::as_str)
        .map(|value| display_text(value, 64));
    let args = input.get("tool_input");
    let file = ["file_path", "filePath", "path", "target_file"]
        .iter()
        .find_map(|key| args.and_then(|args| args.get(*key)).and_then(Value::as_str))
        .and_then(|value| relative_file(value, input.get("cwd").and_then(Value::as_str)));
    let command = ["command", "cmd"]
        .iter()
        .find_map(|key| args.and_then(|args| args.get(*key)).and_then(Value::as_str))
        .map(redacted_command);
    let exit_code = input
        .pointer("/tool_response/exit_code")
        .or_else(|| input.get("exit_code"))
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    let model = input
        .get("model")
        .and_then(Value::as_str)
        .map(|value| display_text(value, 64));
    let operation_id = input
        .get("tool_use_id")
        .or_else(|| input.get("tool_call_id"))
        .and_then(Value::as_str)
        .map(opaque);
    let task = ticket_id(input);
    let detail = prismattyc_mux::activity::ActivityDetail {
        tool,
        file,
        command,
        duration_ms: None,
        exit_code,
        task,
        operation_id,
        model,
    };
    (detail.tool.is_some()
        || detail.file.is_some()
        || detail.command.is_some()
        || detail.exit_code.is_some()
        || detail.task.is_some()
        || detail.operation_id.is_some()
        || detail.model.is_some())
    .then_some(detail)
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
    let detail = (!notify).then(|| hook_detail(input)).flatten();
    Ok(Some(ActivityInput {
        schema_version: 1,
        seat,
        run_id: opaque(session),
        event_id: opaque(&key),
        source,
        kind,
        process_ids: if source == ActivitySource::Hooks {
            prismattyc_mux::procinfo::activity_hook_owner_pid()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        },
        detail,
    }))
}

/// Run two ordered observation waits on one bound.
///
/// `clock` reports how long the observation has already been running. The
/// second wait receives only the time still left. It does not get `bound`
/// again, so the waits cannot add up to more than `bound`.
fn drive_observation<C, A, B, T, U>(bound: Duration, mut clock: C, first: A, second: B) -> Result<U>
where
    C: FnMut() -> Duration,
    A: FnOnce(Duration) -> Result<T>,
    B: FnOnce(Duration, T) -> Result<U>,
{
    let left = bound.saturating_sub(clock());
    if left.is_zero() {
        bail!("hook input deadline exceeded");
    }
    let value = first(left)?;
    let left = bound.saturating_sub(clock());
    if left.is_zero() {
        bail!("activity request deadline exceeded");
    }
    second(left, value)
}

fn read_limited_input<R>(reader: R, deadline: Instant) -> Result<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("hook input deadline exceeded");
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = reader
            .take((MAX_INPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let bytes = receiver
        .recv_timeout(remaining)
        .context("hook input deadline exceeded")??;
    Ok(bytes)
}

fn read_hook_event<R>(
    reader: R,
    seat: AgentId,
    hook_event: Option<&str>,
    deadline: Instant,
) -> Result<Option<ActivityInput>>
where
    R: Read + Send + 'static,
{
    let bytes = read_limited_input(reader, deadline)?;
    if bytes.len() > MAX_INPUT {
        bail!("activity input exceeds limit");
    }
    let mut input: Value = serde_json::from_slice(&bytes).context("invalid hook input")?;
    if let (Some(name), Some(object)) = (hook_event, input.as_object_mut()) {
        object.insert("hook_event_name".into(), Value::String(name.to_owned()));
    }
    normalize(seat, &input, false)
}

fn publish_activity_event(socket: &Path, event: ActivityInput, deadline: Instant) -> Result<()> {
    match activity::exchange_until(
        socket,
        ControlRequest::PublishActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            event,
        },
        deadline,
    )? {
        ControlResponseData::ActivityPublished { .. } => Ok(()),
        _ => bail!("unexpected activity response"),
    }
}

/// Read hook stdin and publish it, sharing `bound` across both waits.
fn publish_hook<R>(
    reader: R,
    socket: &Path,
    seat: AgentId,
    hook_event: Option<&str>,
    bound: Duration,
) -> Result<()>
where
    R: Read + Send + 'static,
{
    let started = Instant::now();
    drive_observation(
        bound,
        || started.elapsed(),
        |remaining| read_hook_event(reader, seat, hook_event, Instant::now() + remaining),
        |remaining, event| match event {
            Some(mut event) => {
                attach_marker(socket, &mut event);
                publish_activity_event(socket, event, Instant::now() + remaining)
            }
            None => Ok(()),
        },
    )
}

fn publish(cli: &Cli) -> Result<()> {
    let event = match &cli.mode {
        Mode::Hook => {
            return publish_hook(
                io::stdin(),
                &cli.socket,
                cli.seat.clone(),
                cli.hook_event.as_deref(),
                activity::CLIENT_TIMEOUT,
            );
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
    let Some(mut event) = event else {
        return Ok(());
    };
    attach_marker(&cli.socket, &mut event);
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
    fn hook_keeps_only_an_explicit_bounded_model_field() {
        let event = normalize(
            AgentId::new("worker-model").unwrap(),
            &json!({"hook_event_name":"SessionStart", "session_id":"session-model",
                "model":"o4-mini", "prompt":"private prompt"}),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(event.detail.unwrap().model.as_deref(), Some("o4-mini"));
    }

    #[test]
    fn hook_classifies_operation_and_explicit_failure() {
        let event = normalize(
            AgentId::new("worker-b").unwrap(),
            &json!({"hook_event_name":"PostToolUseFailure", "session_id":"session-b",
                "tool_use_id":"call-b", "tool_name":"Bash", "cwd":"/work/project",
                "tool_input":{"command":"curl https://example.test --api-key=private-secret -H 'Authorization: Bearer private-bearer'",
                    "file_path":"/work/project/src/main.rs"},
                "exit_code":1, "ticket":"PT-108", "error":"private-error"}),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(event.kind, ActivityKind::CommandFailed);
        assert_eq!(event.source, ActivitySource::Hooks);
        let detail = event.detail.unwrap();
        assert_eq!(detail.tool.as_deref(), Some("Bash"));
        assert_eq!(detail.file.as_deref(), Some("src/main.rs"));
        assert_eq!(detail.task.as_deref(), Some("PT-108"));
        assert_eq!(detail.exit_code, Some(1));
        assert!(detail
            .command
            .as_deref()
            .is_some_and(|command| command.contains("[redacted]")));
        let serialized = serde_json::to_string(&detail).unwrap();
        for private in [
            "private-secret",
            "private-bearer",
            "/work/project",
            "call-b",
            "private-error",
        ] {
            assert!(!serialized.contains(private));
        }
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

    #[test]
    fn observation_budget_is_shared_and_does_not_restart_for_the_activity_rpc() {
        use std::cell::{Cell, RefCell};
        let bound = Duration::from_millis(2_000);
        let elapsed = Cell::new(Duration::ZERO);
        let granted = RefCell::new(Vec::new());
        let published = drive_observation(
            bound,
            || elapsed.get(),
            |budget| {
                granted.borrow_mut().push(budget);
                elapsed.set(elapsed.get() + Duration::from_millis(1_200));
                Ok("hook-bytes")
            },
            |budget, input| {
                assert_eq!(input, "hook-bytes");
                granted.borrow_mut().push(budget);
                elapsed.set(elapsed.get() + budget);
                Ok("published")
            },
        )
        .unwrap();
        assert_eq!(published, "published");
        assert_eq!(
            granted.into_inner(),
            vec![Duration::from_millis(2_000), Duration::from_millis(800)]
        );
        assert!(
            elapsed.get() <= bound,
            "stdin plus the activity RPC ran for {:?}, past {bound:?}",
            elapsed.get()
        );
    }

    #[test]
    fn observation_does_not_call_the_activity_rpc_after_the_deadline_is_spent() {
        use std::cell::Cell;
        let bound = Duration::from_millis(2_000);
        let elapsed = Cell::new(Duration::from_millis(500));
        let rpc_called = Cell::new(false);
        let error = drive_observation(
            bound,
            || elapsed.get(),
            |budget| {
                assert_eq!(budget, Duration::from_millis(1_500));
                elapsed.set(elapsed.get() + budget);
                Ok(())
            },
            |budget, _| {
                rpc_called.set(true);
                let _ = budget;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "activity request deadline exceeded");
        assert!(!rpc_called.get());
        assert_eq!(elapsed.get(), bound);
    }

    #[test]
    fn spent_observation_does_not_start_either_wait() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let error = drive_observation(
            Duration::from_millis(2_000),
            || Duration::from_millis(2_000),
            |_| {
                calls.set(calls.get() + 1);
                Ok(())
            },
            |_, _| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "hook input deadline exceeded");
        assert_eq!(calls.get(), 0);
    }

    /// Hold a TCP connection open so a read blocks until `release` is dropped.
    fn held_stdin() -> (
        std::net::TcpStream,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, unblock) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let _accepted = listener.accept().unwrap();
            let _ = unblock.recv();
        });
        let stream = std::net::TcpStream::connect(address).unwrap();
        (stream, release, server)
    }

    #[test]
    fn hook_stdin_read_stops_at_its_deadline() {
        let (reader, release, server) = held_stdin();
        let bound = Duration::from_millis(200);
        let started = Instant::now();
        let error = read_limited_input(reader, started + bound).unwrap_err();
        let elapsed = started.elapsed();
        drop(release);
        let _ = server.join();
        assert!(
            error.to_string().contains("deadline"),
            "stdin read failed another way: {error}"
        );
        assert!(
            elapsed < Duration::from_millis(700),
            "stdin read waited {elapsed:?} on a {bound:?} deadline"
        );
        assert!(
            elapsed + Duration::from_millis(80) >= bound,
            "stdin read returned after {elapsed:?}, before {bound:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hook_observation_keeps_stdin_and_the_activity_rpc_inside_one_deadline() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        const BOUND: Duration = Duration::from_millis(800);
        const STDIN_DELAY: Duration = Duration::from_millis(500);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload =
            br#"{"hook_event_name":"SessionStart","session_id":"bound-session"}"#.to_vec();
        let stdin_server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            std::thread::sleep(STDIN_DELAY);
            let _ = sock.write_all(&payload);
        });
        let reader = std::net::TcpStream::connect(address).unwrap();

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::path::PathBuf::from(format!(
            "/tmp/activity-obs-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let rpc = prismattyc_mux::local_socket::UnixListener::bind(&path).unwrap();
        rpc.set_nonblocking(true).unwrap();
        let (release, unblock) = mpsc::channel::<()>();
        let rpc_server = std::thread::spawn(move || {
            let mut stream = loop {
                match rpc.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        match unblock.recv_timeout(Duration::from_millis(20)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        }
                    }
                    Err(_) => return,
                }
            };
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = unblock.recv_timeout(Duration::from_secs(3));
        });

        let started = Instant::now();
        let error = publish_hook(
            reader,
            &path,
            AgentId::new("worker-bound").unwrap(),
            None,
            BOUND,
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        drop(release);
        let _ = stdin_server.join();
        let _ = rpc_server.join();
        let _ = std::fs::remove_file(&path);

        // Separate two-second-style waits would be STDIN_DELAY + BOUND (1.3s).
        assert!(
            elapsed <= BOUND + Duration::from_millis(300),
            "observation waited {elapsed:?}, past one {BOUND:?} deadline: {error}"
        );
        assert!(
            elapsed + Duration::from_millis(150) >= BOUND,
            "observation returned after {elapsed:?}, before the {BOUND:?} deadline: {error}"
        );
    }

    fn marker_socket(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pmux-marker-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("pmux.sock")
    }

    fn hook_for(seat: &str, extra: Value) -> ActivityInput {
        let mut input = json!({"hook_event_name":"PostToolUse", "session_id":"session-m",
            "tool_use_id":"call-m", "tool_name":"Bash"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        normalize(AgentId::new(seat).unwrap(), &input, false)
            .unwrap()
            .unwrap()
    }

    fn task_of(event: &ActivityInput) -> Option<&str> {
        event.detail.as_ref().and_then(|d| d.task.as_deref())
    }

    #[test]
    fn marker_reaches_events_that_name_no_ticket() {
        let socket = marker_socket("attach");
        run_activity(
            &socket,
            ["ticket", "set", "--seat", "worker-m", "pt-175"]
                .map(String::from)
                .to_vec(),
        )
        .unwrap();

        let mut bare = hook_for("worker-m", json!({}));
        assert_eq!(task_of(&bare), None);
        attach_marker(&socket, &mut bare);
        assert_eq!(task_of(&bare), Some("PT-175"));

        // A notify event has no detail at all; the marker still lands.
        let mut turn = ActivityInput {
            detail: None,
            ..hook_for("worker-m", json!({}))
        };
        attach_marker(&socket, &mut turn);
        assert_eq!(task_of(&turn), Some("PT-175"));

        // A producer's own explicit ticket is not overwritten.
        let mut own = hook_for("worker-m", json!({"ticket":"WP-9"}));
        attach_marker(&socket, &mut own);
        assert_eq!(task_of(&own), Some("WP-9"));

        // Another seat's events are untouched.
        let mut other = hook_for("worker-n", json!({}));
        attach_marker(&socket, &mut other);
        assert_eq!(task_of(&other), None);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn marker_clear_is_conditional_and_final() {
        let socket = marker_socket("clear");
        let seat = AgentId::new("worker-c").unwrap();
        let run =
            |args: &[&str]| run_activity(&socket, args.iter().map(|a| (*a).to_owned()).collect());
        run(&["ticket", "set", "--seat", "worker-c", "PT-175"]).unwrap();
        // Clearing a different ticket leaves this seat's marker alone.
        run(&["ticket", "clear", "--seat", "worker-c", "PT-9"]).unwrap();
        assert_eq!(read_marker(&socket, &seat).as_deref(), Some("PT-175"));
        run(&["ticket", "clear", "--seat", "worker-c", "PT-175"]).unwrap();
        assert_eq!(read_marker(&socket, &seat), None);
        // Clearing with nothing set is fine; an unconditional clear removes.
        run(&["ticket", "clear", "--seat", "worker-c"]).unwrap();
        run(&["ticket", "set", "--seat", "worker-c", "PT-1"]).unwrap();
        run(&["ticket", "clear", "--seat", "worker-c"]).unwrap();
        assert_eq!(read_marker(&socket, &seat), None);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn marker_rejects_anything_but_a_ticket_key() {
        let socket = marker_socket("reject");
        for bad in ["fix the build", "PT-", "PT-17x", "../etc", "PT-1/2"] {
            let result = run_activity(
                &socket,
                ["ticket", "set", "--seat", "worker-r", bad]
                    .map(String::from)
                    .to_vec(),
            );
            assert!(result.is_err(), "{bad:?} was accepted");
        }
        assert_eq!(
            read_marker(&socket, &AgentId::new("worker-r").unwrap()),
            None
        );
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    /// Secret-shaped values built at run time, so no literal in this file
    /// looks like a real credential.
    fn shaped(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    #[test]
    fn commands_hide_unlabeled_secrets() {
        let pw = shaped("hunter", "2-private");
        let basic = shaped("YWRtaW46", "aHVudGVyMg==");
        let github = shaped("ghp_", &"Ab1".repeat(12));
        let aws = shaped("AKIA", "Q7ZK3M2XN4PL8RTW");
        let slack = shaped("xoxb-", "1234567890-abcdefABCDEF");
        let jwt = format!(
            "{}.{}.{}",
            shaped("eyJ", "hbGciOiJIUzI1NiJ9"),
            shaped("eyJ", "zdWIiOiIxIn0"),
            "c2lnbmF0dXJlLXZhbHVl"
        );
        let opaque = shaped("Zq8Lm2Np4Rt6Vx9B", "c3Df5Gh7Jk1Mw0Ys");
        let aws_secret = shaped("wJalrXUtnFEMI/K7MDENG/", "bPxRfiCY2EXAMPLEKEY");
        let key = |marker: &str| format!("-----{marker} {} PRIVATE KEY-----", "OPENSSH");
        let cases = [
            (
                format!("git clone https://deploy:{pw}@git.example.test/org/repo.git"),
                "git clone https://[redacted]@git.example.test/org/repo.git".to_owned(),
            ),
            (
                format!("git clone https://{github}@git.example.test/org/repo.git"),
                "git clone https://[redacted]@git.example.test/org/repo.git".to_owned(),
            ),
            (
                format!("curl -u admin:{pw} https://example.test/api"),
                "curl -u [redacted] https://example.test/api".to_owned(),
            ),
            (
                format!("curl --user=admin:{pw} https://example.test"),
                "curl --user=[redacted] https://example.test".to_owned(),
            ),
            (
                format!("curl -H 'Authorization: Basic {basic}' https://example.test"),
                "curl -H Authorization=[redacted] [redacted] [redacted] https://example.test"
                    .to_owned(),
            ),
            (
                format!("http example.test Basic {basic}"),
                "http example.test Basic [redacted]".to_owned(),
            ),
            (
                format!("deploy --registry example.test {github}"),
                "deploy --registry example.test [redacted]".to_owned(),
            ),
            (
                format!("aws configure set aws_access_key_id {aws}"),
                "aws configure set aws_access_key_id [redacted]".to_owned(),
            ),
            (
                format!("configure set signing_value {aws_secret}"),
                "configure set signing_value [redacted]".to_owned(),
            ),
            (
                format!("curl https://hooks.example.test/x?sig={slack}"),
                "curl https://hooks.example.test/x?sig=[redacted]".to_owned(),
            ),
            (
                format!("http example.test X-Session:{jwt}"),
                "http example.test X-Session:[redacted]".to_owned(),
            ),
            (
                format!("deploy --ref {opaque}"),
                "deploy --ref [redacted]".to_owned(),
            ),
            (
                format!(
                    "printf '%s' '{}\nb3BlbnNzaC1rZXktdjEAAAAA\nAAAABG5vbmU=\n{}' > id_test && chmod 600 id_test",
                    key("BEGIN"),
                    key("END")
                ),
                "printf '%s' [redacted private key] > id_test && chmod 600 id_test".to_owned(),
            ),
            (
                format!("cat <<EOF\n{}\nb3BlbnNzaC1rZXktdjEAAAAA", key("BEGIN")),
                "cat <<EOF [redacted private key]".to_owned(),
            ),
        ];
        for (command, expected) in &cases {
            assert_eq!(&redacted_command(command), expected, "{command}");
        }
        for secret in [
            &pw,
            &basic,
            &github,
            &aws,
            &slack,
            &jwt,
            &opaque,
            &aws_secret,
        ] {
            for (command, _) in &cases {
                assert!(
                    !redacted_command(command).contains(secret.as_str()),
                    "{secret} leaked from {command}"
                );
            }
        }
    }

    #[test]
    fn ordinary_commands_are_not_redacted() {
        for command in [
            "git checkout 3bd2b933a1f0c4e5d6b7a8c9d0e1f2a3b4c5d6e7",
            "cargo test -p prismattyc-mux --test pane_log_restart -- --nocapture",
            "python -u scripts/build_release_notes.py --out dist/notes.md",
            "sort -u names.txt",
            "curl -fsSL https://example.test/install.sh",
            "git log --format=%H origin/main..HEAD",
            "rm cache/123e4567-e89b-12d3-a456-426614174000.json",
            "ssh deploy@build.example.test uptime",
            "git clone git@git.example.test:org/repo.git",
            "npm install @scope/package@1.2.3 sk-learn",
            "rename ExampleCamelCaseIdentifierNameWithoutDigits",
            "ls crates/prismattyc-mux/src/bin/pmux/activity_emit.rs",
            "echo -----BEGIN CERTIFICATE----- MIIBszCCAVmgAwIBAgIU -----END CERTIFICATE-----",
        ] {
            assert_eq!(redacted_command(command), command);
        }
    }
}
