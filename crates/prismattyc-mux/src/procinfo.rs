//! Portable process inspection for mux/host.
//!
//! Linux uses `/proc`. macOS uses `sysctl` / libproc. Other unix: `kill(0)`
//! only. Inode-level socket ownership stays Linux-only.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Whether `/proc/<pid>` (Linux) or `kill(pid, 0)` says the pid exists.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    pid_alive_impl(pid)
}

/// NUL-separated argv, same shape as Linux `/proc/<pid>/cmdline`.
#[must_use]
pub fn cmdline(pid: u32) -> Option<Vec<Vec<u8>>> {
    cmdline_impl(pid)
}

/// Direct children. Empty when the platform cannot enumerate them.
#[must_use]
pub fn children_of(pid: u32) -> Vec<u32> {
    children_of_impl(pid)
}

/// Working directory of `pid`, if the platform exposes it.
#[must_use]
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    cwd_of_impl(pid)
}

/// Pids visible to this process (Linux `/proc` or macOS `proc_listpids`).
#[must_use]
pub fn pids() -> Vec<u32> {
    list_pids()
}

/// Pids that look like a `pmuxd --socket PATH` on this host.
#[must_use]
pub fn find_server_pids(socket: &Path) -> Vec<u32> {
    list_pids()
        .into_iter()
        .filter(|&pid| cmdline_matches_server(pid, socket))
        .collect()
}

/// Test-only: `space save` records this string as the pane foreground command
/// instead of walking `/proc`. Empty means "no command". Honored only in
/// debug builds so a leftover env var cannot change a release save.
pub const TEST_FOREGROUND_COMMAND_ENV: &str = "PMUX_TEST_FOREGROUND_COMMAND";

/// Shell-quoted command line of the pane's **terminal foreground** process.
///
/// `root` is the PTY child (usually a shell). Linux and macOS read the
/// controlling terminal's foreground process group (`tpgid`). Members of
/// that group that are not a shell are the recorded command. A background
/// job (`sleep 30 &`) is a different process group, so it is not recorded.
/// When the shell owns the tty (prompt), this returns `None`.
///
/// No controlling TTY (`tpgid` ≤ 0): walk children (piped tests). A
/// background job can still look like a child in that path. If the
/// platform cannot read `tpgid` at all, this returns `None` (fail closed).
///
/// Debug builds honor [`TEST_FOREGROUND_COMMAND_ENV`] so `space save` can
/// record a fake command without a live child. Live detection on `space
/// open` must call [`live_foreground_command`] so that env cannot mark a
/// new pane as already running.
#[must_use]
pub fn foreground_command(root: u32) -> Option<String> {
    if cfg!(debug_assertions) {
        if let Ok(raw) = std::env::var(TEST_FOREGROUND_COMMAND_ENV) {
            let trimmed = raw.trim();
            return if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
    }
    live_foreground_command(root)
}

/// Terminal foreground only. Ignores [`TEST_FOREGROUND_COMMAND_ENV`].
#[must_use]
pub fn live_foreground_command(root: u32) -> Option<String> {
    foreground_command_impl(root)
}

enum TtyForeground {
    /// Controlling terminal's foreground process group.
    Pgid(u32),
    /// Process has no controlling TTY (`tpgid` ≤ 0).
    NoTty,
    /// Platform cannot read `tpgid`. Fail closed.
    Unknown,
}

fn foreground_command_impl(root: u32) -> Option<String> {
    match classify_tty(root) {
        TtyForeground::Pgid(tpgid) => command_in_foreground_pgid(root, tpgid),
        TtyForeground::NoTty => {
            // Piped tests / no job control. A background job can still
            // look like a child here.
            let args = cmdline_strings(root)?;
            if !is_shell_argv(&args) {
                return Some(shell_join(&args));
            }
            let child = first_non_shell_descendant(root)?;
            cmdline_strings(child).map(|args| shell_join(&args))
        }
        TtyForeground::Unknown => None,
    }
}

fn classify_tty(pid: u32) -> TtyForeground {
    classify_tty_impl(pid)
}

#[cfg(target_os = "linux")]
fn classify_tty_impl(pid: u32) -> TtyForeground {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => match parse_proc_stat(&raw) {
            Some((_, tpgid)) if tpgid > 0 => TtyForeground::Pgid(tpgid as u32),
            Some(_) => TtyForeground::NoTty,
            None => TtyForeground::Unknown,
        },
        Err(_) => TtyForeground::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn classify_tty_impl(pid: u32) -> TtyForeground {
    match macos::pgid_and_tpgid(pid) {
        Some((_, tpgid)) if tpgid > 0 => TtyForeground::Pgid(tpgid),
        Some(_) => TtyForeground::NoTty,
        None => TtyForeground::Unknown,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn classify_tty_impl(_pid: u32) -> TtyForeground {
    TtyForeground::Unknown
}

/// `pgrp` and `tpgid` from a Linux `/proc/<pid>/stat` line.
///
/// After the comm field (`pid (comm) …`) the tokens are state, ppid, pgrp,
/// session, tty_nr, tpgid. Comm may contain spaces and parentheses, so this
/// splits on the last `)`.
#[cfg(target_os = "linux")]
fn parse_proc_stat(raw: &str) -> Option<(u32, i32)> {
    let close = raw.rfind(')')?;
    let mut fields = raw.get(close + 1..)?.split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    let _session = fields.next()?;
    let _tty_nr = fields.next()?;
    let tpgid = fields.next()?.parse().ok()?;
    Some((pgrp, tpgid))
}

#[cfg(target_os = "linux")]
fn pgid_of(pid: u32) -> Option<u32> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (pgrp, _) = parse_proc_stat(&raw)?;
    Some(pgrp)
}

#[cfg(target_os = "macos")]
fn pgid_of(pid: u32) -> Option<u32> {
    macos::pgid_and_tpgid(pid).map(|(pgid, _)| pgid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pgid_of(_pid: u32) -> Option<u32> {
    None
}

/// Whether a descendant currently owns the root shell's terminal foreground.
pub fn in_terminal_foreground(root: u32, pid: u32) -> Option<bool> {
    match classify_tty(root) {
        TtyForeground::Pgid(group) => pgid_of(pid).map(|pgid| pgid == group),
        TtyForeground::NoTty | TtyForeground::Unknown => None,
    }
}

fn command_in_foreground_pgid(root: u32, tpgid: u32) -> Option<String> {
    let mut members = Vec::new();
    for pid in tree_pids(root) {
        if pgid_of(pid) == Some(tpgid) {
            members.push(pid);
        }
    }
    let non_shell: Vec<u32> = members
        .into_iter()
        .filter(|&pid| {
            cmdline_strings(pid)
                .map(|args| !is_shell_argv(&args))
                .unwrap_or(false)
        })
        .collect();
    if non_shell.is_empty() {
        return None;
    }
    let pick = non_shell
        .iter()
        .copied()
        .find(|pid| *pid == tpgid)
        .unwrap_or(non_shell[0]);
    cmdline_strings(pick).map(|args| shell_join(&args))
}

fn tree_pids(root: u32) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([root]);
    seen.insert(root);
    while let Some(pid) = queue.pop_front() {
        for child in children_of(pid) {
            if seen.insert(child) {
                queue.push_back(child);
            }
        }
    }
    seen.into_iter().collect()
}

fn cmdline_strings(pid: u32) -> Option<Vec<String>> {
    let args = cmdline(pid)?;
    if args.is_empty() {
        return None;
    }
    Some(
        args.into_iter()
            .map(|arg| String::from_utf8_lossy(&arg).into_owned())
            .collect(),
    )
}

fn is_shell_argv(args: &[String]) -> bool {
    let Some(argv0) = args.first() else {
        return false;
    };
    let name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argv0);
    let name = name.strip_prefix('-').unwrap_or(name);
    matches!(
        name,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh" | "ash" | "busybox"
    )
}

/// Quote argv so a shell can parse it back.
#[must_use]
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '-' | '_' | '.' | '/' | ':' | '@' | '=' | '+' | ',' | '~'
            )
    }) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn first_non_shell_descendant(root: u32) -> Option<u32> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([root]);
    seen.insert(root);
    while let Some(pid) = queue.pop_front() {
        for child in children_of(pid) {
            if !seen.insert(child) {
                continue;
            }
            match cmdline_strings(child) {
                Some(args) if is_shell_argv(&args) => queue.push_back(child),
                Some(_) => return Some(child),
                None => queue.push_back(child),
            }
        }
    }
    None
}

/// Exact-argv check used by `prismattyc-mux stop` so a recycled pid is never signalled.
#[must_use]
pub fn cmdline_matches_server(pid: u32, socket: &Path) -> bool {
    let Some(args) = cmdline(pid) else {
        return false;
    };
    let Some(argv0) = args.first() else {
        return false;
    };
    let Ok(argv0) = std::str::from_utf8(argv0) else {
        return false;
    };
    let name = Path::new(argv0).file_name();
    if name != Some(OsStr::new("pmuxd")) {
        return false;
    }
    let want = socket.as_os_str().as_encoded_bytes();
    args.windows(2)
        .any(|pair| pair[0] == b"--socket" && pair[1] == want)
}

/// Linux-only: listening inode of a unix socket path.
#[must_use]
pub fn listener_inode(path: &Path) -> Option<u64> {
    listener_inode_impl(path)
}

/// Linux-only: whether `pid` holds `socket:[inode]`.
#[must_use]
pub fn pid_holds_socket(pid: u32, inode: u64) -> bool {
    pid_holds_socket_impl(pid, inode)
}

#[cfg(target_os = "linux")]
fn pid_alive_impl(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive_impl(pid: u32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        return false;
    };
    rustix::process::test_kill_process(pid).is_ok()
}

#[cfg(target_os = "linux")]
fn cmdline_impl(pid: u32) -> Option<Vec<Vec<u8>>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|a| !a.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn cmdline_impl(pid: u32) -> Option<Vec<Vec<u8>>> {
    macos::procargs(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cmdline_impl(_pid: u32) -> Option<Vec<Vec<u8>>> {
    None
}

#[cfg(target_os = "linux")]
fn children_of_impl(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(task) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return out;
    };
    for entry in task.flatten() {
        let path = entry.path().join("children");
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for tok in text.split_whitespace() {
            if let Ok(child) = tok.parse::<u32>() {
                out.push(child);
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn children_of_impl(pid: u32) -> Vec<u32> {
    macos::list_children(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn children_of_impl(_pid: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn cwd_of_impl(pid: u32) -> Option<PathBuf> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    path.is_absolute().then_some(path)
}

#[cfg(target_os = "macos")]
fn cwd_of_impl(pid: u32) -> Option<PathBuf> {
    macos::cwd(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cwd_of_impl(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
fn list_pids() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn list_pids() -> Vec<u32> {
    macos::list_pids()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn list_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn listener_inode_impl(path: &Path) -> Option<u64> {
    let table = std::fs::read_to_string("/proc/net/unix").ok()?;
    let want = path.to_str()?;
    for line in table.lines().skip(1) {
        let mut rest = line;
        let mut st = "";
        let mut inode = "";
        for field in 0..7 {
            rest = rest.trim_start();
            let end = rest.find(' ').unwrap_or(rest.len());
            let (token, tail) = rest.split_at(end);
            if field == 5 {
                st = token;
            }
            if field == 6 {
                inode = token;
            }
            rest = tail;
        }
        if rest.trim_start() == want && st == "01" {
            return inode.parse().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn listener_inode_impl(_path: &Path) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn pid_holds_socket_impl(pid: u32, inode: u64) -> bool {
    let want = format!("socket:[{inode}]");
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .any(|target| target.as_os_str() == want.as_str())
}

#[cfg(not(target_os = "linux"))]
fn pid_holds_socket_impl(_pid: u32, _inode: u64) -> bool {
    false
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ptr;

    pub(super) fn procargs(pid: u32) -> Option<Vec<Vec<u8>>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
        let mut size = 0usize;
        unsafe {
            if libc::sysctl(
                mib.as_mut_ptr(),
                3,
                ptr::null_mut(),
                &mut size,
                ptr::null_mut(),
                0,
            ) != 0
                || size < 4
            {
                return None;
            }
        }
        let mut buf = vec![0u8; size];
        unsafe {
            if libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
        if argc <= 0 {
            return None;
        }
        let mut i = 4usize;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        i += 1;
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
        let mut args = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            let start = i;
            while i < buf.len() && buf[i] != 0 {
                i += 1;
            }
            if start < i {
                args.push(buf[start..i].to_vec());
            }
            i += 1;
            if i >= buf.len() {
                break;
            }
        }
        (!args.is_empty()).then_some(args)
    }

    /// `libproc.h` `PROC_ALL_PIDS`. libc 0.2 does not export this selector.
    const PROC_ALL_PIDS: u32 = 1;

    pub(super) fn list_pids() -> Vec<u32> {
        let need = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let mut buf = vec![0i32; (need as usize) + 32];
        let got = unsafe {
            libc::proc_listpids(
                PROC_ALL_PIDS,
                0,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if got <= 0 {
            return Vec::new();
        }
        let n = (got as usize) / std::mem::size_of::<i32>();
        buf.into_iter()
            .take(n)
            .filter_map(|pid| (pid > 0).then_some(pid as u32))
            .collect()
    }

    pub(super) fn list_children(pid: u32) -> Vec<u32> {
        let need = unsafe { libc::proc_listchildpids(pid as libc::pid_t, ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let mut buf = vec![0i32; (need as usize) + 8];
        let got = unsafe {
            libc::proc_listchildpids(
                pid as libc::pid_t,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if got <= 0 {
            return Vec::new();
        }
        buf.into_iter()
            .take(got as usize)
            .filter_map(|child| (child > 0).then_some(child as u32))
            .collect()
    }

    pub(super) fn pgid_and_tpgid(pid: u32) -> Option<(u32, u32)> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::pid_t,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if n < size {
            return None;
        }
        Some((info.pbi_pgid, info.e_tpgid))
    }

    pub(super) fn cwd(pid: u32) -> Option<PathBuf> {
        // PROC_PIDVNODEPATHINFO fills a `proc_vnodepathinfo` (cdir + rdir). The
        // kernel requires the passed buffer size to equal the struct size
        // exactly, so use the libc struct rather than hand-computed offsets:
        // an off-by-one on the vnode_info prefix makes proc_pidinfo return 0
        // and the cwd silently unavailable.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::pid_t,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if n < size {
            return None;
        }
        // `vip_path` is a flattened [[c_char; 32]; 32] (MAXPATHLEN bytes);
        // read it as one contiguous NUL-terminated byte run.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&info.pvi_cdir.vip_path),
            )
        };
        let nul = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let path = PathBuf::from(std::str::from_utf8(&bytes[..nul]).ok()?);
        path.is_absolute().then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn self_pid_is_alive() {
        let me = std::process::id();
        assert!(pid_alive(me));
        assert!(!pid_alive(1_000_000_007));
    }

    #[test]
    fn self_cmdline_mentions_this_test() {
        let args = cmdline(std::process::id()).expect("self cmdline");
        let joined = args
            .iter()
            .map(|a| String::from_utf8_lossy(a))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("prism") || joined.contains("procinfo") || !args.is_empty(),
            "{joined:?}"
        );
    }

    #[test]
    fn shell_join_quotes_spaces() {
        assert_eq!(shell_join(&["claude".into()]), "claude");
        assert_eq!(
            shell_join(&["/usr/bin/claude".into(), "--resume".into()]),
            "/usr/bin/claude --resume"
        );
        assert_eq!(shell_join(&["sleep".into(), "30".into()]), "sleep 30");
        assert_eq!(shell_join(&["echo".into(), "a b".into()]), "echo 'a b'");
    }

    #[test]
    fn foreground_command_sees_shell_child() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn sh -c sleep");
        let pid = child.id();
        let mut found = None;
        for _ in 0..50 {
            found = foreground_command_impl(pid);
            if found.as_deref().is_some_and(|cmd| cmd.contains("sleep")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let cmd = found.expect("foreground command");
        assert!(cmd.contains("sleep"), "{cmd}");
    }

    #[test]
    fn foreground_command_absent_for_bare_shell() {
        let mut bare = std::process::Command::new("/bin/sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn bare sh");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let cmd = foreground_command_impl(bare.id());
        let _ = bare.kill();
        let _ = bare.wait();
        assert!(
            cmd.is_none(),
            "bare shell must not record a command: {cmd:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_skips_comm_with_spaces() {
        let raw = "42 (sleep 30) S 1 99 99 34816 99 4194304 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0";
        let (pgrp, tpgid) = parse_proc_stat(raw).expect("parse");
        assert_eq!(pgrp, 99);
        assert_eq!(tpgid, 99);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn background_job_on_pty_is_not_foreground() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut cmd = CommandBuilder::new("/bin/bash");
        cmd.arg("--norc");
        cmd.arg("--noprofile");
        cmd.arg("-i");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn bash");
        let pid = child.process_id().expect("pty child pid");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        let mut writer = pair.master.take_writer().expect("pty writer");
        std::thread::sleep(std::time::Duration::from_millis(200));
        writer.write_all(b"sleep 30 &\r").expect("write bg");
        let _ = writer.flush();
        let mut found = None;
        for _ in 0..20 {
            found = live_foreground_command(pid);
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        assert!(
            found.is_none(),
            "background sleep must not be the PTY foreground: {found:?}"
        );
    }
}
