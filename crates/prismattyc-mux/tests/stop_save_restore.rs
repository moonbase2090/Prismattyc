//! `pmux stop --save` keeps the sessions for `pmux up --restore-workspace`.
#![cfg(unix)]

mod support;

use std::path::PathBuf;

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pss-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "run", "data", "state", "config"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn command(&self, args: &[&str]) -> std::process::Output {
        let mut command = prismattyc_process::capture_command(env!("CARGO_BIN_EXE_pmux"));
        support::clear_command_env(&mut command);
        command
            .env_remove(prismattyc_mux::default_space::DEFAULT_SPACE_ENV)
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .args(args);
        prismattyc_process::spawn(&mut command)
            .and_then(std::process::Child::wait_with_output)
            .expect("run pmux")
    }

    fn pmux(&self, args: &[&str]) -> String {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "pmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn lists(&self, name: &str) -> bool {
        self.pmux(&["ls"]).contains(name)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn stop_save_keeps_sessions_for_restore_and_plain_stop_does_not() {
    let home = Home::new("save");
    home.pmux(&["up"]);
    home.pmux(&["new", "--no-attach", "kept", "--", "/bin/sh"]);
    assert!(home.lists("kept"));

    let stopped = home.pmux(&["stop", "--save"]);
    let report: serde_json::Value = serde_json::from_str(stopped.trim()).expect("json report");
    assert_eq!(report["status"], "stopped");
    assert!(
        report["saved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "kept"),
        "{report}"
    );
    // The server is gone.
    assert!(
        home.pmux(&["status"]).contains("status: not running"),
        "server still answers"
    );

    home.pmux(&["up", "--restore-workspace"]);
    assert!(home.lists("kept"), "the saved session came back");

    // --save takes no other arguments.
    let refused = home.command(&["stop", "--save", "kept"]);
    assert!(!refused.status.success());
}
