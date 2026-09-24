//! Per-CLI submit bytes for `InjectMail`.
//!
//! The doorbell text is the fixed token `PMUX_MAIL`. The terminator
//! depends on the guest TUI
//! (live probes, 2026-08-15):
//! - Grok: one CR submits (Enter). Alt+Enter is newline.
//! - Cursor: CR is a newline; kitty Ctrl+Enter (`CSI 13;5u`) submits.
//! - Codex: first CR inserts; a second CR submits.
//! - Claude: assumed one CR (not probed this session).
//! - Kiro: assumed one CR (not probed).

#[cfg(not(windows))]
use std::collections::{HashSet, VecDeque};
#[cfg(not(windows))]
use std::path::Path;

use crate::PMUX_MAIL_NOTIFICATION;

/// Guest agent family for inject submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectAgent {
    Claude,
    Grok,
    Cursor,
    Codex,
    Kiro,
    Unknown,
}

/// Kitty keyboard protocol: Ctrl+Enter.
pub const CURSOR_SUBMIT: &[u8] = b"\x1b[13;5u";

/// Classify a `/proc` cmdline (NUL or space separated).
#[must_use]
#[cfg(not(windows))]
pub fn classify_cmdline(cmd: &str) -> Option<InjectAgent> {
    let normalized = cmd.replace('\0', " ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let names: Vec<String> = tokens
        .iter()
        .map(|t| {
            Path::new(t)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(t)
                .to_ascii_lowercase()
        })
        .collect();
    let joined = names.join(" ");
    if names
        .iter()
        .any(|n| n == "cursor-agent" || n.contains("cursor-agent"))
        || joined.contains("cursor-agent")
    {
        return Some(InjectAgent::Cursor);
    }
    if names
        .iter()
        .any(|n| n == "codex" || n.starts_with("codex-"))
    {
        return Some(InjectAgent::Codex);
    }
    if names
        .iter()
        .any(|n| n == "claude" || n.starts_with("claude-"))
    {
        return Some(InjectAgent::Claude);
    }
    if names.iter().any(|n| n == "grok" || n.starts_with("grok-")) {
        return Some(InjectAgent::Grok);
    }
    if names.iter().any(|n| n == "kiro" || n.starts_with("kiro-")) {
        return Some(InjectAgent::Kiro);
    }
    None
}

#[cfg(windows)]
pub fn classify_cmdline(cmd: &str) -> Option<InjectAgent> {
    if cmd.contains('\0') {
        let args = cmd
            .trim_end_matches('\0')
            .split('\0')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        return classify_windows_argv(&args);
    }
    if cmd.is_empty() {
        return None;
    }
    let wide = cmd.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut count = 0;
    unsafe {
        let argv = windows_sys::Win32::UI::Shell::CommandLineToArgvW(wide.as_ptr(), &mut count);
        if argv.is_null() {
            return None;
        }
        let args = std::slice::from_raw_parts(argv, count as usize)
            .iter()
            .map(|&arg| {
                let mut len = 0;
                while *arg.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16(std::slice::from_raw_parts(arg, len)).ok()
            })
            .collect::<Option<Vec<_>>>();
        windows_sys::Win32::Foundation::LocalFree(argv.cast());
        classify_windows_argv(&args?)
    }
}

#[cfg(windows)]
pub(crate) fn classify_windows_argv(args: &[String]) -> Option<InjectAgent> {
    let mut name = crate::procinfo::executable_name(args.first()?);
    if matches!(name.as_str(), "node" | "nodejs" | "bun" | "deno") {
        let script = args.get(1)?;
        if script.starts_with('-') {
            return None;
        }
        name = crate::procinfo::executable_name(script);
        if name == "cli.js"
            && std::path::Path::new(script)
                .parent()
                .is_some_and(|parent| {
                    crate::procinfo::executable_name(&parent.to_string_lossy()) == "claude-code"
                        && parent.parent().is_some_and(|scope| {
                            crate::procinfo::executable_name(&scope.to_string_lossy()) == "@anthropic-ai"
                        })
                })
        {
            return Some(InjectAgent::Claude);
        }
        name = name
            .strip_suffix(".js")
            .or_else(|| name.strip_suffix(".cjs"))
            .or_else(|| name.strip_suffix(".mjs"))?
            .to_owned();
    }
    let name = name
        .strip_suffix(".cmd")
        .or_else(|| name.strip_suffix(".bat"))
        .unwrap_or(&name);
    if name == "cursor-agent" || name.starts_with("cursor-agent-") {
        Some(InjectAgent::Cursor)
    } else if name == "codex" || name.starts_with("codex-") {
        Some(InjectAgent::Codex)
    } else if name == "claude" || name.starts_with("claude-") {
        Some(InjectAgent::Claude)
    } else if name == "grok" || name.starts_with("grok-") {
        Some(InjectAgent::Grok)
    } else if name == "kiro" || name.starts_with("kiro-") {
        Some(InjectAgent::Kiro)
    } else {
        None
    }
}

/// Writes to send, in order. Codex needs two writes (text+CR, then CR).
/// The text is always [`PMUX_MAIL_NOTIFICATION`] — never free text.
#[must_use]
pub fn inject_writes(agent: InjectAgent) -> Vec<Vec<u8>> {
    let text = PMUX_MAIL_NOTIFICATION.as_bytes();
    match agent {
        InjectAgent::Cursor => vec![text.to_vec(), CURSOR_SUBMIT.to_vec()],
        InjectAgent::Codex => {
            let mut first = text.to_vec();
            first.push(b'\r');
            vec![first, vec![b'\r']]
        }
        InjectAgent::Claude | InjectAgent::Grok | InjectAgent::Kiro | InjectAgent::Unknown => {
            let mut one = text.to_vec();
            one.push(b'\r');
            vec![one]
        }
    }
}

#[cfg(not(windows))]
fn read_cmdline(pid: u32) -> Option<String> {
    let args = crate::procinfo::cmdline(pid)?;
    if args.is_empty() {
        return None;
    }
    let mut raw = Vec::new();
    for arg in args {
        raw.extend_from_slice(&arg);
        raw.push(0);
    }
    Some(String::from_utf8_lossy(&raw).into_owned())
}

#[cfg(not(windows))]
fn children_of(pid: u32) -> Vec<u32> {
    crate::procinfo::children_of(pid)
}

/// True if `target` is `root` or a descendant.
#[must_use]
#[cfg(not(windows))]
pub fn pid_in_tree(root: u32, target: u32) -> bool {
    if root == target {
        return true;
    }
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([root]);
    while let Some(pid) = q.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if pid == target {
            return true;
        }
        q.extend(children_of(pid));
    }
    false
}

#[cfg(windows)]
pub fn pid_in_tree(root: u32, target: u32) -> bool {
    crate::procinfo::windows_pid_in_tree(root, target)
}

/// Walk `bound_pid` then the pane root and their descendants.
#[must_use]
#[cfg(not(windows))]
pub fn detect_inject_agent(bound_pid: Option<u32>, root_pid: Option<u32>) -> InjectAgent {
    let mut seen = HashSet::new();
    let mut q = VecDeque::new();
    for pid in [bound_pid, root_pid].into_iter().flatten() {
        q.push_back(pid);
    }
    while let Some(pid) = q.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(cmd) = read_cmdline(pid) {
            if let Some(kind) = classify_cmdline(&cmd) {
                return kind;
            }
        }
        q.extend(children_of(pid));
    }
    InjectAgent::Unknown
}

#[cfg(windows)]
pub fn detect_inject_agent(bound_pid: Option<u32>, root_pid: Option<u32>) -> InjectAgent {
    root_pid
        .or(bound_pid)
        .map(crate::procinfo::foreground_agent)
        .unwrap_or(InjectAgent::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_cursor_agent() {
        assert_eq!(
            classify_cmdline("/home/brandan/.local/bin/cursor-agent --resume"),
            Some(InjectAgent::Cursor)
        );
    }

    #[test]
    fn classify_codex() {
        assert_eq!(
            classify_cmdline("codex\0--inside"),
            Some(InjectAgent::Codex)
        );
        assert_eq!(
            classify_cmdline("/home/brandan/.codex/packages/standalone/releases/0.147.0-x86_64-unknown-linux-musl/bin/codex-code-mode-host"),
            Some(InjectAgent::Codex)
        );
    }

    #[test]
    fn classify_claude_and_grok() {
        assert_eq!(
            classify_cmdline("/usr/bin/claude"),
            Some(InjectAgent::Claude)
        );
        assert_eq!(classify_cmdline("grok\0--cwd"), Some(InjectAgent::Grok));
    }

    #[test]
    fn classify_kiro() {
        assert_eq!(
            classify_cmdline("/usr/local/bin/kiro-cli\0chat"),
            Some(InjectAgent::Kiro)
        );
        assert_eq!(classify_cmdline("kiro"), Some(InjectAgent::Kiro));
    }

    #[test]
    fn classify_bash_is_unknown() {
        assert_eq!(classify_cmdline("/usr/bin/bash\0-l"), None);
    }

    #[test]
    fn cursor_writes_text_then_ctrl_enter() {
        let w = inject_writes(InjectAgent::Cursor);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], PMUX_MAIL_NOTIFICATION.as_bytes());
        assert_eq!(w[1], CURSOR_SUBMIT);
        assert!(!w[0].ends_with(b"\r"));
    }

    #[test]
    fn codex_writes_text_cr_then_cr() {
        let w = inject_writes(InjectAgent::Codex);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], b"PMUX_MAIL\r");
        assert_eq!(w[1], b"\r");
    }

    #[test]
    fn claude_grok_kiro_unknown_are_one_cr() {
        for agent in [
            InjectAgent::Claude,
            InjectAgent::Grok,
            InjectAgent::Kiro,
            InjectAgent::Unknown,
        ] {
            let w = inject_writes(agent);
            assert_eq!(w, vec![b"PMUX_MAIL\r".to_vec()], "{agent:?}");
        }
    }
}
