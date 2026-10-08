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
//! - Muse: use Kitty Enter when the pane has Kitty keyboard flags, else CR.
//!   A Muse 1.4.3 probe (2026-10-08) found flags `3`, modifyOtherKeys `0`;
//!   raw CR and bare `CSI 13u` left `PMUX_MAIL` in the composer, while a
//!   plain-text write followed by event-typed Enter (`CSI 13;1u`) could still
//!   leave it in the composer. Bracketed paste followed by that Enter submits.

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
    Muse,
    Unknown,
}

/// Lowercase id for logs and attention copy. `None` for a plain shell.
#[must_use]
pub fn inject_agent_slug(agent: InjectAgent) -> Option<&'static str> {
    match agent {
        InjectAgent::Claude => Some("claude"),
        InjectAgent::Grok => Some("grok"),
        InjectAgent::Cursor => Some("cursor"),
        InjectAgent::Codex => Some("codex"),
        InjectAgent::Kiro => Some("kiro"),
        InjectAgent::Muse => Some("muse"),
        InjectAgent::Unknown => None,
    }
}

/// Shared file-name matcher for unix cmdlines and Windows argv.
///
/// `name` is a single path basename. Matching is case-insensitive.
/// `muse` and `muse-*` (including `muse-bin*`) are Muse. `museum` is not.
fn classify_agent_file_name(name: &str) -> Option<InjectAgent> {
    let name = name.to_ascii_lowercase();
    if name == "cursor-agent" || name.starts_with("cursor-agent-") || name.contains("cursor-agent")
    {
        return Some(InjectAgent::Cursor);
    }
    if agent_stem(&name, "codex") {
        return Some(InjectAgent::Codex);
    }
    if agent_stem(&name, "claude") {
        return Some(InjectAgent::Claude);
    }
    if agent_stem(&name, "grok") {
        return Some(InjectAgent::Grok);
    }
    if agent_stem(&name, "kiro") {
        return Some(InjectAgent::Kiro);
    }
    if agent_stem(&name, "muse") {
        return Some(InjectAgent::Muse);
    }
    None
}

/// Exact `stem`, or `stem-*` (`muse-bin-1.4.3` is `muse-*`).
fn agent_stem(name: &str, stem: &str) -> bool {
    name == stem
        || name
            .strip_prefix(stem)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Kitty keyboard protocol: Ctrl+Enter.
pub const CURSOR_SUBMIT: &[u8] = b"\x1b[13;5u";

/// Unmodified Enter in the Kitty keyboard protocol, with its modifier field explicit.
pub(crate) const KITTY_ENTER_SUBMIT: &[u8] = b"\x1b[13;1u";

/// Submit bytes for one detected guest. Muse depends on the live Kitty mode;
/// other agents keep their established submit sequences.
#[must_use]
pub(crate) fn submit_writes(agent: InjectAgent, kitty_flags: u16) -> Vec<Vec<u8>> {
    match agent {
        InjectAgent::Cursor => vec![CURSOR_SUBMIT.to_vec()],
        InjectAgent::Codex => vec![vec![b'\r'], vec![b'\r']],
        InjectAgent::Muse if kitty_flags != 0 => vec![KITTY_ENTER_SUBMIT.to_vec()],
        InjectAgent::Claude
        | InjectAgent::Grok
        | InjectAgent::Kiro
        | InjectAgent::Muse
        | InjectAgent::Unknown => vec![vec![b'\r']],
    }
}

/// Classify a `/proc` cmdline (NUL or space separated).
#[must_use]
#[cfg(not(windows))]
pub fn classify_cmdline(cmd: &str) -> Option<InjectAgent> {
    let normalized = cmd.replace('\0', " ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let names: Vec<String> = tokens
        .iter()
        .map(|token| {
            Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(token)
                .to_ascii_lowercase()
        })
        .collect();
    // Family order matches the previous classifiers: cursor wins over a
    // later token that names another agent.
    const ORDER: [InjectAgent; 6] = [
        InjectAgent::Cursor,
        InjectAgent::Codex,
        InjectAgent::Claude,
        InjectAgent::Grok,
        InjectAgent::Kiro,
        InjectAgent::Muse,
    ];
    ORDER.into_iter().find(|&kind| {
        names
            .iter()
            .any(|name| classify_agent_file_name(name) == Some(kind))
    })
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
            && std::path::Path::new(script).parent().is_some_and(|parent| {
                crate::procinfo::executable_name(&parent.to_string_lossy()) == "claude-code"
                    && parent.parent().is_some_and(|scope| {
                        crate::procinfo::executable_name(&scope.to_string_lossy())
                            == "@anthropic-ai"
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
    classify_agent_file_name(name)
}

/// Writes to send, in order. Codex needs two writes (text+CR, then CR).
/// The text is always [`PMUX_MAIL_NOTIFICATION`] — never free text.
#[must_use]
pub fn inject_writes(agent: InjectAgent) -> Vec<Vec<u8>> {
    inject_writes_for_mode(agent, 0)
}

pub(crate) fn inject_writes_for_mode(agent: InjectAgent, kitty_flags: u16) -> Vec<Vec<u8>> {
    let text = PMUX_MAIL_NOTIFICATION.as_bytes();
    let submit = submit_writes(agent, kitty_flags);
    match agent {
        InjectAgent::Cursor => vec![text.to_vec(), submit[0].clone()],
        InjectAgent::Codex => {
            let mut first = text.to_vec();
            first.extend_from_slice(&submit[0]);
            vec![first, submit[1].clone()]
        }
        InjectAgent::Muse if kitty_flags != 0 => {
            // Muse submits this token reliably when the paste terminator
            // establishes an input boundary before the Kitty Enter event.
            let mut pasted = Vec::with_capacity(text.len() + 12);
            pasted.extend_from_slice(b"\x1b[200~");
            pasted.extend_from_slice(text);
            pasted.extend_from_slice(b"\x1b[201~");
            vec![pasted, submit[0].clone()]
        }
        InjectAgent::Claude
        | InjectAgent::Grok
        | InjectAgent::Kiro
        | InjectAgent::Muse
        | InjectAgent::Unknown => {
            let mut one = text.to_vec();
            one.extend_from_slice(&submit[0]);
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
    fn classify_muse_binary_and_not_museum() {
        assert_eq!(classify_cmdline("muse"), Some(InjectAgent::Muse));
        assert_eq!(classify_cmdline("MUSE"), Some(InjectAgent::Muse));
        assert_eq!(
            classify_cmdline("/Users/x/.local/bin/muse-bin-1.4.3-R5018.1 --yolo"),
            Some(InjectAgent::Muse)
        );
        assert_eq!(
            classify_cmdline("/Users/x/.local/bin/muse-bin-1.4.3-R5018.1\0--model\0spark\0--yolo"),
            Some(InjectAgent::Muse)
        );
        assert_eq!(classify_cmdline("muse-bin"), Some(InjectAgent::Muse));
        assert_eq!(classify_cmdline("museum"), None);
        assert_eq!(classify_cmdline("musebin"), None);
        assert_eq!(classify_cmdline("/opt/museum/bin/bash --yolo"), None);
        assert_eq!(inject_agent_slug(InjectAgent::Muse), Some("muse"));
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
    fn legacy_agents_and_muse_without_kitty_mode_use_one_cr() {
        for agent in [
            InjectAgent::Claude,
            InjectAgent::Grok,
            InjectAgent::Kiro,
            InjectAgent::Muse,
            InjectAgent::Unknown,
        ] {
            let w = inject_writes(agent);
            assert_eq!(w, vec![b"PMUX_MAIL\r".to_vec()], "{agent:?}");
        }
    }

    #[test]
    fn muse_submit_uses_live_kitty_keyboard_mode() {
        use prismattyc_emulator::{KITTY_DISAMBIGUATE, KITTY_EVENT_TYPES};

        assert_eq!(submit_writes(InjectAgent::Muse, 0), vec![b"\r".to_vec()]);
        let flags = KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES;
        assert_eq!(
            submit_writes(InjectAgent::Muse, flags),
            vec![b"\x1b[13;1u".to_vec()]
        );
        assert_eq!(
            inject_writes_for_mode(InjectAgent::Muse, flags),
            vec![
                b"\x1b[200~PMUX_MAIL\x1b[201~".to_vec(),
                b"\x1b[13;1u".to_vec()
            ]
        );
        assert_eq!(
            submit_writes(InjectAgent::Grok, flags),
            vec![b"\r".to_vec()],
            "guest keyboard mode must only change Muse submit"
        );
    }
}
