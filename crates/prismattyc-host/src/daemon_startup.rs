use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;

pub(crate) fn ensure_pmuxd_started(pmux: &Path, socket: &Path) -> Result<()> {
    let output = prismattyc_mux::platform::hidden_command(pmux)
        .arg("up")
        .env("PMUX_SOCKET", socket)
        .stdin(Stdio::null())
        .output()
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct PrivateMux {
        pmux: std::path::PathBuf,
        socket: std::path::PathBuf,
        root: std::path::PathBuf,
    }

    impl Drop for PrivateMux {
        fn drop(&mut self) {
            let _ = Command::new(&self.pmux)
                .arg("stop")
                .env("PMUX_SOCKET", &self.socket)
                .stdin(Stdio::null())
                .output();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn starts_private_pmuxd_when_missing_and_waits_until_ready() {
        let binaries = crate::test_support::mux_bin_dir();
        let pmux = binaries.join("pmux");
        let root = std::env::temp_dir().join(format!(
            "prismattyc-daemon-startup-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let private_mux = PrivateMux {
            pmux: pmux.clone(),
            socket: root.join("pmux.sock"),
            root,
        };

        ensure_pmuxd_started(&private_mux.pmux, &private_mux.socket).unwrap();
        ensure_pmuxd_started(&private_mux.pmux, &private_mux.socket).unwrap();

        let status = Command::new(&private_mux.pmux)
            .args(["status"])
            .env("PMUX_SOCKET", &private_mux.socket)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stdout).contains("status: running"),
            "pmuxd did not report ready: {}",
            String::from_utf8_lossy(&status.stdout)
        );
    }
}
