//! What one pmux server hands to the next in a live update.
//!
//! In a live update the server execs the new server binary in its own process.
//! Each pane's PTY master stays open across the exec, so the programs in the
//! panes keep running and stay children of the same process. This record
//! carries what lives only in the old server's memory: each pane's descriptor
//! number and program, its screen and recent output, and its program resume
//! state. It is versioned so a new server can refuse a record it does not
//! understand, and the old server falls back to a normal restart.
//!
//! On Windows a pseudoconsole cannot cross an exec, so each pane's console
//! and program live in a pane holder (`crate::pty_holder`). The old server
//! starts the new one as a separate process with the record in a user-only
//! file, and the record names each pane's holder instead of a descriptor.

#[cfg(any(unix, windows))]
use std::collections::BTreeMap;
#[cfg(any(unix, windows))]
use std::path::PathBuf;

#[cfg(any(unix, windows))]
use prismattyc_emulator::EmulatorStateV1;
use serde::{Deserialize, Serialize};

#[cfg(any(unix, windows))]
use crate::pane_log::PaneLogFrame;
#[cfg(any(unix, windows))]
use crate::program_resume::ResumeRecord;
#[cfg(any(unix, windows))]
use crate::remote_size::SizeOwner;

/// The record format this build writes and reads. Windows records name pane
/// holders instead of descriptors, so they have their own number and a
/// probe never mistakes one platform's server for the other's.
#[cfg(not(windows))]
pub const HANDOFF_FORMAT: u32 = 1;
#[cfg(windows)]
pub const HANDOFF_FORMAT: u32 = 2;

/// How a record reaches a pane: its PTY master's descriptor on unix, its
/// pane holder on Windows.
#[cfg(unix)]
pub(crate) type PaneTransport = i32;
#[cfg(windows)]
pub(crate) type PaneTransport = crate::pty_holder::HolderEndpoint;

/// A pane the old server gave up: the record, plus on unix the master it
/// keeps open across the exec.
#[cfg(unix)]
pub(crate) type KeptPane = (PaneHandoff, std::os::fd::OwnedFd);
#[cfg(windows)]
pub(crate) type KeptPane = PaneHandoff;

/// How long a Windows holder keeps its program waiting for the next server.
#[cfg(windows)]
pub(crate) const HANDOFF_GRACE_MS: u32 = 30_000;

/// One pane, as the old server leaves it.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PaneHandoff {
    pub pane_id: u64,
    /// The PTY master's descriptor number, kept open across the exec.
    #[cfg(unix)]
    pub fd: i32,
    /// The pane holder that owns the pane's console and program.
    #[cfg(windows)]
    pub holder: crate::pty_holder::HolderEndpoint,
    /// The pane's program. A child of the server process.
    pub pid: Option<u32>,
    /// The program already ended and was reaped: the pane shows its last
    /// screen, and `pid` must never be signalled.
    pub exited: bool,
    pub cols: u16,
    pub rows: u16,
    pub outer_rows: u16,
    pub cell_px: (u32, u32),
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_cell: Option<String>,
    /// The screen at `snapshot_seq`, when the emulator was at a point it can
    /// export (not in the middle of an escape sequence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<EmulatorStateV1>,
    pub snapshot_seq: u64,
    /// The pane's output log. Frames after `snapshot_seq` are replayed onto
    /// the snapshot; all of them stay available to clients that catch up.
    pub tail: Vec<PaneLogFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_owner: Option<SizeOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_record: Option<ResumeRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resume_times: BTreeMap<u32, u64>,
}

/// pmuxd prints a [`ProbeReply`] as JSON and exits when started with this
/// before any `--`.
pub const PROBE_ARG: &str = "--handoff-probe";
/// The socket the probe names: under `/dev/null`, so no server can start on it.
pub const PROBE_SOCKET: &str = "/dev/null/pmuxd-probe.sock";
/// pmuxd reads a [`HandoffRecord`] from this descriptor and adopts its panes.
pub const FD_ARG: &str = "--handoff-fd";

/// What a pmuxd binary can take over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReply {
    pub package_version: String,
    pub handoff_formats: Vec<u32>,
}

impl ProbeReply {
    pub fn current() -> Self {
        Self {
            package_version: env!("CARGO_PKG_VERSION").to_string(),
            handoff_formats: vec![HANDOFF_FORMAT],
        }
    }
}

/// Ask `binary` which records it reads. Fails when it does not answer in time
/// or answers with something else, as an older pmuxd without live updates does.
pub fn probe(binary: &std::path::Path) -> std::io::Result<ProbeReply> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let mut command = prismattyc_process::capture_command(binary);
    // An older pmuxd takes an unknown flag for the first pane's program and
    // starts a server. Give it a socket it cannot create, so it fails at once
    // and starts nothing.
    command
        .args(["--socket", PROBE_SOCKET])
        .arg(PROBE_ARG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = prismattyc_process::spawn(&mut command)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the new pmuxd did not answer the live-update probe",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout.take(64 * 1024).read_to_end(&mut out)?;
    }
    if !status.success() {
        return Err(std::io::Error::other(
            "the new pmuxd does not support live updates",
        ));
    }
    serde_json::from_slice(&out).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unreadable live-update probe answer: {error}"),
        )
    })
}

/// Everything one pmux server hands to the next.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRecord {
    pub format: u32,
    /// The package version of the server that wrote the record.
    pub from_version: String,
    /// The listening control socket, kept open so clients queue in its
    /// backlog instead of finding no server.
    #[cfg(unix)]
    pub listener_fd: i32,
    /// The instance lock. Its flock stays held across the exec.
    #[cfg(unix)]
    pub lock_fd: i32,
    /// Sessions, windows, panes, ids and launch specs.
    pub workspace: crate::workspace::Workspace,
    pub panes: Vec<PaneHandoff>,
}

/// Write `record` to an unlinked private file and return its descriptor,
/// positioned at the start. Nothing is left on disk: the record holds program
/// resume tokens.
#[cfg(unix)]
pub(crate) fn write_record(
    dir: &std::path::Path,
    record: &HandoffRecord,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::io::{Seek, Write};

    let path = dir.join(format!(".pmuxd-handoff-{}", std::process::id()));
    let mut file = crate::private_fs::open(
        &path,
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true),
    );
    if let Err(error) = &file {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            // A leftover from a failed update of this same process.
            std::fs::remove_file(&path)?;
            file = crate::private_fs::open(
                &path,
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true),
            );
        }
    }
    let mut file = file?;
    std::fs::remove_file(&path)?;
    serde_json::to_writer(&mut file, record).map_err(std::io::Error::other)?;
    file.flush()?;
    file.rewind()?;
    Ok(file.into())
}

/// Read and check the record a previous server left on `fd`.
#[cfg(unix)]
pub(crate) fn read_record(fd: std::os::fd::OwnedFd) -> std::io::Result<HandoffRecord> {
    let file = std::fs::File::from(fd);
    let record: HandoffRecord = serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if record.format != HANDOFF_FORMAT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "live-update record format {} is not {HANDOFF_FORMAT}",
                record.format
            ),
        ));
    }
    Ok(record)
}

/// Keep `fd` open across the coming exec.
#[cfg(unix)]
pub(crate) fn keep_across_exec(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD only read and change this descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Take ownership of a descriptor the previous server kept across its exec,
/// and make it close-on-exec again.
///
/// # Safety
/// `fd` must be open and owned by nothing else in this process.
#[cfg(unix)]
pub(crate) unsafe fn inherit(fd: std::os::fd::RawFd) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let owned = std::os::fd::OwnedFd::from_raw_fd(fd);
    if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(owned)
}

/// How long the old server waits for every pane's reader and writer to stop.
#[cfg(any(unix, windows))]
pub(crate) const QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Panes an accepted live update gave up, waiting for the next server.
#[cfg(any(unix, windows))]
pub struct PreparedHandoff {
    pub(crate) binary: std::path::PathBuf,
    pub(crate) workspace: crate::workspace::Workspace,
    pub(crate) panes: Vec<KeptPane>,
}

/// Exec the new pmuxd in this process with every pane, the listening socket
/// and the instance lock kept open. `args` are this server's own arguments;
/// any earlier record argument is replaced. Returns only on failure, and by
/// then the panes cannot come back to this server: the caller exits, the
/// programs get a hangup, and the saved workspace restores them as after a
/// normal restart.
#[cfg(unix)]
pub fn exec(
    prepared: PreparedHandoff,
    listener: std::os::fd::OwnedFd,
    lock: &crate::daemon_lock::DaemonLock,
    socket: &std::path::Path,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::io::Error {
    use std::os::fd::AsRawFd;

    let result = (|| -> std::io::Result<std::io::Error> {
        // The fallback if the new server cannot start.
        if let Err(error) = prepared.workspace.write(socket) {
            eprintln!("pmuxd: workspace not saved before the live update: {error:#}");
        }
        let record = HandoffRecord {
            format: HANDOFF_FORMAT,
            from_version: env!("CARGO_PKG_VERSION").to_string(),
            listener_fd: listener.as_raw_fd(),
            lock_fd: lock.as_raw_fd(),
            workspace: prepared.workspace,
            panes: prepared
                .panes
                .iter()
                .map(|(pane, _)| pane.clone())
                .collect(),
        };
        let dir = socket.parent().unwrap_or(std::path::Path::new("/tmp"));
        let record_fd = write_record(dir, &record)?;
        keep_across_exec(record_fd.as_raw_fd())?;
        keep_across_exec(listener.as_raw_fd())?;
        keep_across_exec(lock.as_raw_fd())?;
        for (_, master) in &prepared.panes {
            keep_across_exec(master.as_raw_fd())?;
        }
        let mut command = std::process::Command::new(&prepared.binary);
        command.args(handoff_args(args, record_fd.as_raw_fd()));
        Ok(prismattyc_process::exec(&mut command))
    })();
    match result {
        Ok(error) | Err(error) => error,
    }
}

/// The new server's arguments: the record descriptor first, then this
/// server's own arguments. Anything after the first positional argument (the
/// first pane's program, with or without `--`) belongs to that program, so
/// the flag must come before it. An earlier record flag, which this function
/// always put first, is dropped.
#[cfg(unix)]
fn handoff_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    record_fd: i32,
) -> Vec<std::ffi::OsString> {
    let mut args = args.into_iter().peekable();
    while args.peek().is_some_and(|arg| arg == FD_ARG) {
        args.next();
        args.next();
    }
    let mut out: Vec<std::ffi::OsString> = vec![FD_ARG.into(), record_fd.to_string().into()];
    out.extend(args);
    out
}

/// What a server started by a live update takes over.
#[cfg(unix)]
pub struct Adopted {
    pub plane: crate::control::ControlPlane,
    pub lock: crate::daemon_lock::DaemonLock,
    /// Bind it with [`crate::control::ControlServer::bind_inherited`].
    pub listener: std::os::unix::net::UnixListener,
    /// Resume commands for panes that had to start again.
    pub resumes: Vec<(u64, String)>,
    /// The version of the server that handed over.
    pub from_version: String,
}

/// Read the record on `fd` and take over its panes, lock and socket.
#[cfg(unix)]
pub fn adopt(fd: i32, socket: &std::path::Path) -> anyhow::Result<Adopted> {
    use anyhow::Context;

    // SAFETY: the previous server passed this descriptor for us alone.
    let record = read_record(unsafe { inherit(fd) }.context("live-update record descriptor")?)
        .context("read the live-update record")?;
    // SAFETY: each descriptor below was kept across the exec for this server.
    let lock = crate::daemon_lock::DaemonLock::inherited(
        unsafe { inherit(record.lock_fd) }.context("instance lock descriptor")?,
    );
    let listener = std::os::unix::net::UnixListener::from(
        unsafe { inherit(record.listener_fd) }.context("control socket descriptor")?,
    );
    let mut panes = Vec::with_capacity(record.panes.len());
    for pane in record.panes {
        match unsafe { inherit(pane.fd) } {
            Ok(master) => panes.push((pane, master)),
            Err(error) => eprintln!("pmuxd: pane {} PTY was not kept: {error}", pane.pane_id),
        }
    }
    let workspace = record.workspace;
    let domain = crate::domain::Domain::from_workspace(&workspace)?;
    let bounds = workspace
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .map(|window| window.bounds);
    let spawns = workspace
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .map(|pane| (pane.id, pane.spawn.clone()));
    let (mut plane, respawned) =
        crate::control::ControlPlane::new_adopted(domain, bounds, spawns, socket.to_owned(), panes)
            .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let policy = crate::load_mux_section(&crate::prism_config_path())
        .ok()
        .and_then(|file| file.space_open_runs_commands)
        .unwrap_or_default();
    let resumes = workspace.resume_commands(&mut plane, policy, |id| respawned.contains(&id));
    Ok(Adopted {
        plane,
        lock,
        listener,
        resumes,
        from_version: record.from_version,
    })
}

/// The new Windows server's argument naming the record file.
pub const FILE_ARG: &str = "--handoff-file";

/// Write the record next to the socket, readable only by this user. It holds
/// each holder's token, so the new server deletes it once read.
#[cfg(windows)]
pub(crate) fn write_record_file(
    dir: &std::path::Path,
    record: &HandoffRecord,
) -> std::io::Result<PathBuf> {
    let path = dir.join(format!("pmuxd-handoff-{}.json", std::process::id()));
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    crate::private_fs::write(&path, &bytes)?;
    Ok(path)
}

/// Read and check the record a previous server left at `path`, then delete it.
#[cfg(windows)]
pub(crate) fn read_record_file(path: &std::path::Path) -> std::io::Result<HandoffRecord> {
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    let record: HandoffRecord = serde_json::from_slice(&bytes?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if record.format != HANDOFF_FORMAT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "live-update record format {} is not {HANDOFF_FORMAT}",
                record.format
            ),
        ));
    }
    Ok(record)
}

/// The new Windows server's arguments: the record file first, then this
/// server's own arguments, without an earlier record argument.
#[cfg(any(windows, test))]
pub(crate) fn successor_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    record: &std::path::Path,
) -> Vec<std::ffi::OsString> {
    let mut args = args.into_iter().peekable();
    while args.peek().is_some_and(|arg| arg == FILE_ARG) {
        args.next();
        args.next();
    }
    let mut out: Vec<std::ffi::OsString> = vec![FILE_ARG.into(), record.as_os_str().to_owned()];
    out.extend(args);
    out
}

/// Start the new pmuxd for a Windows live update and return once it runs.
/// The holders already wait for it ([`HANDOFF_GRACE_MS`]); the caller then
/// exits, releasing the instance lock and the socket for the new server.
#[cfg(windows)]
pub fn start_successor(
    prepared: PreparedHandoff,
    socket: &std::path::Path,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::io::Result<()> {
    // The fallback if the new server cannot start.
    if let Err(error) = prepared.workspace.write(socket) {
        eprintln!("pmuxd: workspace not saved before the live update: {error:#}");
    }
    let record = HandoffRecord {
        format: HANDOFF_FORMAT,
        from_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: prepared.workspace,
        panes: prepared.panes,
    };
    let dir = socket
        .parent()
        .ok_or_else(|| std::io::Error::other("socket has no directory"))?;
    let path = write_record_file(dir, &record)?;
    let mut command = std::process::Command::new(&prepared.binary);
    command.args(successor_args(args, &path));
    match crate::platform::spawn_detached(&mut command) {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(error)
        }
    }
}

/// What a Windows server started by a live update takes over. It takes the
/// instance lock and binds the socket itself once the old server exits.
#[cfg(windows)]
pub struct Adopted {
    pub plane: crate::control::ControlPlane,
    /// Resume commands for panes that had to start again.
    pub resumes: Vec<(u64, String)>,
    /// The version of the server that handed over.
    pub from_version: String,
}

/// Read the record at `path` and reconnect to its panes' holders.
#[cfg(windows)]
pub fn adopt_file(path: &std::path::Path, socket: &std::path::Path) -> anyhow::Result<Adopted> {
    use anyhow::Context;

    let record = read_record_file(path).context("read the live-update record")?;
    let workspace = record.workspace;
    let domain = crate::domain::Domain::from_workspace(&workspace)?;
    let bounds = workspace
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .map(|window| window.bounds);
    let spawns = workspace
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter())
        .flat_map(|window| window.panes.iter())
        .map(|pane| (pane.id, pane.spawn.clone()));
    let (mut plane, respawned) = crate::control::ControlPlane::new_adopted(
        domain,
        bounds,
        spawns,
        socket.to_owned(),
        record.panes,
    )
    .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let policy = crate::load_mux_section(&crate::prism_config_path())
        .ok()
        .and_then(|file| file.space_open_runs_commands)
        .unwrap_or_default();
    let resumes = workspace.resume_commands(&mut plane, policy, |id| respawned.contains(&id));
    Ok(Adopted {
        plane,
        resumes,
        from_version: record.from_version,
    })
}

#[cfg(test)]
mod successor_tests {
    use super::*;
    use std::ffi::OsString;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_record_file_comes_first_and_an_earlier_one_is_dropped() {
        let record = std::path::Path::new("C:/run/pmuxd-handoff-7.json");
        assert_eq!(
            successor_args(os(&["--socket", "S", "pwsh"]), record),
            os(&[
                FILE_ARG,
                "C:/run/pmuxd-handoff-7.json",
                "--socket",
                "S",
                "pwsh"
            ])
        );
        assert_eq!(
            successor_args(
                os(&[
                    FILE_ARG,
                    "C:/run/old.json",
                    "--socket",
                    "S",
                    "--",
                    "pwsh",
                    FILE_ARG
                ]),
                record
            ),
            os(&[
                FILE_ARG,
                "C:/run/pmuxd-handoff-7.json",
                "--socket",
                "S",
                "--",
                "pwsh",
                FILE_ARG
            ]),
            "only the leading record argument is ours; the program's own stays"
        );
    }
}

#[cfg(all(test, windows))]
mod record_file_tests {
    use super::*;

    fn record(format: u32) -> HandoffRecord {
        HandoffRecord {
            format,
            from_version: "9.9.9".into(),
            workspace: serde_json::from_value(serde_json::json!({
                "version": 1,
                "next_ids": {"session": 1, "window": 1, "pane": 1},
                "sessions": [],
                "view": null,
            }))
            .unwrap(),
            panes: Vec::new(),
        }
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_record_file_is_read_once_and_then_gone() {
        let dir = scratch_dir("handoff-record-once");
        let path = write_record_file(&dir, &record(HANDOFF_FORMAT)).unwrap();
        let read = read_record_file(&path).unwrap();
        assert_eq!(read.from_version, "9.9.9");
        assert!(
            !path.exists(),
            "the record holds holder tokens and must not stay"
        );
        assert!(read_record_file(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_of_another_format_is_refused() {
        let dir = scratch_dir("handoff-record-format");
        let path = write_record_file(&dir, &record(HANDOFF_FORMAT + 1)).unwrap();
        let error = read_record_file(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(Into::into).collect()
    }

    /// An older pmuxd takes the probe flag for a program and starts a server
    /// on the socket it is given. The probe's socket cannot be created, so
    /// such a binary fails without starting anything.
    #[test]
    fn probing_an_older_pmuxd_starts_no_server() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("handoff-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("started");
        let old = dir.join("pmuxd-old");
        std::fs::write(
            &old,
            format!(
                "#!/bin/sh\nsocket=\nwhile [ $# -gt 0 ]; do [ \"$1\" = --socket ] && socket=$2; shift; done\n\
                 mkdir -p \"$(dirname \"${{socket:-/tmp/x}}\")\" 2>/dev/null || exit 1\n\
                 touch '{}'\nsleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = probe(&old).expect_err("an older pmuxd must not pass the probe");
        assert!(
            error.to_string().contains("does not support live updates"),
            "{error}"
        );
        assert!(!marker.exists(), "the probe started an older server");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_record_flag_goes_before_the_first_pane_command() {
        // With --, and with a positional program and its own arguments.
        assert_eq!(
            handoff_args(strings(&["--socket", "/s", "--", "/bin/sh", "-c", "x"]), 9),
            strings(&[
                "--handoff-fd",
                "9",
                "--socket",
                "/s",
                "--",
                "/bin/sh",
                "-c",
                "x"
            ])
        );
        assert_eq!(
            handoff_args(strings(&["--socket", "/s", "/bin/sh", "-l"]), 9),
            strings(&["--handoff-fd", "9", "--socket", "/s", "/bin/sh", "-l"])
        );
        assert_eq!(
            handoff_args(strings(&["--socket", "/s"]), 9),
            strings(&["--handoff-fd", "9", "--socket", "/s"])
        );
    }

    #[test]
    fn an_earlier_record_flag_is_replaced_but_a_command_argument_is_kept() {
        assert_eq!(
            handoff_args(
                strings(&[
                    "--handoff-fd",
                    "4",
                    "--socket",
                    "/s",
                    "prog",
                    "--handoff-fd",
                    "x"
                ]),
                9
            ),
            strings(&[
                "--handoff-fd",
                "9",
                "--socket",
                "/s",
                "prog",
                "--handoff-fd",
                "x"
            ])
        );
    }
}
