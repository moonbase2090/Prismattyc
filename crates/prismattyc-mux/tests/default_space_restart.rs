//! A fresh install gets one default Space, and restarts never add another.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "run", "data", "state", "config"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        Self { root }
    }

    fn spaces(&self) -> PathBuf {
        self.root.join("data/prismattyc/spaces")
    }

    fn pmux(&self, args: &[&str]) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
        support::clear_command_env(&mut command);
        command
            .env_remove(prismattyc_mux::default_space::DEFAULT_SPACE_ENV)
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .args(args);
        let output = command.output().expect("run pmux");
        assert!(
            output.status.success(),
            "pmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn restart(&self) {
        self.pmux(&["stop"]);
        self.pmux(&["up"]);
    }

    fn space_names(&self) -> Vec<String> {
        prismattyc_mux::list_spaces(&self.spaces())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_pmux"))
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("PMUX_SOCKET")
            .arg("stop")
            .output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session_count(path: &Path) -> usize {
    prismattyc_mux::load_space(path.parent().unwrap(), "default")
        .unwrap()
        .sessions
        .len()
}

#[test]
fn first_start_creates_one_default_space_and_restarts_keep_it_single() {
    let home = Home::new("first");
    assert!(!home.spaces().exists());

    home.pmux(&["up"]);
    assert_eq!(home.space_names(), vec!["default".to_string()]);
    assert_eq!(session_count(&home.spaces().join("default.json")), 1);
    // The Space owns the daemon's first session instead of spawning another.
    let sessions = home.pmux(&["ls"]);
    assert_eq!(sessions.matches("session ").count(), 1, "{sessions}");
    // The bare `space open` that failed on a fresh install now works.
    home.pmux(&["space", "open", "--no-attach"]);

    home.restart();
    assert_eq!(home.space_names(), vec!["default".to_string()]);
}

#[test]
fn restart_recreates_default_only_when_no_space_is_left() {
    let home = Home::new("recreate");
    home.pmux(&["up"]);
    home.pmux(&["stop"]);
    std::fs::remove_file(home.spaces().join("default.json")).unwrap();
    home.pmux(&["up"]);
    assert_eq!(home.space_names(), vec!["default".to_string()]);

    home.pmux(&["stop"]);
    std::fs::rename(
        home.spaces().join("default.json"),
        home.spaces().join("work.json"),
    )
    .unwrap();
    home.pmux(&["up"]);
    assert_eq!(home.space_names(), vec!["work".to_string()]);
}
