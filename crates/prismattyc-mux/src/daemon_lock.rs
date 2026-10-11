//! Hold the instance lock before starting any PTY children.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub struct DaemonLock(File);

impl DaemonLock {
    pub fn acquire(socket: &Path) -> io::Result<Self> {
        if let Some(directory) = socket.parent() {
            crate::private_fs::create_dir(directory)?;
            repair_runtime_files(socket)?;
            #[cfg(windows)]
            crate::platform::require_private_directory(directory)?;
        }
        // Keep the inode after exit. Unlinking a lock lets a waiter and a
        // new opener acquire different files for the same instance.
        let mut file = crate::private_fs::open(
            &socket.with_extension("lock"),
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true),
        )?;
        crate::platform::try_lock_exclusive(&file)?;
        if matches!(
            crate::probe_socket_liveness(socket),
            crate::SocketLiveness::Live | crate::SocketLiveness::Foreign
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "pmuxd is already running",
            ));
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self(file))
    }

    /// A starter can own the instance before its socket is ready. Callers
    /// must wait for that owner instead of reporting their losing child as
    /// a failed startup. The probe never creates or removes the lock file.
    pub fn is_held(socket: &Path) -> io::Result<bool> {
        let file = match File::options()
            .read(true)
            .write(true)
            .open(socket.with_extension("lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match crate::platform::try_lock_exclusive(&file) {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub fn publish_pid(&self, socket: &Path) -> io::Result<()> {
        let _ = &self.0;
        crate::private_fs::write(
            socket.with_extension("pid"),
            format!("{}\n", std::process::id()),
        )
    }
}

fn repair_runtime_files(socket: &Path) -> io::Result<()> {
    for extension in [
        "log",
        "pid",
        "restart.log",
        "host.pid",
        "host.pid.lock",
        "host.render.json",
        "attach-tabs.json",
        "login.lock",
        "stopped",
    ] {
        crate::private_fs::repair_if_exists(&socket.with_extension(extension))?;
    }
    crate::private_fs::repair_if_exists(&crate::spaces_daemon_identity_path(socket))
}
