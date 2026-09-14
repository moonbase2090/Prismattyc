use std::process::Command;

use portable_pty::CommandBuilder;

/// Remove ambient mux routing and pane-log settings from a test child.
pub fn clear_command_env(command: &mut Command) {
    command
        .env_remove("PMUX_SOCKET")
        .env_remove("PMUX_PANE_LOG")
        .env_remove("PRISMATTYC_PANE_ID");
}

/// Remove ambient mux routing and pane-log settings from a PTY test child.
pub fn clear_pty_env(command: &mut CommandBuilder) {
    command.env_remove("PMUX_SOCKET");
    command.env_remove("PMUX_PANE_LOG");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn clear_command_env_removes_mux_settings() {
        let mut command = Command::new("/usr/bin/env");
        command
            .env("PMUX_SOCKET", "/tmp/pt186-sentinel.sock")
            .env("PMUX_PANE_LOG", "/tmp/pt186-sentinel.log")
            .env("PRISMATTYC_PANE_ID", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        clear_command_env(&mut command);
        let output = command.output().expect("run env");
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
