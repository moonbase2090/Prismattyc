//! `[mux] experimental_activity` turns on the activity feed, so an
//! app-managed daemon can serve `pmux-activity-monitor` without a separate
//! pmuxd. pmuxd applies edits live, without a restart, and leaves sessions
//! alone. The feed is on by default, `false` opts out, and the flag still
//! keeps it on.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use prismattyc_mux::{
    ControlErrorCode, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData,
    PROTOCOL_VERSION,
};

mod support;
use support::clear_command_env;

struct Fixture {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(config: Option<&str>, flag: bool) -> Fixture {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Short path: Unix socket paths are limited to about 100 bytes on macOS.
    let dir = std::env::temp_dir().join(format!(
        "pmux-actcfg-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = config_path(&dir);
    if let Some(config) = config {
        std::fs::write(&config_path, config).unwrap();
    }
    let socket = dir.join("s.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    command.arg("--socket").arg(&socket);
    if flag {
        command.arg("--experimental-activity");
    }
    let child = command
        .args(["--", "/bin/sh", "-c", "exec sleep 999"])
        // A missing file is the default config; never read the caller's own.
        .env("PRISMATTYC_CONFIG", &config_path)
        .env("XDG_DATA_HOME", &dir)
        .env("PMUX_SESSION_AGENTS", dir.join("session-agents.json"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pmuxd");
    let fixture = Fixture { child, dir, socket };
    for _ in 0..250 {
        if UnixStream::connect(&fixture.socket).is_ok() {
            return fixture;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pmuxd did not listen on {}", fixture.socket.display());
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

fn call(socket: &Path, request: &ControlRequest) -> ControlResponse {
    let mut stream = UnixStream::connect(socket).expect("connect");
    serde_json::to_writer(&mut stream, request).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn read_activity(socket: &Path) -> ControlResponse {
    call(
        socket,
        &ControlRequest::ReadActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            cursor: None,
            limit: 8,
        },
    )
}

/// The feed's boot epoch, or `None` while it is off.
fn feed_epoch(socket: &Path) -> Option<String> {
    match read_activity(socket).body {
        ControlResponseBody::Ok {
            response: ControlResponseData::Activity { page },
        } => Some(page.epoch),
        ControlResponseBody::Error { error }
            if error.code == ControlErrorCode::ActivityDisabled =>
        {
            None
        }
        other => panic!("unexpected ReadActivity reply: {other:?}"),
    }
}

/// Session and pane ids, which a feed change must leave alone.
fn topology(socket: &Path) -> Vec<(u64, Vec<u64>)> {
    let reply = call(
        socket,
        &ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id: 2,
        },
    );
    let ControlResponseBody::Ok {
        response: ControlResponseData::Snapshot { snapshot },
    } = reply.body
    else {
        panic!("expected a snapshot: {reply:?}");
    };
    snapshot
        .sessions
        .iter()
        .map(|session| {
            let panes = session
                .windows
                .iter()
                .flat_map(|window| window.panes.iter().map(|pane| pane.id))
                .collect();
            (session.id, panes)
        })
        .collect()
}

/// Rewrite the config and wait for the live daemon to reach `on`.
fn flip(fixture: &Fixture, on: bool) -> Option<String> {
    std::fs::write(
        config_path(&fixture.dir),
        format!("[mux]\nexperimental_activity = {on}\n"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let epoch = feed_epoch(&fixture.socket);
        if epoch.is_some() == on {
            return epoch;
        }
        assert!(
            Instant::now() < deadline,
            "feed did not turn {} within 10s",
            if on { "on" } else { "off" }
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn feed_on(socket: &Path) -> bool {
    match read_activity(socket).body {
        ControlResponseBody::Ok {
            response: ControlResponseData::Activity { .. },
        } => true,
        ControlResponseBody::Error { error }
            if error.code == ControlErrorCode::ActivityDisabled =>
        {
            false
        }
        other => panic!("unexpected ReadActivity reply: {other:?}"),
    }
}

#[test]
fn feed_is_on_by_default_without_a_config_file() {
    let fixture = start(None, false);
    assert!(feed_on(&fixture.socket));
}

#[test]
fn feed_is_on_by_default_when_the_config_leaves_the_key_out() {
    let fixture = start(Some("[mux]\nshell = \"/bin/sh\"\n"), false);
    assert!(feed_on(&fixture.socket));
}

#[test]
fn setting_the_key_false_stops_a_default_feed_on_a_live_daemon() {
    let fixture = start(None, false);
    let epoch = feed_epoch(&fixture.socket).expect("on by default");
    let sessions = topology(&fixture.socket);
    assert_eq!(flip(&fixture, false), None);
    let again = flip(&fixture, true).expect("feed on again");
    assert_ne!(epoch, again, "turning the feed back on starts a fresh feed");
    assert_eq!(
        topology(&fixture.socket),
        sessions,
        "sessions and panes are untouched"
    );
}

#[test]
fn feed_stays_off_when_the_config_key_is_false() {
    let fixture = start(Some("[mux]\nexperimental_activity = false\n"), false);
    assert!(!feed_on(&fixture.socket));
}

#[test]
fn config_key_turns_the_feed_on_at_daemon_start() {
    let fixture = start(Some("[mux]\nexperimental_activity = true\n"), false);
    assert!(feed_on(&fixture.socket));
}

#[test]
fn flag_still_turns_the_feed_on_when_the_config_key_is_false() {
    let fixture = start(Some("[mux]\nexperimental_activity = false\n"), true);
    assert!(feed_on(&fixture.socket));
}

#[test]
fn editing_the_key_starts_and_stops_the_feed_on_a_live_daemon() {
    let fixture = start(Some("[mux]\nexperimental_activity = false\n"), false);
    assert_eq!(feed_epoch(&fixture.socket), None);
    let sessions = topology(&fixture.socket);
    assert!(!sessions.is_empty(), "the bootstrap session exists");

    let first = flip(&fixture, true).expect("feed on");
    flip(&fixture, false);
    let second = flip(&fixture, true).expect("feed on again");
    assert_ne!(
        first, second,
        "turning the feed back on starts a fresh feed"
    );
    flip(&fixture, false);

    assert_eq!(
        topology(&fixture.socket),
        sessions,
        "sessions and panes are untouched"
    );
    assert!(
        UnixStream::connect(&fixture.socket).is_ok(),
        "the same daemon still serves its socket"
    );
}

#[test]
fn a_broken_config_keeps_the_live_feed_on() {
    let fixture = start(Some("[mux]\nexperimental_activity = true\n"), false);
    let epoch = feed_epoch(&fixture.socket).expect("feed on");
    std::fs::write(
        config_path(&fixture.dir),
        "[mux\nexperimental_activity = false\n",
    )
    .unwrap();
    // Give the watcher several polls to see the unreadable file.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(feed_epoch(&fixture.socket), Some(epoch));
}

#[test]
fn the_flag_keeps_the_feed_on_when_the_key_turns_off_live() {
    let fixture = start(Some("[mux]\nexperimental_activity = true\n"), true);
    let epoch = feed_epoch(&fixture.socket).expect("feed on");
    std::fs::write(
        config_path(&fixture.dir),
        "[mux]\nexperimental_activity = false\n",
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(feed_epoch(&fixture.socket), Some(epoch));
}

#[test]
fn removing_or_emptying_the_config_keeps_a_running_feed() {
    // A save in progress can leave the file missing or empty for a moment.
    // Only an explicit false turns the feed off.
    let fixture = start(Some("[mux]\nexperimental_activity = true\n"), false);
    let epoch = feed_epoch(&fixture.socket).expect("feed on");
    let path = config_path(&fixture.dir);
    std::fs::write(&path, "").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        feed_epoch(&fixture.socket),
        Some(epoch.clone()),
        "empty file"
    );
    std::fs::remove_file(&path).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(feed_epoch(&fixture.socket), Some(epoch), "missing file");
    assert_eq!(flip(&fixture, false), None);
}

#[test]
fn an_edit_that_keeps_the_modification_time_and_length_still_applies() {
    let on = "[mux]\nexperimental_activity = true \n";
    let off = "[mux]\nexperimental_activity = false\n";
    assert_eq!(on.len(), off.len());
    let fixture = start(Some(on), false);
    assert!(feed_epoch(&fixture.socket).is_some());
    let path = config_path(&fixture.dir);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_secs(1));
    std::fs::write(&path, off).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while feed_epoch(&fixture.socket).is_some() {
        assert!(
            Instant::now() < deadline,
            "metadata-preserving edit was missed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
