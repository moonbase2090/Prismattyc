//! Remote Spaces over real SSH (issue #24): catalog, attach, resize,
//! disconnect, reconnect and stale selection against an isolated pmuxd.
//!
//! Skipped unless these are set (nothing about the key is committed):
//! - `PRISMATTYC_SSH_TEST_TARGET`: `user@host`, usually loopback.
//! - `PRISMATTYC_SSH_TEST_KEY`: private key authorized for that target.
//! - `PRISMATTYC_SSH_TEST_KNOWN_HOSTS`: known_hosts pinning its host key.
//!
//! The target shares this machine's filesystem. Every remote command runs
//! under `env` with this test's PMUX_SOCKET and XDG directories and the
//! test-built binaries by absolute path, so it never reaches a live daemon.
//! See `scripts/remote-ssh-tests.sh`.
#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use prismattyc_mux::remote_catalog::{parse_catalog, Catalog, UnavailableReason};
use prismattyc_mux::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, Snapshot,
    PROTOCOL_VERSION,
};
mod support;

struct Target {
    host: String,
    key: PathBuf,
    known_hosts: PathBuf,
}

fn target() -> Option<Target> {
    let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    match (
        var("PRISMATTYC_SSH_TEST_TARGET"),
        var("PRISMATTYC_SSH_TEST_KEY"),
        var("PRISMATTYC_SSH_TEST_KNOWN_HOSTS"),
    ) {
        (Some(host), Some(key), Some(known_hosts)) => Some(Target {
            host: host.to_string_lossy().into_owned(),
            key: key.into(),
            known_hosts: known_hosts.into(),
        }),
        _ => {
            eprintln!("skipped: PRISMATTYC_SSH_TEST_TARGET/_KEY/_KNOWN_HOSTS not set");
            None
        }
    }
}

/// Remote shell words must survive `ssh`'s space join and `sh` parsing.
fn shell_safe(value: &str) -> &str {
    assert!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-=:@+,".contains(&b)),
        "not shell-safe for a remote command: {value:?}"
    );
    value
}

struct Fixture {
    target: Target,
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
    fn new(target: Target) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "pmux-ssh-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        shell_safe(dir.to_str().unwrap());
        let mut fixture = Self {
            target,
            socket: dir.join("mux.sock"),
            dir,
            server: None,
        };
        fixture.start_daemon();
        fixture
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        let dir = self.dir.to_str().unwrap();
        vec![
            ("PMUX_SOCKET".into(), self.socket.to_str().unwrap().into()),
            ("XDG_DATA_HOME".into(), dir.into()),
            ("XDG_CONFIG_HOME".into(), format!("{dir}/config")),
            ("XDG_STATE_HOME".into(), format!("{dir}/state")),
            ("PMUX_SESSION_AGENTS".into(), format!("{dir}/agents.json")),
            (
                "PMUX_ATTACH".into(),
                env!("CARGO_BIN_EXE_pmux-attach").into(),
            ),
        ]
    }

    fn local(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        support::clear_command_env(&mut command);
        command.envs(self.env_pairs()).stdin(Stdio::null());
        command
    }

    fn start_daemon(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        self.server = Some(
            self.local(env!("CARGO_BIN_EXE_pmuxd"))
                .arg("--socket")
                .arg(&self.socket)
                .args(["--", "/bin/sh", "-c", "exec sleep 999"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&self.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop_daemon(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self
            .local(env!("CARGO_BIN_EXE_pmux"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "pmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn snapshot(&self) -> Snapshot {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket).unwrap();
        let request = ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 1,
        };
        writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        match serde_json::from_str::<ControlResponse>(&line).unwrap().body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Snapshot { snapshot },
            } => snapshot,
            other => panic!("snapshot: {other:?}"),
        }
    }

    /// `ssh` options shared by every call; host key pinned, no agent.
    fn ssh_args(&self, key: &Path, known_hosts: &Path) -> Vec<String> {
        vec![
            "-i".into(),
            key.to_str().unwrap().into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "IdentityAgent=none".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o".into(),
            "GlobalKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
        ]
    }

    /// Remote words: `env VAR=… <abs pmux> args…`.
    fn remote_words(&self, pmux_args: &[&str]) -> Vec<String> {
        let mut words = vec!["env".to_string()];
        for (key, value) in self.env_pairs() {
            words.push(format!("{key}={}", shell_safe(&value)));
        }
        words.push(shell_safe(env!("CARGO_BIN_EXE_pmux")).into());
        words.extend(pmux_args.iter().map(|arg| shell_safe(arg).to_string()));
        words
    }

    fn ssh_with(&self, key: &Path, known_hosts: &Path, pmux_args: &[&str]) -> Output {
        Command::new("/usr/bin/ssh")
            .args(self.ssh_args(key, known_hosts))
            .args(["-T", "--", &self.target.host])
            .args(self.remote_words(pmux_args))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ssh(&self, pmux_args: &[&str]) -> Output {
        self.ssh_with(&self.target.key, &self.target.known_hosts, pmux_args)
    }

    fn catalog(&self) -> Catalog {
        let out = self.ssh(&["space", "catalog"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_catalog(String::from_utf8_lossy(&out.stdout).trim_end().as_bytes()).unwrap()
    }

    fn keygen(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        let status = Command::new("/usr/bin/ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "pmux-ssh-test-unauthorized",
                "-f",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn catalog_over_ssh_lists_the_running_space() {
    let Some(target) = target() else { return };
    let f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "add", "work"]);
    let catalog = f.catalog();
    let work = catalog
        .spaces
        .iter()
        .find(|space| space.name == "work")
        .unwrap();
    assert_eq!(work.sessions.len(), 2);
    assert!(work.sessions.iter().any(|s| s.id == work.active_session));
}

#[test]
fn attach_over_ssh_checks_space_ownership() {
    let Some(target) = target() else { return };
    let f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    f.ok(&["space", "create", "other", "--no-attach"]);
    let catalog = f.catalog();
    let space = |name: &str| {
        catalog
            .spaces
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .clone()
    };
    let (work, other) = (space("work"), space("other"));
    let session = work.active_session.0.to_string();
    let attach = |session: &str, space: &str| {
        f.ssh(&[
            "attach",
            "--session-id",
            session,
            "--space-id",
            space,
            "--json",
        ])
    };
    let owned = attach(&session, work.id.as_str());
    assert!(owned.status.success(), "{}", stderr(&owned));
    assert!(!owned.stdout.is_empty());
    let moved = attach(&session, other.id.as_str());
    assert!(!moved.status.success());
    assert!(
        stderr(&moved).contains("no longer in that Space"),
        "{}",
        stderr(&moved)
    );
    let gone = attach("999999", work.id.as_str());
    assert!(
        stderr(&gone).contains("no longer running"),
        "{}",
        stderr(&gone)
    );
}

#[test]
fn stopped_remote_daemon_is_reported_and_never_started() {
    let Some(target) = target() else { return };
    let mut f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    let work = f.catalog().spaces[0].clone();
    f.stop_daemon();
    let catalog = f.ssh(&["space", "catalog"]);
    assert!(!catalog.status.success());
    assert!(catalog.stdout.is_empty());
    assert!(
        stderr(&catalog).contains("not running"),
        "{}",
        stderr(&catalog)
    );
    let session = work.active_session.0.to_string();
    let attach = f.ssh(&[
        "attach",
        "--session-id",
        &session,
        "--space-id",
        work.id.as_str(),
        "--json",
    ]);
    assert!(
        stderr(&attach).contains("not running"),
        "{}",
        stderr(&attach)
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!f.socket.exists(), "a remote attach must not start pmuxd");
}

#[test]
fn unauthorized_key_and_unknown_host_key_are_refused() {
    let Some(target) = target() else { return };
    let f = Fixture::new(target);
    let stranger = f.keygen("stranger");
    let out = f.ssh_with(&stranger, &f.target.known_hosts, &["space", "catalog"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(
        stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );

    let fake_host = f.keygen("fake_host");
    let fake_pub = std::fs::read_to_string(fake_host.with_extension("pub")).unwrap();
    let key: Vec<&str> = fake_pub.split_whitespace().take(2).collect();
    let host = f.target.host.rsplit('@').next().unwrap().to_string();
    let wrong_known = f.dir.join("wrong_known_hosts");
    std::fs::write(&wrong_known, format!("{host} {} {}\n", key[0], key[1])).unwrap();
    let out = f.ssh_with(&f.target.key, &wrong_known, &["space", "catalog"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(
        stderr(&out).contains("Host key verification failed")
            || stderr(&out).contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "{}",
        stderr(&out)
    );
}

fn pane_size(f: &Fixture, session: u64) -> Option<(u32, u32)> {
    f.snapshot()
        .sessions
        .iter()
        .find(|s| s.id == session)?
        .windows
        .first()?
        .panes
        .first()
        .map(|pane| (pane.geometry.cols, pane.geometry.rows))
}

fn spawn_tty_attach(
    f: &Fixture,
    session: &str,
    space: &str,
) -> (
    portable_pty::PtyPair,
    Box<dyn portable_pty::Child + Send + Sync>,
    Arc<Mutex<Vec<u8>>>,
) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("/usr/bin/ssh");
    support::clear_pty_env(&mut command);
    command.env("TERM", "xterm-256color");
    command.args(f.ssh_args(&f.target.key, &f.target.known_hosts));
    command.args(["-tt", "--", &f.target.host]);
    command.args(f.remote_words(&["attach", "--session-id", session, "--space-id", space]));
    let child = pair.slave.spawn_command(command).unwrap();
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let buf = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    (pair, child, transcript)
}

fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn tty_attach_over_ssh_resizes_disconnects_and_reconnects() {
    let Some(target) = target() else { return };
    let f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    let work = f.catalog().spaces[0].clone();
    let id = work.active_session.0;
    let session = id.to_string();

    let (pair, mut child, transcript) = spawn_tty_attach(&f, &session, work.id.as_str());
    wait_for("attach output", || transcript.lock().unwrap().len() > 64);
    let attached = pane_size(&f, id);
    pair.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    wait_for("remote pane to follow the resize", || {
        pane_size(&f, id).is_some_and(|(cols, _)| cols == 100)
    });
    let resized = pane_size(&f, id).unwrap();
    eprintln!("pane size attached {attached:?} -> resized {resized:?}");
    assert!(
        resized.1 > 24 && resized.1 <= 30,
        "rows follow: {resized:?}"
    );

    // Disconnect: kill the local ssh. The remote session keeps running.
    child.kill().unwrap();
    let _ = child.wait();
    drop(pair);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        f.snapshot().sessions.iter().any(|s| s.id == id),
        "the session outlives the SSH connection"
    );
    assert_eq!(f.catalog().spaces[0].active_session.0, id);

    // Reconnect with the same ids.
    let (pair, mut child, transcript) = spawn_tty_attach(&f, &session, work.id.as_str());
    wait_for("reattach output", || transcript.lock().unwrap().len() > 64);
    child.kill().unwrap();
    let _ = child.wait();
    drop(pair);
}

#[test]
fn stale_ids_after_a_remote_daemon_restart_are_refused() {
    let Some(target) = target() else { return };
    let mut f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    let work = f.catalog().spaces[0].clone();
    let session = work.active_session.0.to_string();

    f.stop_daemon();
    f.start_daemon();
    // Session ids restart; another Space may now own the old number.
    f.ok(&["space", "create", "other", "--no-attach"]);
    let catalog = f.catalog();
    let reason = catalog
        .unavailable
        .iter()
        .find(|space| space.name == "work")
        .map(|space| space.reason);
    assert_eq!(reason, Some(UnavailableReason::NoLiveSessions));
    let attach = f.ssh(&[
        "attach",
        "--session-id",
        &session,
        "--space-id",
        work.id.as_str(),
        "--json",
    ]);
    assert!(!attach.status.success());
    let text = stderr(&attach);
    assert!(
        text.contains("no longer in that Space") || text.contains("no longer running"),
        "{text}"
    );
}

/// The `pmux ls` pane line for `pane`, e.g. `pane 2 — 80x24 … viewers 123`.
fn ls_pane_line(f: &Fixture, pane: u64) -> String {
    let prefix = format!("pane {pane} ");
    f.ok(&["ls"])
        .lines()
        .map(str::trim_start)
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn tty_attach_over_ssh_shows_and_reports_the_selected_session() {
    let Some(target) = target() else { return };
    let f = Fixture::new(target);
    f.ok(&["space", "create", "work", "--no-attach"]);
    let work = f.catalog().spaces[0].clone();
    let id = work.active_session.0;
    let snapshot = f.snapshot();
    let first = snapshot.sessions[0].id;
    assert_ne!(first, id, "the selected session must not be the first one");
    let pane_of = |session: u64| {
        let session = snapshot.sessions.iter().find(|s| s.id == session).unwrap();
        session.windows[0].panes[0].id.to_string()
    };
    let (first_pane, work_pane) = (pane_of(first), pane_of(id));

    let (pair, mut child, transcript) = spawn_tty_attach(&f, &id.to_string(), work.id.as_str());
    wait_for("attach output", || transcript.lock().unwrap().len() > 64);
    f.ok(&[
        "attach",
        "--pane",
        &first_pane,
        "--write",
        "FIRST_PANE_MARK",
    ]);
    f.ok(&["attach", "--pane", &work_pane, "--write", "WORK_PANE_MARK"]);
    wait_for("the selected pane to be drawn", || {
        String::from_utf8_lossy(&transcript.lock().unwrap()).contains("WORK_PANE_MARK")
    });
    let shown = String::from_utf8_lossy(&transcript.lock().unwrap()).into_owned();
    assert!(!shown.contains("FIRST_PANE_MARK"), "drew the first session");

    // `pmux ls` attributes the viewer to the selected session, not the first.
    let work_line = ls_pane_line(&f, work_pane.parse().unwrap());
    let first_line = ls_pane_line(&f, first_pane.parse().unwrap());
    assert!(work_line.contains("viewers"), "{work_line}");
    assert!(!first_line.contains("viewers"), "{first_line}");

    child.kill().unwrap();
    let _ = child.wait();
    drop(pair);
}
