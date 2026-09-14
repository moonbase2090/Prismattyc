//! Session naming through the real CLI, daemon, saved Spaces, and mail store.
#![cfg(unix)]

use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, Snapshot,
    PROTOCOL_VERSION,
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
mod support;

struct Fixture {
    dir: PathBuf,
    socket: PathBuf,
    server: Option<Child>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pmux-naming-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = Self {
            socket: dir.join("mux.sock"),
            dir,
            server: None,
        };
        f.start();
        f
    }
    fn command(&self, binary: &str) -> Command {
        let mut cmd = Command::new(binary);
        support::clear_command_env(&mut cmd);
        cmd.env("XDG_DATA_HOME", &self.dir)
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("PMUX_SOCKET", &self.socket)
            .env("PMUX_SESSION_AGENTS", self.dir.join("agents.json"))
            .stdin(Stdio::null());
        cmd
    }
    fn start(&mut self) {
        self.server = Some(
            self.command(env!("CARGO_BIN_EXE_pmuxd"))
                .arg("--socket")
                .arg(&self.socket)
                .args(["--", "/bin/sh", "-c", "exec sleep 999"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&self.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn restart(&mut self) {
        let mut server = self.server.take().unwrap();
        server.kill().unwrap();
        server.wait().unwrap();
        self.start();
    }
    fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = self.command(env!("CARGO_BIN_EXE_pmux"));
        cmd.args(args);
        cmd
    }
    fn ok(&self, args: &[&str]) -> String {
        ok(self.cli(args).output().unwrap())
    }
    fn snapshot(&self) -> Snapshot {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        let req = ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 1,
        };
        writeln!(stream, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: ControlResponse = serde_json::from_str(&line).unwrap();
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => snapshot,
            other => panic!("snapshot: {other:?}"),
        }
    }
    fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(self.dir.join("prismattyc/spaces/work.json")).unwrap(),
        )
        .unwrap()
    }
}
fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn readable_space_sessions_rename_keeps_process_mail_and_old_addresses() {
    let mut f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "add", "work"]);
    let before = f.snapshot();
    let original = before.sessions.iter().find(|s| s.name == "work-1").unwrap();
    assert_eq!(original.agent_id.as_deref(), Some("work-1"));
    assert!(before
        .sessions
        .iter()
        .any(|s| s.name == "work-2" && s.agent_id.as_deref() == Some("work-2")));
    let pane = &original.windows[0].panes[0];
    let held = f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "held letter",
        "--body",
        "keep this body",
    ]);
    let letters: serde_json::Value =
        serde_json::from_str(&f.ok(&["mail", "--as", "work-1", "claim", "--json"])).unwrap();
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "open letter",
    ]);
    f.ok(&[
        "session",
        "name",
        "reviewer",
        "--session",
        &original.id.to_string(),
    ]);
    let after = f.snapshot();
    let renamed = after
        .sessions
        .iter()
        .find(|s| s.name == "reviewer")
        .unwrap();
    assert_eq!(renamed.id, original.id);
    assert_eq!(renamed.agent_id.as_deref(), Some("reviewer"));
    assert_eq!(renamed.space_id, original.space_id);
    assert_eq!(renamed.windows[0].panes[0].id, pane.id);
    assert_eq!(renamed.windows[0].panes[0].child_pid, pane.child_pid);
    assert!(f
        .ok(&["mail", "--as", "reviewer", "inbox"])
        .contains("open: 1 held: 1"));
    let saved = f.saved();
    assert_eq!(saved["sessions"][0]["name"], "reviewer");
    assert_eq!(saved["sessions"][0]["agent"], "reviewer");
    assert_eq!(saved["tabs"][0]["sessions"][0], "reviewer");
    // Already-running children carry the old environment. Seat lookup must win.
    let out = f
        .cli(&["mail", "inbox"])
        .env("PRISMATTYC_PANE_ID", pane.id.to_string())
        .env("PMUX_AGENT", "unrelated-old-env")
        .output()
        .unwrap();
    assert!(ok(out).contains("open: 1 held: 1"));
    // Claim retains exact IDs and content. Previously held IDs still commit.
    let claimed: serde_json::Value =
        serde_json::from_str(&f.ok(&["mail", "--as", "work-1", "claim", "--json"])).unwrap();
    assert_eq!(claimed["letters"][0]["id"], letters["letters"][0]["id"]);
    assert_eq!(claimed["letters"][0]["body"], "keep this body");
    assert!(held.contains(letters["letters"][0]["id"].as_str().unwrap()));
    f.ok(&[
        "mail",
        "--as",
        "work-1",
        "commit",
        letters["letters"][0]["id"].as_str().unwrap(),
    ]);
    f.ok(&[
        "session",
        "rename",
        "reviewer-next",
        "--session",
        "reviewer",
    ]);
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "old address",
    ]);
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "reviewer",
        "--summary",
        "second address",
    ]);
    assert!(f
        .ok(&["mail", "--as", "reviewer-next", "inbox"])
        .contains("open: 2 held: 1"));
    f.restart();
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "after restart",
    ]);
    assert!(f
        .ok(&["mail", "--as", "reviewer-next", "inbox"])
        .contains("open: 3 held: 1"));
    let refused = f.cli(&["new", "--no-attach", "work-1"]).output().unwrap();
    assert!(
        !refused.status.success(),
        "old forwarding address must not be stolen"
    );
}

#[test]
fn bind_existing_unassigned_session_and_reject_name_conflicts_without_changes() {
    let f = Fixture::new();
    f.ok(&[
        "new",
        "--no-attach",
        "--no-agent",
        "legacy",
        "--",
        "/bin/sh",
    ]);
    let snap = f.snapshot();
    let session = snap.sessions.iter().find(|s| s.name == "legacy").unwrap();
    let pane = session.windows[0].panes[0].id;
    ok(f.cli(&["session", "name", "astra-spaces"])
        .env("PRISMATTYC_PANE_ID", pane.to_string())
        .output()
        .unwrap());
    let who = ok(f
        .cli(&["whoami", "--json"])
        .env("PRISMATTYC_PANE_ID", pane.to_string())
        .output()
        .unwrap());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&who).unwrap()["agent"],
        "astra-spaces"
    );
    f.ok(&["new", "--no-attach", "taken-name", "--", "/bin/sh"]);
    for invalid in ["taken-name", "Bad Name", "a", "--bad"] {
        let out = f
            .cli(&["session", "name", invalid, "--session", "astra-spaces"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "invalid name {invalid} succeeded");
        let current = f.snapshot();
        let same = current
            .sessions
            .iter()
            .find(|s| s.id == session.id)
            .unwrap();
        assert_eq!(same.name, "astra-spaces");
        assert_eq!(same.windows[0].panes[0].id, pane);
        assert!(!f
            .dir
            .join("prismattyc/spaces/.session-name-transaction")
            .exists());
    }
}

#[test]
fn mailbox_failure_leaves_the_session_and_saved_space_unchanged() {
    let f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "keep me",
    ]);
    let saved = f.saved();
    let db = rusqlite::Connection::open(f.dir.join("prismattyc/mail.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_rename BEFORE UPDATE OF recipient ON letters BEGIN SELECT RAISE(ABORT, 'fixture disk failure'); END;").unwrap();
    let refused = f
        .cli(&["session", "name", "new-name", "--session", "work-1"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert_eq!(f.saved(), saved);
    assert!(f
        .snapshot()
        .sessions
        .iter()
        .any(|s| s.name == "work-1" && s.agent_id.as_deref() == Some("work-1")));
    assert!(f
        .ok(&["mail", "--as", "work-1", "inbox"])
        .contains("open: 1 held: 0"));
    db.execute_batch("DROP TRIGGER fail_rename;").unwrap();
    f.ok(&["session", "name", "new-name", "--session", "work-1"]);
    // Renaming back must flatten forwards instead of forming a cycle.
    f.ok(&["session", "name", "work-1", "--session", "new-name"]);
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "new-name",
        "--summary",
        "after rename back",
    ]);
    assert!(f
        .ok(&["mail", "--as", "work-1", "inbox"])
        .contains("open: 2 held: 0"));
}

#[test]
fn interrupted_rename_reconciles_saved_references_before_the_next_space_operation() {
    let f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    let snapshot = f.snapshot();
    let old = snapshot
        .sessions
        .iter()
        .find(|s| s.name == "work-1")
        .unwrap();
    let mut saved = f.saved();
    saved["sessions"][0]["name"] = "renamed".into();
    saved["sessions"][0]["agent"] = "renamed".into();
    saved["tabs"][0]["sessions"][0] = "renamed".into();
    let journal = serde_json::json!({
        "id": old.id, "old": old.name, "new": "renamed",
        "panes": old.windows.iter().flat_map(|w| &w.panes).map(|p| (p.id, p.child_pid)).collect::<Vec<_>>(),
        "files": [["work", saved]],
    });
    // Simulate a CLI exit after the daemon ACK but before updating the file.
    let mut stream = UnixStream::connect(&f.socket).unwrap();
    let req = ControlRequest::NameSession {
        version: PROTOCOL_VERSION,
        request_id: 1,
        session_id: old.id,
        name: "renamed".into(),
    };
    writeln!(stream, "{}", serde_json::to_string(&req).unwrap()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: ControlResponse = serde_json::from_str(&line).unwrap();
    assert!(matches!(response.body, ControlResponseBody::Ok { .. }));
    let path = f.dir.join("prismattyc/spaces/.session-name-transaction");
    std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
    assert_eq!(
        f.ok(&["session", "suggest", "--space", "work"]).trim(),
        "work-2"
    );
    assert!(!path.exists());
    assert_eq!(f.saved()["sessions"][0]["name"], "renamed");
    assert_eq!(f.saved()["tabs"][0]["sessions"][0], "renamed");
    assert_eq!(
        f.snapshot()
            .sessions
            .iter()
            .find(|s| s.name == "renamed")
            .unwrap()
            .id,
        old.id
    );
}

#[test]
fn moving_a_single_pane_between_spaces_keeps_its_session_and_mailbox_name() {
    let f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "create", "other", "--no-attach"]);
    let before = f.snapshot();
    let session = before.sessions.iter().find(|s| s.name == "work-1").unwrap();
    let pane = &session.windows[0].panes[0];
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "before move",
    ]);
    f.ok(&["space", "move", "other", "--pane", &pane.id.to_string()]);
    let after = f.snapshot();
    let moved = after.sessions.iter().find(|s| s.id == session.id).unwrap();
    assert_eq!(moved.name, "work-1");
    assert_eq!(moved.agent_id.as_deref(), Some("work-1"));
    assert_ne!(moved.space_id, session.space_id);
    assert_eq!(moved.windows[0].panes[0].id, pane.id);
    assert_eq!(moved.windows[0].panes[0].child_pid, pane.child_pid);
    assert!(f
        .ok(&["mail", "--as", "work-1", "inbox"])
        .contains("open: 1 held: 0"));
}

#[test]
fn reopen_restores_only_the_named_saved_seat_with_mail_and_owner() {
    let f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "add", "work"]);
    f.ok(&["session", "name", "astra-spaces", "--session", "work-1"]);
    f.ok(&[
        "mail",
        "--as",
        "sender",
        "send",
        "work-1",
        "--summary",
        "restart",
        "--body",
        "retained",
    ]);
    let owner = f.saved()["id"].as_str().unwrap().to_string();
    f.ok(&["stop", "astra-spaces"]);
    f.ok(&["stop", "work-2"]);
    f.ok(&["session", "reopen", "astra-spaces", "--space", "work"]);
    let snapshot = f.snapshot();
    let restored = snapshot
        .sessions
        .iter()
        .find(|s| s.name == "astra-spaces")
        .unwrap();
    assert_eq!(restored.agent_id.as_deref(), Some("astra-spaces"));
    assert_eq!(restored.space_id.as_deref(), Some(owner.as_str()));
    assert!(!snapshot.sessions.iter().any(|s| s.name == "work-2"));
    let before = restored.id;
    f.ok(&["session", "reopen", "astra-spaces", "--space", "work"]);
    assert_eq!(
        f.snapshot()
            .sessions
            .iter()
            .find(|s| s.name == "astra-spaces")
            .unwrap()
            .id,
        before
    );
    let mail = f.ok(&["mail", "--as", "work-1", "claim", "--json"]);
    assert!(mail.contains("retained"), "{mail}");
}
