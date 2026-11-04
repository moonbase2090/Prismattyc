//! `pmux space monitor` through the real CLI and daemon: it creates the
//! Activity Monitor Space once, starts the monitor in its session, and only
//! starts it again when the session's shell is idle.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("psm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "run", "data", "state", "config", "launcher dir"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        // A stand-in monitor in a directory with a space, so the typed command
        // must be quoted. Each start appends a line, then it waits for `q`.
        let launcher = root.join("launcher dir/monitor");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\necho started >> '{}'\nexec head -n 1\n",
                root.join("starts").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
        support::clear_command_env(&mut command);
        command
            .env("HOME", self.root.join("home"))
            .env("SHELL", "/bin/sh")
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env(
                "PMUX_ACTIVITY_MONITOR",
                self.root.join("launcher dir/monitor"),
            )
            .args(args);
        command.output().expect("run pmux")
    }

    fn pmux(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "pmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn starts(&self) -> usize {
        std::fs::read_to_string(self.root.join("starts"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn wait_for_starts(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.starts() < count {
            assert!(Instant::now() < deadline, "monitor start {count} never ran");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.output(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn space_row(home: &Home) -> String {
    home.pmux(&["space", "ls"])
        .lines()
        .find(|line| line.starts_with("Activity Monitor "))
        .unwrap_or_default()
        .to_string()
}

/// The pane id of the Activity Monitor session's only pane, from `pmux ls`.
fn monitor_pane(home: &Home) -> String {
    let listing = home.pmux(&["ls"]);
    let mut lines = listing
        .lines()
        .skip_while(|line| !line.starts_with("session Activity Monitor "));
    lines
        .find_map(|line| line.trim_start().strip_prefix("pane "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no Activity Monitor pane in {listing}"))
        .to_string()
}

#[test]
fn monitor_creates_the_space_once_and_restarts_the_monitor_only_when_idle() {
    let home = Home::new("flow");
    let first = home.pmux(&["space", "monitor"]);
    assert!(first.contains("started the monitor"), "{first}");
    home.wait_for_starts(1);
    assert!(
        space_row(&home).contains(" 1 session "),
        "{}",
        space_row(&home)
    );

    // Asked again while the monitor runs: same Space, no second monitor.
    let again = home.pmux(&["space", "monitor"]);
    assert!(again.contains("the monitor is running"), "{again}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(home.starts(), 1);
    assert!(
        space_row(&home).contains(" 1 session "),
        "{}",
        space_row(&home)
    );

    // Quit the monitor; its shell is idle, so the next request starts it.
    let pane = monitor_pane(&home);
    home.pmux(&["send", &pane, "q", "--enter"]);
    wait_until("the monitor to exit", || {
        home.pmux(&["space", "monitor"])
            .contains("started the monitor")
    });
    home.wait_for_starts(2);
    assert!(Path::new(&home.root).exists());
}
