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
    let mut child = command
        .arg("--")
        .args(paths)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
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
    if !output.stderr.is_empty() || !matches!(output.status.code(), Some(0 | 1)) {
        return Err(io::Error::other(format!(
            "could not inspect mapped processes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut pids: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse().ok())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}
