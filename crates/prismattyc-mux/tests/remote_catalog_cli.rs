//! `pmux space catalog` through the real CLI and an isolated daemon (#24).
#![cfg(unix)]

use prismattyc_mux::remote_catalog::{parse_catalog, Catalog, UnavailableReason};
use prismattyc_mux::{
    save_space, stub_space_session, ControlRequest, ControlResponse, ControlResponseBody,
    ControlResponseData, SavedSpace, Snapshot, PROTOCOL_VERSION, SAVED_SPACE_VERSION,
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
        self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pmux-catalog-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = Self {
            socket: dir.join("mux.sock"),
            dir,
            server: None,
        };
        f.server = Some(
            f.command(env!("CARGO_BIN_EXE_pmuxd"))
                .arg("--socket")
                .arg(&f.socket)
                .args(["--", "/bin/sh", "-c", "exec sleep 999"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&f.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        f
    }

    fn stop_daemon(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
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

    fn cli(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_pmux"))
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "pmux {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn catalog(&self) -> Catalog {
        parse_catalog(self.ok(&["space", "catalog"]).trim_end().as_bytes()).unwrap()
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

    fn save_legacy_space(&self, name: &str, session: &str) {
        let space = SavedSpace {
            version: SAVED_SPACE_VERSION,
            id: None,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![stub_space_session(session)],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        save_space(&self.dir.join("prismattyc/spaces"), name, &space).unwrap();
    }
}

#[test]
fn catalog_lists_owned_live_sessions_and_explains_the_rest_without_side_effects() {
    let f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "add", "work"]);
    f.ok(&["space", "create", "idle", "--no-attach"]);
    f.save_legacy_space("legacy", "work-1");

    let before = f.snapshot();
    let owner = |space: &str| {
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(f.dir.join(format!("prismattyc/spaces/{space}.json"))).unwrap(),
        )
        .unwrap();
        saved["id"].as_str().unwrap().to_string()
    };
    let work_id = owner("work");
    let idle_id = owner("idle");
    let idle_sessions: Vec<_> = before
        .sessions
        .iter()
        .filter(|s| s.space_id.as_deref() == Some(idle_id.as_str()))
        .map(|s| s.name.clone())
        .collect();
    for name in &idle_sessions {
        f.ok(&["stop", name]);
    }
    let before = f.snapshot();

    let catalog = f.catalog();
    let after = f.snapshot();
    assert_eq!(
        before.sessions.len(),
        after.sessions.len(),
        "catalog must not create or stop sessions"
    );

    let [work] = catalog.spaces.as_slice() else {
        panic!("one running Space: {catalog:?}")
    };
    assert_eq!(work.name, "work");
    assert_eq!(work.id.as_str(), work_id);
    let mut expected: Vec<u64> = after
        .sessions
        .iter()
        .filter(|s| s.space_id.as_deref() == Some(work_id.as_str()))
        .map(|s| s.id)
        .collect();
    expected.sort_unstable();
    let mut listed: Vec<u64> = work.sessions.iter().map(|s| s.id.0).collect();
    listed.sort_unstable();
    assert_eq!(listed, expected);
    assert_eq!(listed.len(), 2);
    assert!(work.sessions.iter().any(|s| s.id == work.active_session));

    let reason = |name: &str| {
        catalog
            .unavailable
            .iter()
            .find(|space| space.name == name)
            .map(|space| space.reason)
    };
    assert_eq!(reason("idle"), Some(UnavailableReason::NoLiveSessions));
    assert_eq!(
        reason("legacy"),
        Some(UnavailableReason::MissingIdentity),
        "a legacy file naming a live session is never attachable by name"
    );
}

#[test]
fn catalog_reports_a_stopped_daemon_as_an_error() {
    let mut f = Fixture::new();
    f.stop_daemon();
    let out = f.cli(&["space", "catalog"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "no partial catalog on stdout");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn space_id_attach_requires_ownership_and_never_starts_a_daemon() {
    let mut f = Fixture::new();
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "create", "other", "--no-attach"]);
    let catalog = f.catalog();
    let space = |name: &str| {
        catalog
            .spaces
            .iter()
            .find(|space| space.name == name)
            .unwrap()
    };
    let work = space("work").clone();
    let other = space("other").clone();
    let session = work.active_session.0.to_string();
    fn attach(f: &Fixture, session: &str, space_id: &str) -> Output {
        f.command(env!("CARGO_BIN_EXE_pmux"))
            .env("PMUX_ATTACH", env!("CARGO_BIN_EXE_pmux-attach"))
            .args([
                "attach",
                "--session-id",
                session,
                "--space-id",
                space_id,
                "--json",
            ])
            .output()
            .unwrap()
    }

    let owned = attach(&f, &session, work.id.as_str());
    assert!(
        owned.status.success(),
        "{}",
        String::from_utf8_lossy(&owned.stderr)
    );
    assert!(!owned.stdout.is_empty(), "the owned session attaches");

    let moved = attach(&f, &session, other.id.as_str());
    assert!(!moved.status.success());
    assert!(
        String::from_utf8_lossy(&moved.stderr).contains("no longer in that Space"),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );

    let gone = attach(&f, "999999", work.id.as_str());
    assert!(!gone.status.success());
    assert!(String::from_utf8_lossy(&gone.stderr).contains("no longer running"));

    f.stop_daemon();
    let _ = std::fs::remove_file(&f.socket);
    let down = attach(&f, &session, work.id.as_str());
    assert!(!down.status.success());
    assert!(String::from_utf8_lossy(&down.stderr).contains("not running"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !f.socket.exists(),
        "a remote attach must never start a daemon"
    );
}
