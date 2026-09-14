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

use std::collections::{HashSet, VecDeque};
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

fn children_of(pid: u32) -> Vec<u32> {
    crate::procinfo::children_of(pid)
}

/// True if `target` is `root` or a descendant.
#[must_use]
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

/// Walk `bound_pid` then the pane root and their descendants.
#[must_use]
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
