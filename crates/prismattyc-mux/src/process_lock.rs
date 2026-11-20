// SPDX-License-Identifier: MIT

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

pub(crate) fn acquire(file: &File, path: &Path, kind: &str) -> io::Result<()> {
    let deadline = Instant::now() + WAIT;
    let mut delay = Duration::from_millis(25);
    loop {
        match crate::platform::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("The {kind} lock is still held by {} after {} seconds. Wait for that process to finish and retry; no process was stopped.", holder(path), WAIT.as_secs()),
                    ));
                }
                std::thread::sleep(delay.min(remaining));
                delay = (delay * 2).min(Duration::from_millis(250));
            }
            Err(error) => return Err(error),
        }
    }
}

fn owner_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".owner");
    PathBuf::from(name)
}

pub(crate) fn record_owner(file: &mut File, path: &Path) -> io::Result<()> {
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "{}", std::process::id())?;
    // Windows denies reads through other handles inside the exclusive range.
    // Keep owner metadata readable without acquiring that range.
    crate::private_fs::write(owner_path(path), format!("{}\n", std::process::id()))
}

pub(crate) fn recorded_pid(path: &Path) -> Option<u32> {
    let read = |path: &Path| {
        let mut text = String::new();
        File::open(path)
            .ok()?
            .take(64)
            .read_to_string(&mut text)
            .ok()?;
        text.trim().parse().ok()
    };
    read(path).or_else(|| read(&owner_path(path)))
}

pub(crate) fn process_label(pid: u32) -> String {
    let command = crate::procinfo::cmdline(pid)
        .and_then(|args| args.into_iter().next())
        .and_then(|name| String::from_utf8(name).ok())
        .and_then(|name| {
            Path::new(&name)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        });
    match command {
        Some(command) => format!("{command} (pid {pid})"),
        None => format!("process pid {pid}"),
    }
}

pub(crate) fn holder(path: &Path) -> String {
    if let Some(pid) = recorded_pid(path).filter(|pid| crate::procinfo::pid_alive(*pid)) {
        return process_label(pid);
    }
    #[cfg(target_os = "macos")]
    if let Ok(pids) = file_users(&[path.to_path_buf()], false) {
        let names: Vec<_> = pids
            .into_iter()
            .filter(|pid| *pid != std::process::id())
            .map(process_label)
            .collect();
        if !names.is_empty() {
            return names.join(", ");
        }
    }
    "an unidentified process".into()
}

#[cfg(target_os = "macos")]
pub(crate) fn file_users(paths: &[std::path::PathBuf], mapped_only: bool) -> io::Result<Vec<u32>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = std::process::Command::new("/usr/sbin/lsof");
    command.args(["-nP", "-Fp"]);
    if mapped_only {
        command.args(["-a", "-d", "txt"]);
    }
    // lsof resolves mapped vnodes after an app rename; proc_pidpath retains
    // the original executable path on macOS.
    let mut child = prismattyc_process::spawn(
        command
            .arg("--")
            .args(paths)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "mapped-process inspection timed out; the previous app was kept",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    interpret_lsof(output.status.code(), &output.stdout, &output.stderr, paths)
}

/// lsof warns about mounts it cannot stat, such as a Time Machine snapshot,
/// even when none of the requested files live there. Those warnings are
/// tolerated. Any other stderr, a warning about a mount that holds a requested
/// path, or an exit status other than 0 or 1 still means the inspection failed.
#[cfg(target_os = "macos")]
fn interpret_lsof(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    paths: &[std::path::PathBuf],
) -> io::Result<Vec<u32>> {
    let blocking = blocking_stderr(stderr, paths);
    if blocking.is_some() || !matches!(code, Some(0 | 1)) {
        return Err(io::Error::other(format!(
            "could not inspect mapped processes: {}",
            blocking.unwrap_or_else(|| String::from_utf8_lossy(stderr).trim().to_string())
        )));
    }
    let mut pids: Vec<_> = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse().ok())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// The first stderr line that is not a warning about an unrelated mount.
#[cfg(target_os = "macos")]
fn blocking_stderr(stderr: &[u8], paths: &[std::path::PathBuf]) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    const WARNING: &[u8] = b"lsof: WARNING: can't stat() ";
    const MARKER: &[u8] = b" file system ";
    // The continuation lines only qualify the warning just above them; on
    // their own they say nothing about which mount was skipped.
    let mut after_unrelated_warning = false;
    for line in stderr.split(|&b| b == b'\n') {
        let text = line.trim_ascii();
        if text.is_empty() {
            continue;
        }
        let continuation = text == b"Output information may be incomplete."
            || (text.starts_with(b"assuming \"dev=") && text.ends_with(b"\" from mount table"));
        if continuation && after_unrelated_warning {
            continue;
        }
        let mount = line.strip_prefix(WARNING).and_then(|rest| {
            let at = rest.windows(MARKER.len()).position(|w| w == MARKER)?;
            Some(&rest[at + MARKER.len()..])
        });
        match mount {
            Some(mount)
                if !paths
                    .iter()
                    .any(|path| path.starts_with(std::ffi::OsStr::from_bytes(mount))) =>
            {
                after_unrelated_warning = true;
            }
            _ => return Some(String::from_utf8_lossy(text).into_owned()),
        }
    }
    None
}

#[cfg(all(test, target_os = "macos"))]
mod lsof_tests {
    use super::interpret_lsof;
    use std::path::PathBuf;

    const SNAPSHOT_WARNING: &[u8] = b"lsof: WARNING: can't stat() apfs file system /Volumes/com.apple.TimeMachine.localsnapshots/Backups.backupdb/Some Mac/2026-11-16-112811/Data\n      Output information may be incomplete.\n      assuming \"dev=3200000d\" from mount table\n";

    fn app() -> Vec<PathBuf> {
        vec![PathBuf::from("/Applications/Demo.app/Contents/MacOS/demo")]
    }

    #[test]
    fn an_unrelated_mount_warning_with_no_user_finds_no_processes() {
        assert_eq!(
            interpret_lsof(Some(1), b"", SNAPSHOT_WARNING, &app()).unwrap(),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn an_unrelated_mount_warning_still_reports_the_mapped_users() {
        let pids = interpret_lsof(Some(0), b"p412\np77\np412\n", SNAPSHOT_WARNING, &app()).unwrap();
        assert_eq!(pids, vec![77, 412]);
    }

    #[test]
    fn a_warning_about_the_mount_holding_the_requested_file_fails() {
        let stderr = b"lsof: WARNING: can't stat() apfs file system /Applications\n      Output information may be incomplete.\n";
        let err = interpret_lsof(Some(1), b"", stderr, &app()).unwrap_err();
        assert!(err.to_string().contains("/Applications"), "{err}");
    }

    #[test]
    fn any_other_stderr_fails_even_beside_an_unrelated_warning() {
        let mut stderr = SNAPSHOT_WARNING.to_vec();
        stderr.extend_from_slice(
            b"lsof: status error on /Applications/Demo.app: Permission denied\n",
        );
        let err = interpret_lsof(Some(1), b"p5\n", &stderr, &app()).unwrap_err();
        assert!(err.to_string().contains("Permission denied"), "{err}");
    }

    #[test]
    fn an_exit_status_other_than_0_or_1_fails_even_with_only_warnings() {
        assert!(interpret_lsof(Some(2), b"", SNAPSHOT_WARNING, &app()).is_err());
        assert!(interpret_lsof(None, b"", b"", &app()).is_err());
    }

    #[test]
    fn a_standalone_incomplete_output_line_fails() {
        let err = interpret_lsof(
            Some(1),
            b"",
            b"      Output information may be incomplete.\n",
            &app(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("incomplete"), "{err}");
    }

    #[test]
    fn a_standalone_assumed_device_line_fails() {
        let stderr = b"      assuming \"dev=3200000d\" from mount table\n";
        assert!(interpret_lsof(Some(1), b"", stderr, &app()).is_err());
    }

    #[test]
    fn a_continuation_line_after_a_blocking_line_fails() {
        let stderr = b"lsof: status error on /Applications/Demo.app: Permission denied\n      Output information may be incomplete.\n";
        let err = interpret_lsof(Some(1), b"", stderr, &app()).unwrap_err();
        assert!(err.to_string().contains("Permission denied"), "{err}");
    }

    #[test]
    fn a_continuation_line_after_a_warning_for_the_requested_mount_fails() {
        let stderr = b"lsof: WARNING: can't stat() apfs file system /Applications\n      Output information may be incomplete.\n";
        assert!(interpret_lsof(Some(1), b"", stderr, &app()).is_err());
    }

    #[test]
    fn clean_output_still_parses() {
        assert_eq!(
            interpret_lsof(Some(0), b"p9\n", b"", &app()).unwrap(),
            vec![9]
        );
    }
}
