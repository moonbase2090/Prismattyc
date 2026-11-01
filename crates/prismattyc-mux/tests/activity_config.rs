//! `[mux] experimental_activity` turns on the activity feed when pmuxd
//! starts, so an app-managed daemon can serve `pmux-activity-monitor`
//! without a separate pmuxd. The flag still works, and the default is off.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
    let config_path = dir.join("config.toml");
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

fn read_activity(socket: &Path) -> ControlResponse {
    let mut stream = UnixStream::connect(socket).expect("connect");
    let request = ControlRequest::ReadActivity {
        version: PROTOCOL_VERSION,
        request_id: 1,
        cursor: None,
        limit: 8,
    };
    serde_json::to_writer(&mut stream, &request).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
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
fn feed_is_off_without_the_flag_or_the_config_key() {
    let fixture = start(None, false);
    assert!(!feed_on(&fixture.socket));
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
