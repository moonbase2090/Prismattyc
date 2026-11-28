// SPDX-License-Identifier: MIT
//! OSC 7577 program resume.
//!
//! A program in a pane can tell the pmux server how to start it again after a
//! restart. The server gives each local pane's first process a random token in
//! `OSC7577_TOKEN`. The program writes `ESC ] 7577 ; body ST` where the body is
//! `key=value` pairs joined by `:` and ends with `:mac=` and the lowercase hex
//! SHA-256 of the body before that suffix, a `:` and the token. Output from
//! anything without the token (a `cat` of a file, a remote host) cannot make a
//! record.
//!
//! This module checks reports and keeps one record per pane. Resuming still
//! starts a new process; nothing is handed over from the old one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Environment variable that carries a pane's token.
pub(crate) const TOKEN_ENV: &str = "OSC7577_TOKEN";
/// A report whose time is further than this from the server clock is ignored.
pub(crate) const TIME_WINDOW_SECS: u64 = 60;
/// An owner that exited this recently still counts as running, so a program
/// that quit a moment before the server stopped is still resumed.
pub(crate) const OWNER_EXIT_GRACE_SECS: u64 = 2;
/// A resumed program has to stay up this long before its record may be
/// resumed again. A resume that crashes or is cut short is not retried.
pub(crate) const RESUME_SETTLE_SECS: u64 = 10;
/// Per-pane memory of the last accepted time for each reporting pid.
const MAX_TRACKED_PIDS: usize = 64;
/// Keys this server implements, for `need`.
const IMPLEMENTED_KEYS: &[&str] = &["pid", "time", "mac", "argv", "cwd", "clear", "need"];
/// Programs resumed without asking when `[mux] resume_trusted` is unset:
/// editors and Git interfaces, which reopen files or a repository. Some run
/// commands given as arguments (`nvim -c`, `emacs --eval`), so trusting
/// them replays what their report carried; only a same-user process in the
/// pane that is really running the program can make such a record.
pub(crate) const DEFAULT_TRUSTED: &[&str] = &[
    "nvim", "vim", "hx", "emacs", "kak", "micro", "lazygit", "tig",
];

/// A fresh random token: 32 bytes as 43 characters of `[A-Za-z0-9_-]`.
pub(crate) fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("program resume token: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Remove a token this process inherited from the terminal it runs in, so no
/// child sees it. Call before starting threads.
pub fn forget_inherited_token() {
    std::env::remove_var(TOKEN_ENV);
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The server's policy, read from the config file once per process.
pub(crate) fn policy() -> &'static ResumePolicy {
    static POLICY: std::sync::OnceLock<ResumePolicy> = std::sync::OnceLock::new();
    POLICY.get_or_init(ResumePolicy::load)
}

/// Whether a pane running `program` reaches another machine, so it gets no
/// token: ssh, mosh and rsh.
pub(crate) fn reaches_another_machine(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    matches!(
        name.strip_suffix(".exe").unwrap_or(&name),
        "ssh" | "mosh" | "rsh"
    )
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_PMUX: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The `pmux` installed next to this server.
pub(crate) fn pmux_executable() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_PMUX.with(|path| path.borrow().clone()) {
        return Some(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| crate::executable::sibling(&exe, &["pmux"]))
}

/// The fixed text typed into a restored pane's shell to start its resume:
/// the quoted path of `pmux`, `resume-run` and the one-time id. Nothing from
/// the record is in it. None when the path cannot be quoted for a shell.
pub(crate) fn handoff_command(pmux: &Path, id: &str) -> Option<String> {
    let path = pmux.to_str()?;
    if path.contains(['\'', '\n', '\r'])
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(format!("'{path}' resume-run {id}"))
}

/// The process that made a record. A different start time on the same pid
/// is a different process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeOwner {
    pub pid: u32,
    pub start: u64,
}

/// What the pmux server starts when it restores the pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRecord {
    /// The command. Element 0 is a bare command name or an absolute path.
    pub argv: Vec<String>,
    /// Absolute directory to start in, when the program reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The report's time, in seconds since the Unix epoch.
    pub reported_at: u64,
    /// The process that reported. None after a restore, until the resumed
    /// program starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ResumeOwner>,
    /// User that ran the owner when the report was accepted.
    pub uid: u32,
    /// The executable the owner was last seen running. A report only names a
    /// command; this is what binds it to a real program. Updated each time the
    /// owner is checked, so a wrapper that reports and then runs its target
    /// is bound once the target runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_exe: Option<PathBuf>,
    /// Why the last restore did not resume this record, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_resumed: Option<String>,
    /// Set when a resume starts, cleared once the program has stayed up for
    /// [`RESUME_SETTLE_SECS`]. A set flag at the next restore means the last
    /// resume failed or crashed, so it is not run again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub attempted: bool,
    #[serde(skip)]
    exited_at: Option<u64>,
    #[serde(skip)]
    started_at: Option<u64>,
}

impl ResumeRecord {
    /// The command for display: each argument quoted with control and
    /// format characters escaped, so it cannot hide what it runs.
    pub fn display(&self) -> String {
        self.argv
            .iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What a report body did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not provably from a token holder, or stale: change nothing.
    Ignored(&'static str),
    /// From a token holder but refused. The owner's record is removed.
    Rejected {
        pid: Option<u32>,
        reason: &'static str,
    },
    Clear {
        pid: u32,
        time: u64,
    },
    Set {
        pid: u32,
        time: u64,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

/// Check one report body against `token`. The MAC is checked before anything
/// else is read. `last_time` gives the last accepted time for a pid.
pub(crate) fn check(
    body: &[u8],
    token: &str,
    now: u64,
    last_time: impl Fn(u32) -> Option<u64>,
) -> Verdict {
    let Some(message) = message_with_valid_mac(body, token) else {
        return Verdict::Ignored("missing or wrong mac");
    };
    // A pair without `=` keeps an empty key, so the strict pass rejects it.
    let pairs: Vec<(&[u8], &[u8])> = message
        .split(|byte| *byte == b':')
        .map(|pair| match pair.iter().position(|byte| *byte == b'=') {
            Some(at) => (&pair[..at], &pair[at + 1..]),
            None => (&[][..], pair),
        })
        .collect();
    let first = |key: &[u8]| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let Some(time) = first(b"time").and_then(decimal::<u64>) else {
        return Verdict::Ignored("no usable time");
    };
    if now.abs_diff(time) > TIME_WINDOW_SECS {
        return Verdict::Ignored("time outside the window");
    }
    let pid = first(b"pid").and_then(decimal::<u32>);
    if let Some(pid) = pid {
        if last_time(pid).is_some_and(|last| time < last) {
            return Verdict::Ignored("older than the last accepted report");
        }
    }
    let reject = |reason| Verdict::Rejected { pid, reason };
    let mut seen = BTreeMap::new();
    for (key, value) in &pairs {
        if key.is_empty() || !key.iter().all(u8::is_ascii_lowercase) {
            return reject("malformed key");
        }
        if !value.iter().all(|byte| value_byte(*byte)) {
            return reject("malformed value");
        }
        if seen.insert(*key, *value).is_some() {
            return reject("repeated key");
        }
    }
    let Some(pid) = pid else {
        return reject("no usable pid");
    };
    if let Some(need) = seen.get(&b"need"[..]) {
        for key in need.split(|byte| *byte == b',') {
            if !IMPLEMENTED_KEYS.iter().any(|known| known.as_bytes() == key) {
                return reject("needs a key this server does not implement");
            }
        }
    }
    let clear = seen.get(&b"clear"[..]);
    let argv = seen.get(&b"argv"[..]);
    match (argv, clear) {
        (Some(_), Some(_)) => reject("both argv and clear"),
        (None, None) => reject("neither argv nor clear"),
        (None, Some(value)) if *value == b"1" => Verdict::Clear { pid, time },
        (None, Some(_)) => reject("clear must be 1"),
        (Some(argv), None) => {
            let Some(argv) = decode_argv(argv) else {
                return reject("argv is not a valid command");
            };
            let cwd = match seen.get(&b"cwd"[..]) {
                None => None,
                Some(value) => match decode_text(value).map(PathBuf::from) {
                    Some(path) if path.is_absolute() => Some(path),
                    _ => return reject("cwd is not an absolute path"),
                },
            };
            Verdict::Set {
                pid,
                time,
                argv,
                cwd,
            }
        }
    }
}

/// The body before `:mac=` when the MAC is last, well formed and correct.
fn message_with_valid_mac<'a>(body: &'a [u8], token: &str) -> Option<&'a [u8]> {
    const SUFFIX: &[u8] = b":mac=";
    let split = body.len().checked_sub(SUFFIX.len() + 64)?;
    let (message, tail) = body.split_at(split);
    let mac = tail.strip_prefix(SUFFIX)?;
    if !mac
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let expected = mac_hex(message, token);
    constant_time_eq(expected.as_bytes(), mac).then_some(message)
}

/// Lowercase hex SHA-256 of `message`, `:` and `token`.
pub(crate) fn mac_hex(message: &[u8], token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(message);
    hasher.update(b":");
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// `[1-9][0-9]*` that fits the type.
fn decimal<T: std::str::FromStr>(value: &[u8]) -> Option<T> {
    if value.first().is_none_or(|first| *first == b'0') || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn decode_argv(value: &[u8]) -> Option<Vec<String>> {
    if value.is_empty() {
        return None;
    }
    let argv = value
        .split(|byte| *byte == b',')
        .map(decode_text)
        .collect::<Option<Vec<_>>>()?;
    let program = argv.first()?;
    let bare = !program.is_empty() && !program.starts_with('-') && !program.contains('/');
    (bare || Path::new(program).is_absolute()).then_some(argv)
}

/// Standard base64 (padding absent, or present and correct) of UTF-8 with no
/// control characters.
fn decode_text(value: &[u8]) -> Option<String> {
    use base64::engine::{general_purpose, DecodePaddingMode, GeneralPurpose};
    let padded = value.last() == Some(&b'=');
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        general_purpose::GeneralPurposeConfig::new().with_decode_padding_mode(if padded {
            DecodePaddingMode::RequireCanonical
        } else {
            DecodePaddingMode::RequireNone
        }),
    );
    let text = String::from_utf8(engine.decode(value).ok()?).ok()?;
    (!text
        .chars()
        .any(|ch| matches!(ch, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')))
    .then_some(text)
}

/// How the server looks up processes; a trait so tests can supply their own.
pub(crate) trait OwnerLookup {
    fn identity(&self, pid: u32) -> Option<crate::procinfo::ProcessIdentity>;
    /// The canonical path of the executable `pid` is running.
    fn executable(&self, pid: u32) -> Option<PathBuf>;
    /// Whether `pid` is the pane's first process or one of its descendants.
    fn in_pane(&self, pid: u32) -> bool;
    /// The user the pmux server runs as.
    fn server_uid(&self) -> Option<u32>;
    /// Whether the pane's processes may speak for it: see [`pane_authority`].
    fn pane_authority(&self) -> Result<(), &'static str>;
}

/// The live system, for a pane whose first process is `root`.
pub(crate) struct SystemLookup {
    pub root: Option<u32>,
}

impl OwnerLookup for SystemLookup {
    fn identity(&self, pid: u32) -> Option<crate::procinfo::ProcessIdentity> {
        crate::procinfo::process_identity(pid)
    }

    fn executable(&self, pid: u32) -> Option<PathBuf> {
        crate::procinfo::process_executable(pid)
    }

    fn in_pane(&self, pid: u32) -> bool {
        self.root
            .is_some_and(|root| crate::procinfo::is_process_descendant(root, pid))
    }

    fn server_uid(&self) -> Option<u32> {
        server_uid()
    }

    fn pane_authority(&self) -> Result<(), &'static str> {
        let server = crate::procinfo::process_facts(std::process::id())
            .ok()
            .flatten()
            .ok_or(UNCHECKED);
        pane_authority(
            self.root,
            crate::procinfo::process_tree_strict,
            crate::procinfo::process_facts,
            server,
        )
    }
}

/// Reports are ignored while another user's process is in the pane.
pub(crate) const OTHER_USER: &str = "a process of another user is in the pane";
/// Reports are ignored while a sandboxed process is in the pane.
pub(crate) const SANDBOXED: &str = "a sandboxed process is in the pane";
/// Reports are ignored while a process in the pane has another root
/// directory or namespace than the pmux server.
pub(crate) const OTHER_ROOT: &str =
    "a process with another root directory or namespace is in the pane";
/// Reports are ignored when the pane's processes cannot all be checked.
pub(crate) const UNCHECKED: &str = "the pane's processes could not be checked";

/// Whether a report from this pane may be trusted at all.
///
/// The protocol trusts every same-user holder of the token, and a launcher
/// should remove the token from children it restricts. When one does not,
/// a confined child (another user, another root or namespace, a sandbox)
/// still holds the token. So while any such process is in the pane — its
/// first process or any descendant — reports are ignored. Anything that
/// cannot be checked counts against the pane.
pub(crate) fn pane_authority(
    root: Option<u32>,
    tree: impl Fn(u32) -> Option<Vec<u32>>,
    facts: impl Fn(u32) -> Result<Option<crate::procinfo::ProcessFacts>, ()>,
    server: Result<crate::procinfo::ProcessFacts, &'static str>,
) -> Result<(), &'static str> {
    let server = server?;
    let root = root.ok_or(UNCHECKED)?;
    let first = facts(root).ok().flatten().ok_or(UNCHECKED)?;
    for pid in tree(root).ok_or(UNCHECKED)? {
        let process = match facts(pid) {
            Ok(Some(process)) => process,
            // Exited since the tree was listed: no longer in the pane.
            Ok(None) => continue,
            Err(()) => return Err(UNCHECKED),
        };
        if process.uid != first.uid {
            return Err(OTHER_USER);
        }
        if process.sandboxed {
            return Err(SANDBOXED);
        }
        if process.root_dir != server.root_dir || process.namespaces != server.namespaces {
            return Err(OTHER_ROOT);
        }
    }
    Ok(())
}

/// The user the pmux server runs as, on Unix.
pub(crate) fn server_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(rustix::process::geteuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Why `pid` may not own a record in this pane: it must be running, as the
/// server's user, inside the pane.
fn owner_problem(pid: u32, lookup: &dyn OwnerLookup) -> Option<&'static str> {
    let Some(identity) = lookup.identity(pid) else {
        return Some("owner is not running");
    };
    if lookup.server_uid() != Some(identity.uid) {
        return Some("owner runs as another user");
    }
    if !lookup.in_pane(pid) {
        return Some("owner is not in this pane");
    }
    None
}

/// What a received report did to the pane, for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ignored(&'static str),
    /// The report could be checked, but the pane holds a process that may
    /// not speak for it; `first` is set the first time this pane says so.
    PaneNotTrusted {
        reason: &'static str,
        first: bool,
    },
    Rejected(&'static str),
    Recorded,
    Cleared,
}

/// A pane's token and record.
#[derive(Debug, Default)]
pub(crate) struct PaneResume {
    token: Option<String>,
    record: Option<ResumeRecord>,
    last_time: BTreeMap<u32, u64>,
    /// Whether this pane has already logged that it is not trusted.
    distrust_noted: bool,
}

impl PaneResume {
    pub(crate) fn new(token: Option<String>, record: Option<ResumeRecord>) -> Self {
        Self {
            token,
            record,
            last_time: BTreeMap::new(),
            distrust_noted: false,
        }
    }

    pub(crate) fn record(&self) -> Option<&ResumeRecord> {
        self.record.as_ref()
    }

    #[cfg(any(unix, windows))]
    /// Token, record and per-process report times, to hand to the next pmux
    /// server in a live update. The program keeps running, so its record keeps
    /// its owner, and the report times keep refusing a replayed report.
    pub(crate) fn handoff_parts(
        &self,
    ) -> (Option<String>, Option<ResumeRecord>, BTreeMap<u32, u64>) {
        (
            self.token.clone(),
            self.record.clone(),
            self.last_time.clone(),
        )
    }

    #[cfg(any(unix, windows))]
    /// The inverse of [`PaneResume::handoff_parts`].
    pub(crate) fn from_handoff(
        token: Option<String>,
        record: Option<ResumeRecord>,
        last_time: BTreeMap<u32, u64>,
    ) -> Self {
        Self {
            token,
            record,
            last_time,
            distrust_noted: false,
        }
    }

    /// The record as it should be saved or shown: when its owner is still the
    /// same running process, with the executable it runs now. A wrapper that
    /// reported and then ran a quiet program is bound even if no output has
    /// arrived since.
    pub(crate) fn current_record(&self, lookup: &dyn OwnerLookup) -> Option<ResumeRecord> {
        let mut record = self.record.clone()?;
        if let Some(owner) = record.owner {
            let same = lookup
                .identity(owner.pid)
                .is_some_and(|identity| identity.start == owner.start);
            if let Some(exe) = same.then(|| lookup.executable(owner.pid)).flatten() {
                record.owner_exe = Some(exe);
            }
        }
        Some(record)
    }

    /// Install a record saved before a restart. It has no running owner
    /// until the resumed program starts or reports.
    pub(crate) fn restore(&mut self, mut record: ResumeRecord) {
        record.owner = None;
        record.exited_at = None;
        record.started_at = None;
        self.record = Some(record);
    }

    /// Check a report body and apply it.
    pub(crate) fn receive(&mut self, body: &[u8], now: u64, lookup: &dyn OwnerLookup) -> Outcome {
        let Some(token) = self.token.as_deref() else {
            return Outcome::Ignored("no token for this pane");
        };
        let verdict = check(body, token, now, |pid| self.last_time.get(&pid).copied());
        if let Verdict::Ignored(reason) = verdict {
            return Outcome::Ignored(reason);
        }
        if let Err(reason) = lookup.pane_authority() {
            let first = !std::mem::replace(&mut self.distrust_noted, true);
            return Outcome::PaneNotTrusted { reason, first };
        }
        if let Verdict::Clear { pid, .. } | Verdict::Set { pid, .. } = &verdict {
            if let Some(problem) = owner_problem(*pid, lookup) {
                self.remove_owned_by(*pid);
                return Outcome::Rejected(problem);
            }
        }
        match verdict {
            Verdict::Ignored(reason) => Outcome::Ignored(reason),
            Verdict::Rejected { pid, reason } => {
                if let Some(pid) = pid {
                    self.remove_owned_by(pid);
                }
                Outcome::Rejected(reason)
            }
            Verdict::Clear { pid, time } => {
                self.remember(pid, time);
                if self.remove_owned_by(pid) {
                    Outcome::Cleared
                } else {
                    Outcome::Ignored("no record of this owner")
                }
            }
            Verdict::Set {
                pid,
                time,
                argv,
                cwd,
            } => {
                let Some(identity) = lookup.identity(pid) else {
                    return Outcome::Rejected("owner is not running");
                };
                self.remember(pid, time);
                self.record = Some(ResumeRecord {
                    argv,
                    cwd,
                    reported_at: time,
                    owner: Some(ResumeOwner {
                        pid,
                        start: identity.start,
                    }),
                    uid: identity.uid,
                    owner_exe: lookup.executable(pid),
                    not_resumed: None,
                    attempted: false,
                    exited_at: None,
                    started_at: None,
                });
                Outcome::Recorded
            }
        }
    }

    fn remember(&mut self, pid: u32, time: u64) {
        self.last_time.insert(pid, time);
        while self.last_time.len() > MAX_TRACKED_PIDS {
            let oldest = self
                .last_time
                .iter()
                .min_by_key(|(_, time)| **time)
                .map(|(pid, _)| *pid);
            if let Some(pid) = oldest {
                self.last_time.remove(&pid);
            }
        }
    }

    fn remove_owned_by(&mut self, pid: u32) -> bool {
        let owned = self
            .record
            .as_ref()
            .is_some_and(|record| record.owner.is_some_and(|owner| owner.pid == pid));
        if owned {
            self.record = None;
        }
        owned
    }

    /// Track the owner while the server runs: clear the record once its owner
    /// has been gone longer than the grace period, and mark a resumed program
    /// settled once it has stayed up.
    pub(crate) fn refresh(&mut self, now: u64, lookup: &dyn OwnerLookup) {
        let Some(record) = self.record.as_mut() else {
            return;
        };
        let Some(owner) = record.owner else {
            return;
        };
        let alive = lookup
            .identity(owner.pid)
            .is_some_and(|identity| identity.start == owner.start);
        if alive {
            record.exited_at = None;
            if let Some(exe) = lookup.executable(owner.pid) {
                record.owner_exe = Some(exe);
            }
            if record.attempted
                && record
                    .started_at
                    .is_some_and(|started| now.saturating_sub(started) >= RESUME_SETTLE_SECS)
            {
                record.attempted = false;
            }
            return;
        }
        let exited = *record.exited_at.get_or_insert(now);
        if now.saturating_sub(exited) > OWNER_EXIT_GRACE_SECS {
            self.record = None;
        }
    }

    /// The resumed program started as `pid`: the record is now its own.
    pub(crate) fn resumed(&mut self, pid: u32, now: u64, lookup: &dyn OwnerLookup) -> bool {
        let (Some(record), Some(identity)) = (self.record.as_mut(), lookup.identity(pid)) else {
            return false;
        };
        record.owner = Some(ResumeOwner {
            pid,
            start: identity.start,
        });
        record.owner_exe = lookup.executable(pid);
        record.not_resumed = None;
        record.attempted = true;
        record.started_at = Some(now);
        record.exited_at = None;
        true
    }

    /// The resume could not start: drop the record so it is not run again.
    pub(crate) fn resume_failed(&mut self) {
        self.record = None;
    }

    /// Note why the last restore did not resume this record.
    pub(crate) fn note_not_resumed(&mut self, reason: &str) {
        if let Some(record) = self.record.as_mut() {
            record.not_resumed = Some(reason.to_string());
        }
    }

    /// Mark a resume as started before the program is running, so a crash in
    /// between is not retried.
    pub(crate) fn mark_attempted(&mut self) {
        if let Some(record) = self.record.as_mut() {
            record.attempted = true;
        }
    }
}

/// `[mux]` program resume settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumePolicy {
    /// Command names (argv[0] basenames) resumed without asking. `*` trusts
    /// every command.
    pub trusted: Vec<String>,
    /// Accept reports and resume while the pmux server runs as root.
    pub allow_elevated: bool,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        Self {
            trusted: DEFAULT_TRUSTED
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            allow_elevated: false,
        }
    }
}

impl ResumePolicy {
    pub(crate) fn from_mux(section: &crate::MuxSection) -> Self {
        let default = Self::default();
        Self {
            trusted: section.resume_trusted.clone().unwrap_or(default.trusted),
            allow_elevated: section.resume_allow_elevated.unwrap_or(false),
        }
    }

    /// The config file's policy, or the defaults if it cannot be read.
    pub(crate) fn load() -> Self {
        crate::load_mux_section(&crate::prism_config_path())
            .map(|section| Self::from_mux(&section))
            .unwrap_or_default()
    }

    pub(crate) fn trusts(&self, argv0: &str) -> bool {
        let name = Path::new(argv0)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(argv0);
        self.trusted
            .iter()
            .any(|trusted| trusted == "*" || trusted == name)
    }

    /// Whether this server sets tokens, takes reports and resumes.
    pub(crate) fn enabled(&self) -> bool {
        protocol_enabled(PLATFORM_SUPPORTED, running_elevated(), self.allow_elevated)
    }
}

/// Linux and macOS can check a report's owner (user, start time, process
/// tree) and start a resume without building shell text. Windows cannot yet,
/// so the pmux server there sets no token and keeps restoring by its own
/// process inspection.
pub(crate) const PLATFORM_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

pub(crate) fn protocol_enabled(platform: bool, elevated: bool, allow_elevated: bool) -> bool {
    platform && (!elevated || allow_elevated)
}

fn running_elevated() -> bool {
    server_uid() == Some(0)
}

/// Why a record is not resumed because its command is not the program its
/// owner was running.
pub(crate) const UNBOUND: &str = "reported command does not match the running program";

/// The executable a record resumes, checked against what its owner ran.
///
/// A report only claims a command; any process in the pane that holds the
/// token can claim any name. The record is bound to the owner's real
/// executable: an absolute `argv[0]` must be that same file, and a bare name
/// must be that file's name and also what the name finds on the resume
/// `PATH`, so another program of the same name earlier on `PATH` cannot take
/// its place. `canonical` resolves a path, `on_path` looks up a bare name.
pub(crate) fn verified_executable(
    record: &ResumeRecord,
    canonical: impl Fn(&Path) -> Option<PathBuf>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Result<PathBuf, &'static str> {
    let exe = record.owner_exe.as_ref().ok_or(UNBOUND)?;
    let program = record.argv.first().ok_or(UNBOUND)?;
    if Path::new(program).is_absolute() {
        return match canonical(Path::new(program)) {
            Some(path) if path == *exe => Ok(path),
            _ => Err(UNBOUND),
        };
    }
    if exe.file_name().and_then(|name| name.to_str()) != Some(program.as_str()) {
        return Err(UNBOUND);
    }
    match on_path(program) {
        Some(path) if path == *exe => Ok(path),
        _ => Err("another program of that name comes first on PATH"),
    }
}

/// The canonical path of the first executable file named `name` on `path`.
pub(crate) fn resolve_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
        .and_then(|candidate| candidate.canonicalize().ok())
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// The verified executable to resume `record` with, or why it is not resumed.
/// The trust list applies to the verified executable's name.
pub(crate) fn restore_decision(
    record: &ResumeRecord,
    policy: &ResumePolicy,
    enabled: bool,
    uid: Option<u32>,
    canonical: impl Fn(&Path) -> Option<PathBuf>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
    dir_exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, &'static str> {
    if !enabled {
        return Err("program resume is off for this pmux server");
    }
    if record.attempted {
        return Err("its last resume did not start or did not last");
    }
    if uid != Some(record.uid) {
        return Err("it was reported as another user");
    }
    let exe = verified_executable(record, canonical, on_path)?;
    let name = exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !policy.trusts(name) {
        return Err("its command is not in [mux] resume_trusted");
    }
    if record.cwd.as_deref().is_some_and(|cwd| !dir_exists(cwd)) {
        return Err("its directory no longer exists");
    }
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procinfo::ProcessIdentity;
    use std::cell::RefCell;

    const TOKEN: &str = "dummy-token-replace-me";
    const NOW: u64 = 1_795_543_200;

    fn b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    fn signed(message: &str) -> Vec<u8> {
        format!("{message}:mac={}", mac_hex(message.as_bytes(), TOKEN)).into_bytes()
    }

    fn verdict(message: &str) -> Verdict {
        check(&signed(message), TOKEN, NOW, |_| None)
    }

    #[test]
    fn the_mac_matches_the_published_test_vector() {
        assert_eq!(
            mac_hex(b"pid=74877:time=1795543200:clear=1", TOKEN),
            "c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93"
        );
        assert_eq!(
            check(
                b"pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93",
                TOKEN,
                NOW,
                |_| None
            ),
            Verdict::Clear {
                pid: 74877,
                time: NOW
            }
        );
    }

    #[test]
    fn a_wrong_missing_misplaced_or_uppercase_mac_is_ignored() {
        let good = signed("pid=7:time=1795543200:clear=1");
        let mut tampered = good.clone();
        tampered[4] = b'8';
        let mut upper = good.clone();
        let len = upper.len();
        upper[len - 64..].make_ascii_uppercase();
        let mac = String::from_utf8(good[good.len() - 69..].to_vec()).unwrap();
        for body in [
            tampered,
            upper,
            b"pid=7:time=1795543200:clear=1".to_vec(),
            format!("pid=7{mac}:time=1795543200:clear=1").into_bytes(),
            signed("pid=7:time=1795543200:clear=1")[..good.len() - 1].to_vec(),
        ] {
            assert!(
                matches!(check(&body, TOKEN, NOW, |_| None), Verdict::Ignored(_)),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
        assert!(matches!(
            check(&good, "another-token-entirely", NOW, |_| None),
            Verdict::Ignored(_)
        ));
    }

    #[test]
    fn time_outside_the_window_or_older_than_the_last_report_is_ignored() {
        let at = |time: u64| format!("pid=7:time={time}:clear=1");
        assert!(matches!(verdict(&at(NOW + 61)), Verdict::Ignored(_)));
        assert!(matches!(verdict(&at(NOW - 61)), Verdict::Ignored(_)));
        assert!(matches!(verdict(&at(NOW - 60)), Verdict::Clear { .. }));
        assert!(matches!(
            verdict("pid=7:time=99999999999999999999999:clear=1"),
            Verdict::Ignored(_)
        ));
        assert!(matches!(verdict("pid=7:clear=1"), Verdict::Ignored(_)));
        assert!(matches!(
            verdict("pid=7:time=01795543200:clear=1"),
            Verdict::Ignored(_)
        ));
        let older = check(&signed(&at(NOW - 5)), TOKEN, NOW, |pid| {
            (pid == 7).then_some(NOW - 4)
        });
        assert!(matches!(older, Verdict::Ignored(_)));
        let same = check(&signed(&at(NOW - 4)), TOKEN, NOW, |_| Some(NOW - 4));
        assert!(matches!(same, Verdict::Clear { .. }));
    }

    #[test]
    fn a_valid_command_report_decodes_every_argument() {
        let message = format!(
            "time={NOW}:argv={},{},{},{}:pid=74877:cwd={}:need=cwd,argv:later=x",
            b64("nvim"),
            b64("-S"),
            b64("$(rm -rf ~); echo"),
            "",
            b64("/home/user/src/project"),
        );
        assert_eq!(
            verdict(&message),
            Verdict::Set {
                pid: 74877,
                time: NOW,
                argv: vec![
                    "nvim".into(),
                    "-S".into(),
                    "$(rm -rf ~); echo".into(),
                    String::new()
                ],
                cwd: Some("/home/user/src/project".into()),
            }
        );
        // Unpadded base64 is accepted too.
        let unpadded = format!("pid=7:time={NOW}:argv={}", b64("vi").trim_end_matches('='));
        assert!(matches!(verdict(&unpadded), Verdict::Set { .. }));
    }

    #[test]
    fn every_strict_parse_failure_is_rejected_for_its_owner() {
        let time = format!("time={NOW}");
        let cases = [
            format!("pid=7:{time}:argv={}:argv={}", b64("vi"), b64("vi")),
            format!("pid=7:{time}:Argv={}", b64("vi")),
            format!("pid=7:{time}:argv={}:x=a;b", b64("vi")),
            format!("pid=7:{time}:argv={}:clear=1", b64("vi")),
            format!("pid=7:{time}"),
            format!("pid=7:{time}:clear=2"),
            format!("pid=7:{time}:argv="),
            format!("pid=7:{time}:argv={}", b64("./run.sh")),
            format!("pid=7:{time}:argv={}", b64("bin/run")),
            format!("pid=7:{time}:argv={}", b64("-x")),
            format!("pid=7:{time}:argv=,{}", b64("x")),
            format!("pid=7:{time}:argv={}", b64("vi\nrm")),
            format!("pid=7:{time}:argv={}", b64("vi\u{85}")),
            format!("pid=7:{time}:argv={}", b64("vi\u{7f}")),
            format!("pid=7:{time}:argv=QQ="),
            format!("pid=7:{time}:argv=/w=="),
            format!("pid=7:{time}:argv=gA=="),
            format!("pid=7:{time}:argv={}:cwd={}", b64("vi"), b64("project")),
            format!("pid=7:{time}:argv={}:need=later", b64("vi")),
            format!("pid=7:{time}:argv={}:novalue", b64("vi")),
        ];
        for message in cases {
            assert!(
                matches!(verdict(&message), Verdict::Rejected { pid: Some(7), .. }),
                "{message}: {:?}",
                verdict(&message)
            );
        }
        assert!(matches!(
            verdict(&format!("{time}:argv={}", b64("vi"))),
            Verdict::Rejected { pid: None, .. }
        ));
    }

    #[test]
    fn a_report_with_the_most_arguments_that_fit_is_accepted() {
        let argv = std::iter::repeat_n(b64("a"), 300)
            .collect::<Vec<_>>()
            .join(",");
        let message = format!("pid=7:time={NOW}:argv={argv}");
        assert!(message.len() + 5 + 64 + 8 <= 2048);
        match verdict(&message) {
            Verdict::Set { argv, .. } => assert_eq!(argv.len(), 300),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tokens_are_random_and_in_the_allowed_alphabet() {
        let first = new_token().unwrap();
        let second = new_token().unwrap();
        assert_ne!(first, second);
        for token in [&first, &second] {
            assert!((22..=64).contains(&token.len()), "{token}");
            assert!(token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
        }
    }

    struct FakeLookup {
        processes: RefCell<BTreeMap<u32, ProcessIdentity>>,
        exes: RefCell<BTreeMap<u32, PathBuf>>,
        pane: Vec<u32>,
        uid: u32,
        authority: std::cell::Cell<Result<(), &'static str>>,
    }

    impl FakeLookup {
        fn new(pane: &[u32]) -> Self {
            let processes = pane
                .iter()
                .map(|pid| {
                    (
                        *pid,
                        ProcessIdentity {
                            uid: 501,
                            start: u64::from(*pid) * 10,
                        },
                    )
                })
                .collect();
            Self {
                processes: RefCell::new(processes),
                exes: RefCell::new(BTreeMap::new()),
                pane: pane.to_vec(),
                uid: 501,
                authority: std::cell::Cell::new(Ok(())),
            }
        }

        fn runs(&self, pid: u32, exe: &str) {
            self.exes.borrow_mut().insert(pid, PathBuf::from(exe));
        }
    }

    impl OwnerLookup for FakeLookup {
        fn identity(&self, pid: u32) -> Option<ProcessIdentity> {
            self.processes.borrow().get(&pid).copied()
        }
        fn executable(&self, pid: u32) -> Option<PathBuf> {
            self.exes.borrow().get(&pid).cloned()
        }
        fn in_pane(&self, pid: u32) -> bool {
            self.pane.contains(&pid)
        }
        fn server_uid(&self) -> Option<u32> {
            Some(self.uid)
        }
        fn pane_authority(&self) -> Result<(), &'static str> {
            self.authority.get()
        }
    }

    use crate::procinfo::ProcessFacts;

    fn facts(uid: u32) -> ProcessFacts {
        ProcessFacts {
            uid,
            sandboxed: false,
            root_dir: Some(PathBuf::from("/")),
            namespaces: vec![PathBuf::from("user:[1]"), PathBuf::from("mnt:[2]")],
        }
    }

    /// A pane whose first process is 10 with descendants 11 and 12;
    /// `change` alters process 12.
    fn authority_with(change: impl Fn(&mut ProcessFacts)) -> Result<(), &'static str> {
        pane_authority(
            Some(10),
            |root| (root == 10).then(|| vec![10, 11, 12]),
            |pid| {
                let mut process = facts(501);
                if pid == 12 {
                    change(&mut process);
                }
                Ok(Some(process))
            },
            Ok(facts(501)),
        )
    }

    #[test]
    fn a_pane_of_one_user_in_the_servers_root_and_namespaces_may_report() {
        assert_eq!(authority_with(|_| {}), Ok(()));
    }

    #[test]
    fn a_descendant_of_another_user_makes_the_pane_untrusted() {
        assert_eq!(authority_with(|p| p.uid = 0), Err(OTHER_USER));
    }

    #[test]
    fn a_sandboxed_descendant_makes_the_pane_untrusted() {
        assert_eq!(authority_with(|p| p.sandboxed = true), Err(SANDBOXED));
    }

    #[test]
    fn a_descendant_in_another_root_or_namespace_makes_the_pane_untrusted() {
        assert_eq!(
            authority_with(|p| p.root_dir = Some(PathBuf::from("/jail"))),
            Err(OTHER_ROOT)
        );
        assert_eq!(
            authority_with(|p| p.namespaces[0] = PathBuf::from("user:[9]")),
            Err(OTHER_ROOT)
        );
    }

    #[test]
    fn anything_that_cannot_be_checked_makes_the_pane_untrusted() {
        let ok = |_: u32| Ok(Some(facts(501)));
        let tree = |_: u32| Some(vec![10, 11]);
        // A process that cannot be read.
        assert_eq!(
            pane_authority(
                Some(10),
                tree,
                |pid| if pid == 11 {
                    Err(())
                } else {
                    Ok(Some(facts(501)))
                },
                Ok(facts(501))
            ),
            Err(UNCHECKED)
        );
        // A tree that cannot be listed.
        assert_eq!(
            pane_authority(Some(10), |_| None, ok, Ok(facts(501))),
            Err(UNCHECKED)
        );
        // The server's own facts.
        assert_eq!(
            pane_authority(Some(10), tree, ok, Err(UNCHECKED)),
            Err(UNCHECKED)
        );
        // No first process, or one that is gone.
        assert_eq!(
            pane_authority(None, tree, ok, Ok(facts(501))),
            Err(UNCHECKED)
        );
        assert_eq!(
            pane_authority(Some(10), tree, |_| Ok(None), Ok(facts(501))),
            Err(UNCHECKED)
        );
        // A descendant that exited after the listing is no longer in the pane.
        assert_eq!(
            pane_authority(
                Some(10),
                tree,
                |pid| Ok((pid == 10).then(|| facts(501))),
                Ok(facts(501))
            ),
            Ok(())
        );
    }

    /// Start `/bin/sh -c script` as a stand-in pane. The script starts one
    /// child that prints `ready` once it is running (inside its sandbox, if
    /// any). Kills the whole tree on drop.
    #[cfg(target_os = "macos")]
    struct FakePane(std::process::Child);

    #[cfg(target_os = "macos")]
    impl FakePane {
        fn start(script: &str) -> Self {
            use std::process::{Command, Stdio};
            let child = prismattyc_process::spawn(
                Command::new("/bin/sh")
                    .args(["-c", script])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null()),
            )
            .unwrap();
            Self(child)
        }

        fn lookup(&self) -> SystemLookup {
            SystemLookup {
                root: Some(self.0.id()),
            }
        }

        /// Block until the child says it is running, then check the pane.
        fn authority_when_ready(&mut self) -> Result<(), &'static str> {
            use std::io::BufRead;
            let stdout = self.0.stdout.as_mut().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "ready", "the stand-in child did not start");
            let tree = crate::procinfo::process_tree_strict(self.0.id()).unwrap();
            assert!(tree.len() >= 2, "the child is in the pane: {tree:?}");
            self.lookup().pane_authority()
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for FakePane {
        fn drop(&mut self) {
            for pid in crate::procinfo::process_tree_strict(self.0.id()).unwrap_or_default() {
                let _ = rustix::process::kill_process(
                    rustix::process::Pid::from_raw(pid as i32).unwrap(),
                    rustix::process::Signal::KILL,
                );
            }
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_sandboxed_child_makes_its_pane_untrusted_and_a_plain_one_does_not() {
        let mut plain = FakePane::start("/bin/sh -c 'echo ready; exec /bin/sleep 30' & wait");
        assert_eq!(plain.authority_when_ready(), Ok(()));
        let mut boxed = FakePane::start(
            "sandbox-exec -p '(version 1)(allow default)' /bin/sh -c 'echo ready; exec /bin/sleep 30' & wait",
        );
        assert_eq!(boxed.authority_when_ready(), Err(SANDBOXED));
    }

    #[test]
    fn reports_from_an_untrusted_pane_change_nothing_and_are_logged_once() {
        let lookup = FakeLookup::new(&[7]);
        lookup.runs(7, "/opt/bin/nvim");
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        lookup.authority.set(Err(SANDBOXED));
        let clear = signed(&format!("pid=7:time={NOW}:clear=1"));
        assert_eq!(
            pane.receive(&clear, NOW, &lookup),
            Outcome::PaneNotTrusted {
                reason: SANDBOXED,
                first: true
            }
        );
        assert_eq!(
            pane.receive(&set_report(7, NOW, "vim"), NOW, &lookup),
            Outcome::PaneNotTrusted {
                reason: SANDBOXED,
                first: false
            }
        );
        assert_eq!(pane.record().unwrap().argv, ["nvim"]);
        // A report that fails its own checks is still just ignored.
        assert!(matches!(
            pane.receive(b"pid=7:time=1:clear=1:mac=00", NOW, &lookup),
            Outcome::Ignored(_)
        ));
    }

    fn set_report(pid: u32, time: u64, program: &str) -> Vec<u8> {
        signed(&format!("pid={pid}:time={time}:argv={}", b64(program)))
    }

    #[test]
    fn ignored_reports_leave_the_record_and_rejected_ones_remove_their_owners() {
        let lookup = FakeLookup::new(&[7, 8]);
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        assert_eq!(
            pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup),
            Outcome::Recorded
        );
        let mut forged = set_report(7, NOW, "evil");
        let at = forged.len() - 1;
        forged[at] ^= 1;
        assert!(matches!(
            pane.receive(&forged, NOW, &lookup),
            Outcome::Ignored(_)
        ));
        assert_eq!(pane.record().unwrap().argv, ["nvim"]);
        // Another pid's rejected report does not touch this owner's record.
        let other = signed(&format!("pid=8:time={NOW}:argv={}", b64("./x")));
        assert!(matches!(
            pane.receive(&other, NOW, &lookup),
            Outcome::Rejected(_)
        ));
        assert_eq!(pane.record().unwrap().argv, ["nvim"]);
        let bad = signed(&format!("pid=7:time={NOW}:argv={}", b64("./x")));
        assert!(matches!(
            pane.receive(&bad, NOW, &lookup),
            Outcome::Rejected(_)
        ));
        assert!(pane.record().is_none());
    }

    #[test]
    fn a_pane_without_a_token_takes_no_reports() {
        let lookup = FakeLookup::new(&[7]);
        let mut pane = PaneResume::new(None, None);
        assert!(matches!(
            pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup),
            Outcome::Ignored(_)
        ));
        assert!(pane.record().is_none());
    }

    #[test]
    fn owners_outside_the_pane_from_another_user_or_gone_are_rejected() {
        let mut lookup = FakeLookup::new(&[7]);
        lookup.processes.borrow_mut().insert(
            9,
            ProcessIdentity {
                uid: 501,
                start: 90,
            },
        );
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        assert!(matches!(
            pane.receive(&set_report(9, NOW, "nvim"), NOW, &lookup),
            Outcome::Rejected("owner is not in this pane")
        ));
        assert!(matches!(
            pane.receive(&set_report(10, NOW, "nvim"), NOW, &lookup),
            Outcome::Rejected("owner is not running")
        ));
        lookup.uid = 0;
        assert!(matches!(
            pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup),
            Outcome::Rejected("owner runs as another user")
        ));
        assert!(pane.record().is_none());
    }

    #[test]
    fn a_clear_removes_only_its_owners_record_and_older_reports_are_ignored() {
        let lookup = FakeLookup::new(&[7, 8]);
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        let clear8 = signed(&format!("pid=8:time={NOW}:clear=1"));
        assert!(matches!(
            pane.receive(&clear8, NOW, &lookup),
            Outcome::Ignored(_)
        ));
        assert!(pane.record().is_some());
        // A replay of an earlier report from the same pid cannot win.
        assert!(matches!(
            pane.receive(&set_report(7, NOW - 1, "vim"), NOW, &lookup),
            Outcome::Ignored(_)
        ));
        assert_eq!(pane.record().unwrap().argv, ["nvim"]);
        let clear7 = signed(&format!("pid=7:time={NOW}:clear=1"));
        assert_eq!(pane.receive(&clear7, NOW, &lookup), Outcome::Cleared);
        assert!(pane.record().is_none());
    }

    #[test]
    fn the_record_outlives_its_owner_only_for_the_grace_period_and_pid_reuse_counts_as_gone() {
        let lookup = FakeLookup::new(&[7]);
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        pane.refresh(NOW + 30, &lookup);
        assert!(pane.record().is_some(), "a live owner keeps its record");
        // Same pid, new process.
        lookup
            .processes
            .borrow_mut()
            .insert(7, ProcessIdentity { uid: 501, start: 1 });
        pane.refresh(NOW + 31, &lookup);
        assert!(pane.record().is_some(), "just gone: still within the grace");
        pane.refresh(NOW + 31 + OWNER_EXIT_GRACE_SECS, &lookup);
        assert!(pane.record().is_some());
        pane.refresh(NOW + 32 + OWNER_EXIT_GRACE_SECS, &lookup);
        assert!(pane.record().is_none());
    }

    #[test]
    fn a_resume_is_retried_only_after_the_program_stayed_up() {
        let lookup = FakeLookup::new(&[7, 20]);
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        let mut saved = pane.record().unwrap().clone();
        saved.owner = None;
        let mut restored = PaneResume::new(Some(TOKEN.into()), Some(saved.clone()));
        assert!(restored.resumed(20, NOW, &lookup));
        assert!(restored.record().unwrap().attempted);
        restored.refresh(NOW + RESUME_SETTLE_SECS - 1, &lookup);
        assert!(restored.record().unwrap().attempted);
        restored.refresh(NOW + RESUME_SETTLE_SECS, &lookup);
        assert!(!restored.record().unwrap().attempted);
        restored.resume_failed();
        assert!(restored.record().is_none());
        // A resume handed off but never started stays marked, so the next
        // restore skips it.
        let mut pending = PaneResume::new(Some(TOKEN.into()), Some(saved));
        pending.mark_attempted();
        assert!(pending.record().unwrap().attempted);
    }

    fn record(argv: &[&str], cwd: Option<&str>) -> ResumeRecord {
        ResumeRecord {
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: cwd.map(PathBuf::from),
            reported_at: NOW,
            owner: None,
            uid: 501,
            owner_exe: Some(PathBuf::from(format!(
                "/opt/bin/{}",
                Path::new(argv[0]).file_name().unwrap().to_str().unwrap()
            ))),
            not_resumed: None,
            attempted: false,
            exited_at: None,
            started_at: None,
        }
    }

    /// `/opt/bin` holds every program; paths are already canonical.
    fn decide(record: &ResumeRecord, policy: &ResumePolicy) -> Result<PathBuf, &'static str> {
        restore_decision(
            record,
            policy,
            true,
            Some(501),
            |path| Some(path.to_path_buf()),
            |name| Some(PathBuf::from(format!("/opt/bin/{name}"))),
            |path| path == Path::new("/src"),
        )
    }

    #[test]
    fn a_child_claiming_a_trusted_name_is_recorded_but_never_resumed() {
        let lookup = FakeLookup::new(&[7]);
        lookup.runs(7, "/bin/sh");
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        let report = signed(&format!(
            "pid=7:time={NOW}:argv={},{},{}",
            b64("nvim"),
            b64("-c"),
            b64("!touch /tmp/owned")
        ));
        assert_eq!(pane.receive(&report, NOW, &lookup), Outcome::Recorded);
        let record = pane.record().unwrap();
        assert_eq!(record.owner_exe.as_deref(), Some(Path::new("/bin/sh")));
        assert_eq!(decide(record, &ResumePolicy::default()), Err(UNBOUND));
    }

    #[test]
    fn the_program_that_reported_is_resumed_from_its_own_file() {
        let lookup = FakeLookup::new(&[7]);
        lookup.runs(7, "/opt/bin/nvim");
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        let report = signed(&format!(
            "pid=7:time={NOW}:argv={},{}",
            b64("nvim"),
            b64("-S")
        ));
        pane.receive(&report, NOW, &lookup);
        assert_eq!(
            decide(pane.record().unwrap(), &ResumePolicy::default()),
            Ok(PathBuf::from("/opt/bin/nvim"))
        );
    }

    #[test]
    fn a_wrapper_that_reports_then_runs_its_target_is_bound_after_the_refresh() {
        let lookup = FakeLookup::new(&[7]);
        lookup.runs(7, "/bin/sh");
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        assert_eq!(
            decide(pane.record().unwrap(), &ResumePolicy::default()),
            Err(UNBOUND)
        );
        // The wrapper execs its target: same pid and start, new executable.
        lookup.runs(7, "/opt/bin/nvim");
        pane.refresh(NOW + 1, &lookup);
        assert_eq!(
            decide(pane.record().unwrap(), &ResumePolicy::default()),
            Ok(PathBuf::from("/opt/bin/nvim"))
        );
    }

    #[test]
    fn a_saved_record_shows_what_its_owner_runs_now_without_waiting_for_output() {
        let lookup = FakeLookup::new(&[7]);
        lookup.runs(7, "/bin/sh");
        let mut pane = PaneResume::new(Some(TOKEN.into()), None);
        pane.receive(&set_report(7, NOW, "nvim"), NOW, &lookup);
        lookup.runs(7, "/opt/bin/nvim");
        let saved = pane.current_record(&lookup).unwrap();
        assert_eq!(saved.owner_exe.as_deref(), Some(Path::new("/opt/bin/nvim")));
        // A different process on the same pid is not the owner.
        lookup
            .processes
            .borrow_mut()
            .insert(7, ProcessIdentity { uid: 501, start: 1 });
        lookup.runs(7, "/opt/bin/evil");
        let saved = pane.current_record(&lookup).unwrap();
        assert_eq!(saved.owner_exe.as_deref(), Some(Path::new("/bin/sh")));
    }

    #[test]
    fn a_different_program_first_on_path_or_another_absolute_file_is_refused() {
        let policy = ResumePolicy::default();
        let real = record(&["nvim"], None);
        let shadowed = restore_decision(
            &real,
            &policy,
            true,
            Some(501),
            |path| Some(path.to_path_buf()),
            |name| Some(PathBuf::from(format!("/home/user/bin/{name}"))),
            |_| true,
        );
        assert!(matches!(shadowed, Err(reason) if reason.contains("PATH")));
        let mut absolute = record(&["/opt/bin/nvim"], None);
        assert_eq!(
            decide(&absolute, &policy),
            Ok(PathBuf::from("/opt/bin/nvim"))
        );
        absolute.argv[0] = "/usr/bin/nvim".into();
        assert_eq!(decide(&absolute, &policy), Err(UNBOUND));
        let mut renamed = record(&["nvim"], None);
        renamed.owner_exe = Some(PathBuf::from("/opt/bin/vim"));
        assert_eq!(decide(&renamed, &policy), Err(UNBOUND));
        let mut never_seen = record(&["nvim"], None);
        never_seen.owner_exe = None;
        assert_eq!(decide(&never_seen, &policy), Err(UNBOUND));
    }

    #[test]
    fn only_trusted_settled_same_user_records_with_a_directory_are_resumed() {
        let policy = ResumePolicy::default();
        assert!(decide(&record(&["nvim"], Some("/src")), &policy).is_ok());
        assert!(decide(&record(&["/opt/bin/nvim", "-S", "x"], None), &policy).is_ok());
        assert!(decide(&record(&["ssh", "host"], None), &policy).is_err());
        assert!(decide(&record(&["nvim"], Some("/gone")), &policy).is_err());
        let mut other_user = record(&["nvim"], None);
        other_user.uid = 502;
        assert!(decide(&other_user, &policy).is_err());
        let off = restore_decision(
            &record(&["nvim"], None),
            &policy,
            false,
            Some(501),
            |path| Some(path.to_path_buf()),
            |name| Some(PathBuf::from(format!("/opt/bin/{name}"))),
            |_| true,
        );
        assert!(off.is_err());
        let mut attempted = record(&["nvim"], None);
        attempted.attempted = true;
        assert!(decide(&attempted, &policy).is_err());
        let all = ResumePolicy {
            trusted: vec!["*".into()],
            allow_elevated: false,
        };
        assert!(decide(&record(&["ssh", "host"], None), &all).is_ok());
    }

    #[test]
    fn an_elevated_server_resumes_only_when_opted_in_and_unsupported_platforms_never() {
        assert!(protocol_enabled(true, false, false));
        assert!(!protocol_enabled(true, true, false));
        assert!(protocol_enabled(true, true, true));
        assert!(!protocol_enabled(false, false, true));
        assert_eq!(
            PLATFORM_SUPPORTED,
            cfg!(any(target_os = "linux", target_os = "macos"))
        );
    }

    #[test]
    fn the_display_form_escapes_control_and_direction_characters() {
        let shown = record(&["nvim", "a\u{202e}b", "x\ty"], None).display();
        assert!(
            !shown.contains('\u{202e}') && !shown.contains('\t'),
            "{shown}"
        );
    }
}
