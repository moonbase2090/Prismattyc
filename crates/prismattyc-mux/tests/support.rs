#[cfg(unix)]
use std::path::Path;
use std::process::Command;

use portable_pty::CommandBuilder;

/// Remove ambient mux routing and pane-log settings from a test child.
///
/// It also turns off the default Space, so a test starts from zero Spaces.
/// Tests of the default Space remove that setting again.
pub fn clear_command_env(command: &mut Command) {
    command
        .env("PRISMATTYC_DEFAULT_SPACE", "0")
        .env_remove("PMUX_SOCKET")
        .env_remove("PMUX_PANE_LOG")
        .env_remove("PRISMATTYC_PANE_ID");
}

/// Remove ambient mux routing and pane-log settings from a PTY test child.
pub fn clear_pty_env(command: &mut CommandBuilder) {
    command.env_remove("PMUX_SOCKET");
    command.env_remove("PMUX_PANE_LOG");
}

/// Whether a daemon accepts connections on `socket`.
///
/// pmuxd binds its socket a moment before it listens, so the socket file can
/// exist while a client is still refused. Wait on this, not on the file.
#[cfg(unix)]
pub fn accepts_connections(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_bound_socket_that_is_not_listening_does_not_accept() {
        use std::os::unix::net::{UnixDatagram, UnixListener};
        let dir = std::env::temp_dir().join(format!("pt-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        let bound = dir.join("bound.sock");
        assert!(!accepts_connections(&bound), "no socket file yet");
        // A bound socket file with nothing listening for stream clients: the
        // state between pmuxd's bind and its listen.
        let _bound = UnixDatagram::bind(&bound).expect("bind");
        assert!(bound.exists());
        assert!(
            !accepts_connections(&bound),
            "a socket file alone must not count as a ready daemon"
        );
        let listening = dir.join("listening.sock");
        let _listener = UnixListener::bind(&listening).expect("listen");
        assert!(accepts_connections(&listening));
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::process::Stdio;

    #[test]
    fn clear_command_env_removes_mux_settings() {
        let mut command = prismattyc_process::capture_command("/usr/bin/env");
        command
            .env("PMUX_SOCKET", "/tmp/pt186-sentinel.sock")
            .env("PMUX_PANE_LOG", "/tmp/pt186-sentinel.log")
            .env("PRISMATTYC_PANE_ID", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        clear_command_env(&mut command);
        let output = prismattyc_process::spawn(&mut command)
            .and_then(std::process::Child::wait_with_output)
            .expect("run env");
        assert!(output.status.success());
        let env = String::from_utf8_lossy(&output.stdout);
        assert!(!env.contains("PMUX_SOCKET="), "{env}");
        assert!(!env.contains("PMUX_PANE_LOG="), "{env}");
        assert!(!env.contains("PRISMATTYC_PANE_ID="), "{env}");
    }

    #[test]
    fn clear_pty_env_removes_mux_settings() {
        let mut command = CommandBuilder::new("/usr/bin/env");
        command.env("PMUX_SOCKET", "/tmp/pt186-sentinel.sock");
        command.env("PMUX_PANE_LOG", "/tmp/pt186-sentinel.log");
        clear_pty_env(&mut command);
        assert!(command.get_env("PMUX_SOCKET").is_none());
        assert!(command.get_env("PMUX_PANE_LOG").is_none());
    }
}
