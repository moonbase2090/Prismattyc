//! Space recovery through the real `pmux` CLI and daemon: stale owners,
//! windowless entries, and saves that would drop Space members.
#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::process::Command;

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("psr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "run", "data", "state", "config"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
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

    fn space_file(&self, name: &str) -> PathBuf {
        let found = std::fs::read_dir(self.spaces().join("instances"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join(format!("{name}.json")))
            .find(|path| path.exists());
        found.unwrap_or_else(|| self.spaces().join(format!("{name}.json")))
    }

    fn listed(&self, name: &str) -> Vec<String> {
        prismattyc_mux::listed_space_sessions(self.space_file(name).parent().unwrap(), name)
            .unwrap()
    }

    fn pmux_output(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
        support::clear_command_env(&mut command);
        command
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .args(args);
        command.output().expect("run pmux")
    }

    fn pmux(&self, args: &[&str]) -> String {
        let output = self.pmux_output(args);
        assert!(
            output.status.success(),
            "pmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.pmux_output(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn add_takes_over_a_session_whose_owner_file_is_gone() {
    let home = Home::new("stale-add");
    home.pmux(&[
        "space",
        "create",
        "alpha",
        "--session-name",
        "sessa",
        "--no-attach",
    ]);
    home.pmux(&[
        "space",
        "create",
        "beta",
        "--session-name",
        "sessb",
        "--no-attach",
    ]);
    std::fs::remove_file(home.space_file("alpha")).unwrap();

    let output = home.pmux_output(&["space", "add", "beta", "--session", "sessa"]);
    assert!(
        output.status.success(),
        "a stale owner must not block add: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("taking it over"),
        "the takeover is reported"
    );
    assert!(home.listed("beta").contains(&"sessa".to_string()));
}

#[test]
fn add_still_refuses_a_session_owned_by_a_saved_space() {
    let home = Home::new("live-add");
    home.pmux(&[
        "space",
        "create",
        "alpha",
        "--session-name",
        "sessa",
        "--no-attach",
    ]);
    home.pmux(&[
        "space",
        "create",
        "beta",
        "--session-name",
        "sessb",
        "--no-attach",
    ]);

    let output = home.pmux_output(&["space", "add", "beta", "--session", "sessa"]);
    assert!(
        !output.status.success(),
        "a live owner must still block add"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already has a Space owner"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn no_argument_save_refuses_to_drop_a_listed_member_that_is_not_live() {
    let home = Home::new("save-drop");
    home.pmux(&[
        "space",
        "create",
        "alpha",
        "--session-name",
        "sessa",
        "--no-attach",
    ]);
    // "ghost" is listed in alpha but has no live session, so a no-argument
    // save from the live window would silently drop it.
    let path = home.space_file("alpha");
    let mut space: prismattyc_mux::SavedSpace =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut ghost = space.sessions[0].clone();
    ghost.name = "ghost".to_string();
    space.sessions.push(ghost);
    std::fs::write(&path, serde_json::to_vec(&space).unwrap()).unwrap();

    let output = home.pmux_output(&["space", "save", "alpha"]);
    assert!(!output.status.success(), "the save must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("ghost"), "{stderr}");
    assert!(home.listed("alpha").contains(&"ghost".to_string()));
}

#[test]
fn remove_drops_a_windowless_entry_by_name() {
    let home = Home::new("remove-broken");
    home.pmux(&[
        "space",
        "create",
        "gamma",
        "--session-name",
        "sessg",
        "--no-attach",
    ]);
    let path = home.space_file("gamma");
    let mut space: prismattyc_mux::SavedSpace =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut broken = space.sessions[0].clone();
    broken.name = "broken".to_string();
    broken.windows.clear();
    space.sessions.push(broken);
    std::fs::write(&path, serde_json::to_vec(&space).unwrap()).unwrap();

    home.pmux(&["space", "remove", "gamma", "--session", "broken"]);
    assert_eq!(home.listed("gamma"), vec!["sessg".to_string()]);
}
