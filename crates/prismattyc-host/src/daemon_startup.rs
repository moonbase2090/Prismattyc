use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;

pub(crate) fn ensure_pmuxd_started(pmux: &Path, socket: &Path) -> Result<()> {
    let output = prismattyc_process::spawn(
        prismattyc_mux::platform::hidden_capture_command(pmux)
            .arg("up")
            .env("PMUX_SOCKET", socket)
            .stdin(Stdio::null()),
    )
    .and_then(std::process::Child::wait_with_output)
    .with_context(|| format!("start pmuxd using {}", pmux.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = match (stderr.is_empty(), stdout.is_empty()) {
            (false, false) => format!("{stderr}\n{stdout}"),
            (false, true) => stderr,
            (true, false) => stdout,
            (true, true) => format!("exited with {}", output.status),
        };
        anyhow::bail!("pmuxd startup failed: {detail}");
    }
    prismattyc_mux::default_space::ensure(
        pmux,
        socket,
        &prismattyc_mux::spaces_dir_for_socket(socket),
        Some("default"),
    )
    .context("ensure default Space")?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum StartMode {
    /// Sessions saved by Save and stop come back with the server.
    Restore,
    Plain,
}

/// A saved workspace that exists but cannot be read stops the start: starting
/// without it would look like a clean start with every session gone.
fn start_mode(saved: &Path) -> Result<StartMode> {
    match prismattyc_mux::workspace::load_from(saved) {
        Ok(Some(_)) => Ok(StartMode::Restore),
        Ok(None) => Ok(StartMode::Plain),
        Err(error) => Err(error.context(format!(
            "saved sessions could not be read, so nothing was started; run `pmux up` to \
             start without them, or fix or move {}",
            saved.display()
        ))),
    }
}

/// Start the server from the app (the card, Update and restart, the palette),
/// restoring the sessions a Save and stop recorded.
pub(crate) fn start_server(pmux: &Path, socket: &Path) -> Result<()> {
    start_server_with(pmux, socket, &prismattyc_mux::workspace::path(socket))
}

fn start_server_with(pmux: &Path, socket: &Path, saved: &Path) -> Result<()> {
    match start_mode(saved)? {
        StartMode::Restore => start_restoring(pmux, socket),
        StartMode::Plain => ensure_pmuxd_started(pmux, socket),
    }
}

/// `pmux up --restore-workspace`: start the server with the sessions a
/// Save and stop recorded.
fn start_restoring(pmux: &Path, socket: &Path) -> Result<()> {
    let output = prismattyc_process::spawn(
        prismattyc_mux::platform::hidden_capture_command(pmux)
            .args(["up", "--restore-workspace"])
            .env("PMUX_SOCKET", socket)
            .stdin(Stdio::null()),
    )
    .and_then(std::process::Child::wait_with_output)
    .with_context(|| format!("start pmuxd using {}", pmux.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "pmuxd startup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn an_unreadable_saved_workspace_stops_the_start_with_its_reason() {
        let root = std::env::temp_dir().join(format!(
            "pds-start-mode-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("pmux.sock");
        let saved = root.join("workspace.json");

        assert_eq!(start_mode(&saved).unwrap(), StartMode::Plain);

        std::fs::write(&saved, b"{ not a workspace").unwrap();
        let error = format!("{:#}", start_mode(&saved).unwrap_err());
        assert!(error.contains("could not be read"), "{error}");
        assert!(error.contains("parse saved workspace"), "{error}");
        // The app reports it and starts nothing: with no pmux to run, the
        // error is still the unreadable file, not a failed spawn.
        let error = format!(
            "{:#}",
            start_server_with(&root.join("no-such-pmux"), &socket, &saved).unwrap_err()
        );
        assert!(error.contains("could not be read"), "{error}");
        assert!(!error.contains("start pmuxd using"), "{error}");

        std::fs::write(&saved, br#"{"version": 2}"#).unwrap();
        assert!(
            start_mode(&saved).is_err(),
            "unknown versions are not dropped"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct PrivateMux {
        pmux: std::path::PathBuf,
        socket: std::path::PathBuf,
        spaces: std::path::PathBuf,
        root: std::path::PathBuf,
    }

    impl Drop for PrivateMux {
        fn drop(&mut self) {
            let _ = prismattyc_process::spawn(
                prismattyc_process::capture_command(&self.pmux)
                    .arg("stop")
                    .env("PMUX_SOCKET", &self.socket)
                    .stdin(Stdio::null()),
            )
            .and_then(std::process::Child::wait_with_output);
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.spaces);
        }
    }

    #[test]
    fn starts_private_pmuxd_when_missing_and_waits_until_ready() {
        let binaries = crate::test_support::mux_bin_dir();
        let pmux = binaries.join(prismattyc_mux::platform::executable_name("pmux"));
        let root = std::env::temp_dir().join(format!(
            "prismattyc-daemon-startup-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        prismattyc_mux::platform::secure_directory(&root).unwrap();
        let socket = root.join("pmux.sock");
        let spaces = prismattyc_mux::spaces_dir_for_socket(&socket);
        let private_mux = PrivateMux {
            pmux: pmux.clone(),
            socket,
            spaces: spaces.clone(),
            root,
        };

        ensure_pmuxd_started(&private_mux.pmux, &private_mux.socket).unwrap();
        assert!(prismattyc_mux::list_spaces(&spaces)
            .unwrap()
            .iter()
            .any(|entry| entry.name == "default"));

        std::fs::remove_dir_all(&spaces).unwrap();
        assert!(prismattyc_mux::list_spaces(&spaces).unwrap().is_empty());
        ensure_pmuxd_started(&private_mux.pmux, &private_mux.socket).unwrap();

        let status = prismattyc_process::spawn(
            prismattyc_process::capture_command(&private_mux.pmux)
                .args(["status"])
                .env("PMUX_SOCKET", &private_mux.socket),
        )
        .and_then(std::process::Child::wait_with_output)
        .unwrap();
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout).contains("status: running"),
            "pmuxd did not report ready: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        let spaces_output = prismattyc_process::spawn(
            prismattyc_process::capture_command(&private_mux.pmux)
                .args(["space", "ls"])
                .env("PMUX_SOCKET", &private_mux.socket),
        )
        .and_then(std::process::Child::wait_with_output)
        .unwrap();
        assert!(spaces_output.status.success());
        assert!(
            String::from_utf8_lossy(&spaces_output.stdout).contains("default"),
            "default Space was not recreated: {}",
            String::from_utf8_lossy(&spaces_output.stdout)
        );
    }
}
