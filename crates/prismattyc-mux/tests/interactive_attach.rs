//! JSON dump stays scriptable; TTY attach types, detaches, reattaches.

#![cfg(unix)]

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use prismattyc_core::test_time_budget;
use prismattyc_emulator::Emulator;

mod support;
use support::{clear_command_env, clear_pty_env};

use prismattyc_mux::{
    AxisWire, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, Event,
    Snapshot, SpawnSpec, PROTOCOL_VERSION,
};

fn socket_path() -> PathBuf {
    // A per-process atomic counter guarantees a distinct socket per test.
    // A wall-clock timestamp is not enough: parallel test threads can start
    // within the same clock tick, collide on one path, and cross-wire their
    // servers and attach clients.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "prism-attach-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

struct ServerGuard {
    child: Child,
    socket: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn start_server(socket: &Path) -> ServerGuard {
    start_server_shell(socket, "printf 'READY\\n'; exec /bin/cat")
}

fn start_server_shell(socket: &Path, script: &str) -> ServerGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmuxd"));
    clear_command_env(&mut command);
    command
        .arg("--socket")
        .arg(socket)
        .args(["--", "/bin/sh", "-c", script])
        .env("PMUX_PANE_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let guard = ServerGuard {
        child: command.spawn().expect("spawn pmuxd"),
        socket: socket.to_path_buf(),
    };
    let start = Instant::now();
    let budget = test_time_budget(Duration::from_secs(2));
    while start.elapsed() < budget {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not publish {}", socket.display());
}

fn attach_json(socket: &Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux-attach"));
    clear_command_env(&mut command);
    let output = command
        .arg("--socket")
        .arg(socket)
        .output()
        .expect("run pmux-attach");
    assert!(
        output.status.success(),
        "attach json failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait_transcript(buf: &Arc<Mutex<Vec<u8>>>, needle: &str, timeout: Duration) {
    let timeout = test_time_budget(timeout);
    let start = Instant::now();
    loop {
        let hay = String::from_utf8_lossy(&buf.lock().expect("transcript")).into_owned();
        if hay.contains(needle) {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timeout waiting for {needle:?} in {hay:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_attach_json(socket: &Path, needles: &[&str]) -> String {
    let start = Instant::now();
    let budget = test_time_budget(Duration::from_secs(1));
    loop {
        let body = attach_json(socket);
        if needles.iter().all(|needle| body.contains(needle)) {
            return body;
        }
        assert!(
            start.elapsed() < budget,
            "timeout waiting for {needles:?} in attach JSON: {body}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn piped_attach_prints_json_readpane() {
    let socket = socket_path();
    let _server = start_server(&socket);
    let body = wait_attach_json(&socket, &["READY"]);
    assert!(body.contains("\"pane_id\""), "{body}");
    assert!(body.contains("READY"), "{body}");
    let parsed: serde_json::Value = serde_json::from_str(body.trim()).expect("json");
    assert!(parsed.get("child_alive").and_then(|v| v.as_bool()) == Some(true));
}

#[test]
fn interactive_type_detach_reattach_keeps_output() {
    let socket = socket_path();
    let _server = start_server(&socket);
    wait_attach_json(&socket, &["READY"]);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_pmux-attach"));
    clear_pty_env(&mut cmd);
    cmd.arg("--socket");
    cmd.arg(socket.as_os_str());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn attach on pty");
    drop(pair.slave);

    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let buf = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf
                    .lock()
                    .expect("transcript")
                    .extend_from_slice(&chunk[..n]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(_) => break,
            }
        }
    });
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_transcript(&transcript, "READY", Duration::from_secs(3));
    writer.write_all(b"hello\n").expect("type hello");
    let _ = writer.flush();
    wait_transcript(&transcript, "hello", Duration::from_secs(3));

    writer.write_all(&[0x1c, b'd']).expect("detach chord");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));

    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");

    let reattached = wait_attach_json(&socket, &["hello", "READY"]);
    assert!(reattached.contains("READY"), "{reattached}");
    assert!(reattached.contains("hello"), "{reattached}");
    assert!(reattached.contains("\"child_alive\":true"), "{reattached}");
}

#[test]
fn interactive_enter_scroll_empty_history_forwards_later_keys() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_transcript(&transcript, "READY", Duration::from_secs(3));
    writer.write_all(&[0x1c, b'[']).expect("enter scroll");
    let _ = writer.flush();
    writer.write_all(b"z").expect("type after empty history");
    let _ = writer.flush();
    wait_transcript(&transcript, "z", Duration::from_secs(3));

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

fn spawn_attach_pty(
    socket: &Path,
    rows: u16,
    cols: u16,
) -> (
    portable_pty::PtyPair,
    Box<dyn portable_pty::Child + Send + Sync>,
    Arc<Mutex<Vec<u8>>>,
) {
    spawn_attach_pty_args(socket, rows, cols, &[])
}

fn spawn_attach_pty_args(
    socket: &Path,
    rows: u16,
    cols: u16,
    extra: &[&str],
) -> (
    portable_pty::PtyPair,
    Box<dyn portable_pty::Child + Send + Sync>,
    Arc<Mutex<Vec<u8>>>,
) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_pmux-attach"));
    clear_pty_env(&mut cmd);
    cmd.arg("--socket");
    cmd.arg(socket.as_os_str());
    for arg in extra {
        cmd.arg(*arg);
    }
    let child = pair.slave.spawn_command(cmd).expect("spawn attach on pty");
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let buf = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf
                    .lock()
                    .expect("transcript")
                    .extend_from_slice(&chunk[..n]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(_) => break,
            }
        }
    });
    (pair, child, transcript)
}

fn outer_grid_last_cell(transcript: &[u8], cols: usize, rows: usize, needle: char) -> Option<char> {
    let mut emu = Emulator::new(cols, rows, 0);
    emu.feed(transcript);
    for row in 0..rows {
        if let Some(cells) = emu.screen().row(row) {
            if cells.iter().any(|cell| cell.character == needle) {
                return cells.last().map(|cell| cell.character);
            }
        }
    }
    None
}

fn wait_visible_screen(transcript: &Arc<Mutex<Vec<u8>>>, present: &str, absent: &str) {
    let mut emulator = Emulator::new(80, 24, 0);
    let mut consumed = 0;
    let started = Instant::now();
    loop {
        {
            let bytes = transcript.lock().expect("transcript");
            emulator.feed(&bytes[consumed..]);
            consumed = bytes.len();
        }
        let screen = emulator.screen();
        let visible = screen
            .viewport_range()
            .map(|range| screen.extract_text(range))
            .unwrap_or_default();
        if visible.contains(present) && !visible.contains(absent) {
            return;
        }
        assert!(
            started.elapsed() < test_time_budget(Duration::from_secs(3)),
            "visible screen did not contain {present:?} without {absent:?}: {visible:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn alt_screen_output_is_scrollable_through_attach() {
    // a TUI on the alt screen scrolls lines off its top; the server
    // retains them and attach scroll mode can page back to them.
    let socket = socket_path();
    let script = "printf '\x1b[?1049h'; i=1; while [ \"$i\" -le 60 ]; do printf 'ALT%03d\n' \"$i\"; i=$((i+1)); done; exec cat";
    let _server = start_server_shell(&socket, script);
    for _ in 0..80 {
        let dump = attach_json(&socket);
        if dump.contains("ALT060") && dump.contains("\"alt_active\":true") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let dump = attach_json(&socket);
    assert!(
        dump.contains("\"alt_active\":true"),
        "child must be on the alt screen: {dump}"
    );

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_transcript(&transcript, "ALT060", Duration::from_secs(3));
    writer.write_all(b"[5~").expect("page up");
    let _ = writer.flush();
    wait_transcript(&transcript, "[scroll", Duration::from_secs(3));
    writer.write_all(b"[H").expect("home");
    let _ = writer.flush();
    // Earliest evicted alt rows are reachable.
    wait_transcript(&transcript, "ALT001", Duration::from_secs(3));

    writer.write_all(b"q").expect("leave scroll");
    let _ = writer.flush();
    wait_transcript(&transcript, "ALT060", Duration::from_secs(3));

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn interactive_scrollback_pages_then_returns_to_tail() {
    let socket = socket_path();
    let script =
        "i=1; while [ \"$i\" -le 100 ]; do printf 'L%03d\\n' \"$i\"; i=$((i+1)); done; exec cat";
    let _server = start_server_shell(&socket, script);
    for _ in 0..80 {
        if attach_json(&socket).contains("L100") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        attach_json(&socket).contains("L100"),
        "server never produced 100 lines"
    );

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_visible_screen(&transcript, "L100", "[scroll");
    writer.write_all(b"\x1b[5~").expect("page up");
    let _ = writer.flush();
    wait_visible_screen(&transcript, "[scroll", "L001");
    writer.write_all(b"\x1b[H").expect("home");
    let _ = writer.flush();
    wait_visible_screen(&transcript, "L001", "L100");

    writer.write_all(b"q").expect("leave scroll");
    let _ = writer.flush();
    // Status redraws can exceed 800 ANSI bytes after the content was painted.
    // Check the terminal's current screen, not an arbitrary raw-output suffix.
    wait_visible_screen(&transcript, "L100", "[scroll");
    writer
        .write_all(b"LIVE_AFTER_SCROLL\n")
        .expect("live input");
    writer.flush().expect("flush live input");
    wait_visible_screen(&transcript, "LIVE_AFTER_SCROLL", "[scroll");

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn interactive_full_width_row_keeps_last_column() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_transcript(&transcript, "READY", Duration::from_secs(3));
    {
        let hay = String::from_utf8_lossy(&transcript.lock().expect("transcript")).into_owned();
        assert!(
            hay.contains("\u{1b}[?7l"),
            "attach must disable DECAWM: {hay:?}"
        );
    }

    writer.write_all(&[b'X'; 80]).expect("type 80 X");
    writer.write_all(b"\n").expect("newline");
    let _ = writer.flush();

    let start = Instant::now();
    loop {
        let hay = transcript.lock().expect("transcript").clone();
        if String::from_utf8_lossy(&hay).matches('X').count() >= 80
            && outer_grid_last_cell(&hay, 80, 24, 'X') == Some('X')
        {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let last = outer_grid_last_cell(&hay, 80, 24, 'X');
            panic!(
                "expected 80 X columns on the outer grid, last={last:?} stream={:?}",
                String::from_utf8_lossy(&hay)
                    .chars()
                    .rev()
                    .take(200)
                    .collect::<String>()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

/// The styled-row test runs three server + attach + PTY round trips in a
/// row; under CI CPU contention (act in Docker) one of them took longer than
/// the usual 3 s and the assertion fired on a frame that had not arrived yet
/// (PT-134). The budget only matters on a failing run.
const STYLED_ROW_BUDGET: Duration = Duration::from_secs(10);

#[test]
fn interactive_full_width_styled_row_keeps_last_column() {
    for cols in [80_u16, 83, 101] {
        let socket = socket_path();
        let _server = start_server(&socket);
        for _ in 0..50 {
            if attach_json(&socket).contains("READY") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, cols);
        drop(pair.slave);
        let mut writer = pair.master.take_writer().expect("pty writer");

        wait_transcript(&transcript, "READY", STYLED_ROW_BUDGET);
        let mut line = Vec::from(*b"\x1b[31m");
        line.extend(std::iter::repeat_n(b'Z', cols as usize));
        line.extend_from_slice(b"\x1b[0m\n");
        writer.write_all(&line).expect("styled full-width row");
        let _ = writer.flush();

        let start = Instant::now();
        loop {
            let hay = transcript.lock().expect("transcript").clone();
            let last = outer_grid_last_cell(&hay, cols as usize, 24, 'Z');
            if last == Some('Z') && String::from_utf8_lossy(&hay).contains("\x1b[31m") {
                break;
            }
            if start.elapsed() > STYLED_ROW_BUDGET {
                panic!(
                    "styled last column missing at {cols} cols; last={last:?} stream={:?}",
                    String::from_utf8_lossy(&hay)
                        .chars()
                        .rev()
                        .take(240)
                        .collect::<String>()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        writer.write_all(&[0x1c, b'd']).expect("detach");
        let _ = writer.flush();
        wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
        let status = child.wait().expect("wait attach");
        assert!(status.success(), "attach exit {status:?} at {cols} cols");
    }
}

/// PT-140: `pmux send --force` against a REAL interactive attach that holds
/// the lease mid-line. The forced bytes land WITHOUT a CR, so the ledger
/// stays dirty and the attach's next key — still carrying the stale
/// `controller` flag from before the takeover — must re-acquire instead of
/// dying on the server's `InputDirty` gate; the whole line reaches the child.
#[test]
fn send_force_over_live_attach_keeps_attach_alive_and_reacquires() {
    let socket = socket_path();
    let _server = start_server_shell(
        &socket,
        "printf 'READY\\n'; while IFS= read -r l; do printf 'GOT:%s\\n' \"$l\"; done",
    );
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "READY", Duration::from_secs(5));

    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    let pane_id = match ctl.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        ControlResponseData::Snapshot { snapshot } => snapshot.sessions[0].windows[0].panes[0].id,
        other => panic!("snapshot: {other:?}"),
    };

    // The attach takes the lease on the first key and the line stays open.
    writer.write_all(b"PT140_").expect("type partial line");
    let _ = writer.flush();
    wait_transcript(&transcript, "PT140_", Duration::from_secs(5));

    let mut command = Command::new(env!("CARGO_BIN_EXE_pmux"));
    clear_command_env(&mut command);
    let forced = command
        .arg("--socket")
        .arg(&socket)
        .args(["send", &pane_id.to_string(), "--force", "OK"])
        .output()
        .expect("run pmux send");
    assert!(
        forced.status.success(),
        "send --force over a live attach: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    // Type before the attach paints PT140_OK. After drain_events the local
    // controller flag may already be false; the next key would only prove
    // ensure_controller, not the stale-flag InputDirty absorb.
    writer.write_all(b"AGAIN\r").expect("type after force");
    let _ = writer.flush();
    wait_transcript(&transcript, "GOT:PT140_OKAGAIN", Duration::from_secs(5));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "attach must survive the forced write"
    );

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(5));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

struct TestClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl TestClient {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect control");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
            next_id: 1,
        }
    }

    fn request(&mut self, make: impl FnOnce(u64) -> ControlRequest) -> ControlResponseData {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = make(request_id);
        serde_json::to_writer(&mut self.writer, &request).expect("write request");
        self.writer.write_all(b"\n").expect("nl");
        self.writer.flush().expect("flush");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read response");
        let response: ControlResponse = serde_json::from_str(&line).expect("parse response");
        match response.body {
            ControlResponseBody::Ok { response } => response,
            ControlResponseBody::Error { error } => panic!("control error: {error:?}"),
        }
    }
}

fn first_snapshot_pane(snapshot: &Snapshot) -> u64 {
    snapshot
        .sessions
        .first()
        .and_then(|session| session.windows.first())
        .and_then(|window| window.panes.first())
        .map(|pane| pane.id)
        .expect("snapshot pane")
}

fn register_and_snapshot(client: &mut TestClient) -> (u64, u64) {
    let ControlResponseData::ClientRegistered { client_id } =
        client.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })
    else {
        panic!("expected ClientRegistered");
    };
    let ControlResponseData::Snapshot { snapshot } =
        client.request(|request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        })
    else {
        panic!("expected Snapshot");
    };
    (client_id, first_snapshot_pane(&snapshot))
}

fn set_mail(client: &mut TestClient, client_id: u64, pane_id: u64, queue_rev: u64, depth: u32) {
    let response = client.request(|request_id| ControlRequest::MailAttentionSet {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        cell: "aa@1".into(),
        gen: 1,
        queue_rev,
        depth,
        wake: None,
        bound_pid: None,
    });
    assert!(
        matches!(response, ControlResponseData::MailAttention { .. }),
        "{response:?}"
    );
}

fn raise_attention(client: &mut TestClient, client_id: u64, pane_id: u64, message: &str) {
    let response = client.request(|request_id| ControlRequest::RaiseAttention {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
        message: message.to_string(),
    });
    assert!(
        matches!(response, ControlResponseData::Mutation { .. }),
        "{response:?}"
    );
}

fn grid_cell(transcript: &[u8], cols: usize, rows: usize, row: usize, col: usize) -> char {
    let mut emu = Emulator::new(cols, rows, 0);
    emu.feed(transcript);
    emu.screen()
        .row(row)
        .and_then(|cells| cells.get(col))
        .map(|cell| cell.character)
        .unwrap_or('\0')
}

#[test]
fn attach_seeds_mail_letter_from_snapshot() {
    let socket = socket_path();
    let _server = start_server_shell(&socket, "printf '\\n\\n\\nREADY\\n'; exec /bin/cat");
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);
    set_mail(&mut ctl, client_id, pane_id, 1, 2);

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_transcript(&transcript, " — 2 mail", Duration::from_secs(3));
    let hay = transcript.lock().expect("transcript").clone();
    let top = grid_cell(&hay, 80, 24, 0, 0);
    assert_ne!(
        top, ' ',
        "upper-left must be the envelope block, got {top:?}"
    );
    assert_ne!(top, '✉');
    assert_eq!(
        grid_cell(&hay, 80, 24, 3, 0),
        'R',
        "READY stays below overlay"
    );

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn attach_paints_mail_letter_on_attention_event() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "READY", Duration::from_secs(3));

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);
    set_mail(&mut ctl, client_id, pane_id, 1, 1);

    wait_transcript(&transcript, " — 1 mail", Duration::from_secs(3));
    let hay = transcript.lock().expect("transcript").clone();
    let top = grid_cell(&hay, 80, 24, 0, 0);
    assert_ne!(top, 'R', "letter must cover the guest origin: {top:?}");
    assert_ne!(top, ' ', "{top:?}");

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn attach_reemits_pane_attention_as_osc9_without_visible_text() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);
    raise_attention(&mut ctl, client_id, pane_id, "needs input");

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(
        &transcript,
        "\x1b]9;needs input\x07",
        Duration::from_secs(3),
    );
    let hay = transcript.lock().expect("transcript").clone();
    let mut emu = Emulator::new(80, 24, 0);
    emu.feed(&hay);
    let visible = emu
        .screen()
        .viewport_range()
        .map(|range| emu.screen().extract_text(range))
        .unwrap_or_default();
    assert!(!visible.contains("needs input"));

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn live_pane_attention_reaches_interactive_attach() {
    let socket = socket_path();
    let _server = start_server_shell(&socket, "printf '\\033]9;from agent\\007'; exec /bin/cat");

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "\x1b]9;from agent\x07", Duration::from_secs(3));
    let hay = transcript.lock().expect("transcript").clone();
    let mut emu = Emulator::new(80, 24, 0);
    emu.feed(&hay);
    let visible = emu
        .screen()
        .viewport_range()
        .map(|range| emu.screen().extract_text(range))
        .unwrap_or_default();
    assert!(!visible.contains("from agent"));

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}
fn sh_spawn() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        argv: vec![],
        cwd: None,
        env: BTreeMap::new(),
    }
}

fn wait_pane_text(client: &mut TestClient, client_id: u64, pane_id: u64, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = String::new();
    while Instant::now() < deadline {
        match client.request(|request_id| ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        }) {
            ControlResponseData::PaneContent { content } => {
                last = content.lines.join("\n");
                if last.contains(needle) {
                    return last;
                }
            }
            other => panic!("expected PaneContent, got {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("pane {pane_id} never contained {needle:?}: {last}");
}

#[test]
fn attach_sync_input_fans_out_then_isolates() {
    let socket = socket_path();
    let _server = start_server_shell(&socket, "printf 'READY\\n'; exec /bin/sh");
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let ControlResponseData::ClientRegistered { client_id } =
        ctl.request(|request_id| ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id,
        })
    else {
        panic!("expected ClientRegistered");
    };
    let ControlResponseData::Snapshot { snapshot } =
        ctl.request(|request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        })
    else {
        panic!("expected Snapshot");
    };
    let window = snapshot
        .sessions
        .first()
        .and_then(|session| session.windows.first())
        .expect("window");
    let window_id = window.id;
    let pane_a = window.panes[0].id;
    let split = ctl.request(|request_id| ControlRequest::Split {
        version: PROTOCOL_VERSION,
        request_id,
        window_id,
        target_pane_id: pane_a,
        axis: AxisWire::Vertical,
        ratio: 0.5,
        spawn: sh_spawn(),
        client_id: None,
    });
    assert!(
        matches!(split, ControlResponseData::Mutation { .. }),
        "{split:?}"
    );
    let ControlResponseData::Snapshot { snapshot } =
        ctl.request(|request_id| ControlRequest::Snapshot {
            version: PROTOCOL_VERSION,
            request_id,
        })
    else {
        panic!("expected Snapshot");
    };
    let pane_b = snapshot.sessions[0].windows[0]
        .panes
        .iter()
        .map(|pane| pane.id)
        .find(|id| *id != pane_a)
        .expect("sibling pane");

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "READY", Duration::from_secs(3));

    writer.write_all(&[0x1c, b's']).expect("toggle sync on");
    let _ = writer.flush();
    wait_transcript(&transcript, "[sync]", Duration::from_secs(3));
    let hay = transcript.lock().expect("transcript").clone();
    assert!(
        outer_grid_last_cell(&hay, 80, 24, '[').is_some(),
        "status row should show [sync]"
    );

    writer.write_all(b"echo hi\n").expect("type echo hi");
    let _ = writer.flush();
    let text_a = wait_pane_text(&mut ctl, client_id, pane_a, "hi");
    let text_b = wait_pane_text(&mut ctl, client_id, pane_b, "hi");
    assert!(text_a.contains("hi"), "{text_a}");
    assert!(text_b.contains("hi"), "{text_b}");

    writer.write_all(&[0x1c, b's']).expect("toggle sync off");
    let _ = writer.flush();
    writer.write_all(b"echo one\n").expect("type echo one");
    let _ = writer.flush();
    let text_a = wait_pane_text(&mut ctl, client_id, pane_a, "one");
    assert!(text_a.contains("one"), "{text_a}");
    let mut text_b = String::new();
    for _ in 0..20 {
        match ctl.request(|request_id| ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id: pane_b,
        }) {
            ControlResponseData::PaneContent { content } => {
                text_b = content.lines.join("\n");
            }
            other => panic!("expected PaneContent, got {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !text_b.contains("one"),
        "sync off must not reach pane B: {text_b}"
    );

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn attach_paints_guest_status_in_chrome() {
    let socket = socket_path();
    let _server = start_server_shell(&socket, "printf '\\n\\n\\nREADY\\n'; exec /bin/cat");
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);
    let status = "build ok 20 chars...";
    assert_eq!(status.len(), 20);
    let set = ctl.request(|request_id| ControlRequest::SetPaneStatus {
        version: PROTOCOL_VERSION,
        request_id,
        pane_id,
        text: Some(status.into()),
    });
    assert!(
        matches!(set, ControlResponseData::Mutation { .. }),
        "{set:?}"
    );
    let _ = client_id;

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, status, Duration::from_secs(3));
    let hay = transcript.lock().expect("transcript").clone();
    // Live chrome is a top-right overlay (same class as [sync]). CSI column
    // is `cols - len` (1-based), matching the existing sync chip.
    let start = 80usize.saturating_sub(status.len()).saturating_sub(1);
    for (i, ch) in status.chars().enumerate() {
        assert_eq!(
            grid_cell(&hay, 80, 24, 0, start + i),
            ch,
            "chrome col {}",
            start + i
        );
    }
    assert_ne!(
        grid_cell(&hay, 80, 24, 23, 0),
        'b',
        "must not CSI-K last guest row with status"
    );

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let wait_status = child.wait().expect("wait attach");
    assert!(wait_status.success(), "attach exit {wait_status:?}");
}

#[test]
fn two_attaches_share_output_and_block_held_input() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);

    let (pair_a, mut child_a, transcript_a) = spawn_attach_pty(&socket, 24, 80);
    drop(pair_a.slave);
    let mut writer_a = pair_a.master.take_writer().expect("pty writer a");
    wait_transcript(&transcript_a, "READY", Duration::from_secs(3));

    let (pair_b, mut child_b, transcript_b) = spawn_attach_pty(&socket, 30, 100);
    drop(pair_b.slave);
    let mut writer_b = pair_b.master.take_writer().expect("pty writer b");
    wait_transcript(&transcript_b, "READY", Duration::from_secs(3));

    writer_a.write_all(b"echo a\n").expect("type echo a");
    let _ = writer_a.flush();
    let text = wait_pane_text(&mut ctl, client_id, pane_id, "echo a");
    assert!(text.contains("echo a"), "{text}");
    wait_transcript(&transcript_b, "echo a", Duration::from_secs(3));
    // Idle release is 750 ms. Wait so the TestClient can take the lease.
    std::thread::sleep(Duration::from_millis(850));

    let acq = ctl.request(|request_id| ControlRequest::AcquireLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
    assert!(matches!(acq, ControlResponseData::Lease { .. }), "{acq:?}");

    writer_b.write_all(b"nope\n").expect("type nope");
    let _ = writer_b.flush();
    wait_transcript(&transcript_b, "held", Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(200));
    let ControlResponseData::PaneContent { content } =
        ctl.request(|request_id| ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        })
    else {
        panic!("expected PaneContent");
    };
    let joined = content.lines.join("\n");
    assert!(
        !joined.contains("nope"),
        "held client must not write: {joined}"
    );

    let _ = ctl.request(|request_id| ControlRequest::ReleaseLease {
        version: PROTOCOL_VERSION,
        request_id,
        client_id,
        pane_id,
    });
    std::thread::sleep(Duration::from_millis(100));
    writer_b.write_all(b"yes\n").expect("type yes");
    let _ = writer_b.flush();
    let text = wait_pane_text(&mut ctl, client_id, pane_id, "yes");
    assert!(text.contains("yes"), "{text}");

    writer_a.write_all(&[0x1c, b'd']).expect("detach a");
    let _ = writer_a.flush();
    wait_transcript(&transcript_a, "[detached]", Duration::from_secs(3));
    let status_a = child_a.wait().expect("wait a");
    assert!(status_a.success(), "attach A exit {status_a:?}");

    writer_b.write_all(b"after\n").expect("type after");
    let _ = writer_b.flush();
    wait_transcript(&transcript_b, "after", Duration::from_secs(3));

    writer_b.write_all(&[0x1c, b'd']).expect("detach b");
    let _ = writer_b.flush();
    wait_transcript(&transcript_b, "[detached]", Duration::from_secs(3));
    let status_b = child_b.wait().expect("wait b");
    assert!(status_b.success(), "attach B exit {status_b:?}");
}

#[test]
fn read_only_attach_drops_keys_and_shows_ro_chip() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut ctl = TestClient::connect(&socket);
    let (client_id, pane_id) = register_and_snapshot(&mut ctl);

    let (pair, mut child, transcript) = spawn_attach_pty_args(&socket, 24, 80, &["--read-only"]);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "[ro]", Duration::from_secs(3));

    writer.write_all(b"secret\n").expect("type secret");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(300));
    let ControlResponseData::PaneContent { content } =
        ctl.request(|request_id| ControlRequest::ReadPane {
            version: PROTOCOL_VERSION,
            request_id,
            client_id,
            pane_id,
        })
    else {
        panic!("expected PaneContent");
    };
    let joined = content.lines.join("\n");
    assert!(
        !joined.contains("secret"),
        "read-only must not write: {joined}"
    );

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

#[test]
fn attach_ctrl_backslash_n_switches_session_chrome() {
    let socket = socket_path();
    let _server = start_server(&socket);
    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    for (name, marker) in [("alpha", "ALPHA99"), ("beta", "BETA99")] {
        let created = ctl.request(|request_id| ControlRequest::CreateSession {
            version: PROTOCOL_VERSION,
            request_id,
            name: name.into(),
            spawn: SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec!["-c".into(), format!("printf '{marker}\\n'; exec cat")],
                cwd: None,
                env: Default::default(),
            },
            cols: None,
            rows: None,
            agent_id: None,
            headless: false,
        });
        match created {
            ControlResponseData::Session { .. } => {}
            other => panic!("create {name}: {other:?}"),
        }
    }

    let (pair, mut child, transcript) =
        spawn_attach_pty_args(&socket, 24, 80, &["--session", "alpha"]);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "ALPHA99", Duration::from_secs(5));
    writer.write_all(&[0x1c, b'n']).expect("next session");
    let _ = writer.flush();
    wait_transcript(&transcript, "BETA99", Duration::from_secs(5));
    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
}

/// PT-113: TTY attach must not issue a 50 ms ReadPaneStyled cadence while
/// the pane is idle. The count file is written on detach.
#[test]
fn interactive_idle_does_not_poll_readpane_styled() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let count_path = std::env::temp_dir().join(format!(
        "pt113-reads-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_pmux-attach"));
    clear_pty_env(&mut cmd);
    cmd.arg("--socket");
    cmd.arg(socket.as_os_str());
    cmd.env("PRISMATTYC_ATTACH_READ_COUNT", count_path.as_os_str());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn attach on pty");
    drop(pair.slave);
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let buf = Arc::clone(&transcript);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf
                    .lock()
                    .expect("transcript")
                    .extend_from_slice(&chunk[..n]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(_) => break,
            }
        }
    });
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "READY", Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(400));
    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");
    let raw = std::fs::read_to_string(&count_path).unwrap_or_default();
    let _ = std::fs::remove_file(&count_path);
    let n: u64 = raw.trim().parse().unwrap_or(u64::MAX);
    assert!(
        n <= 4,
        "idle attach issued {n} ReadPaneStyled calls; event-driven path should stay near 1"
    );
}

fn snapshot_window_size(client: &mut TestClient) -> (u32, u32) {
    match client.request(|request_id| ControlRequest::Snapshot {
        version: PROTOCOL_VERSION,
        request_id,
    }) {
        ControlResponseData::Snapshot { snapshot } => {
            let bounds = &snapshot.sessions[0].windows[0].bounds;
            (bounds.cols, bounds.rows)
        }
        other => panic!("snapshot: {other:?}"),
    }
}

/// PT-288: killing pmuxd mid-Resize must log Err, not treat the op as success.
#[test]
fn resize_after_pmuxd_dies_logs_request_error() {
    let socket = socket_path();
    let mut server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 24, 80);
    drop(pair.slave);
    wait_transcript(&transcript, "READY", Duration::from_secs(5));

    let _ = server.child.kill();
    let _ = server.child.wait();

    pair.master
        .resize(PtySize {
            rows: 12,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize pty");

    wait_transcript(
        &transcript,
        "pmux-attach: Resize failed pane=",
        test_time_budget(Duration::from_secs(5)),
    );
    let hay = String::from_utf8_lossy(&transcript.lock().expect("transcript")).into_owned();
    assert!(
        hay.contains("session="),
        "Resize failure must name the session, got {hay:?}"
    );

    if let Ok(mut writer) = pair.master.take_writer() {
        let _ = writer.write_all(&[0x1c, b'd']);
    }
    let _ = child.wait();
}

/// PT-200: a smaller TTY attach shrinks the pane; detach restores host size.
#[test]
fn attach_then_detach_restores_host_geometry() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    assert_eq!(snapshot_window_size(&mut ctl), (80, 24));

    let (pair, mut child, transcript) = spawn_attach_pty(&socket, 12, 40);
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("pty writer");
    wait_transcript(&transcript, "READY", Duration::from_secs(5));

    let start = Instant::now();
    loop {
        if snapshot_window_size(&mut ctl) == (40, 12) {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!(
                "attach did not shrink the pane, got {:?}",
                snapshot_window_size(&mut ctl)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    writer.write_all(&[0x1c, b'd']).expect("detach");
    let _ = writer.flush();
    wait_transcript(&transcript, "[detached]", Duration::from_secs(3));
    let status = child.wait().expect("wait attach");
    assert!(status.success(), "attach exit {status:?}");

    let start = Instant::now();
    loop {
        if snapshot_window_size(&mut ctl) == (80, 24) {
            return;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!(
                "detach did not restore host geometry, got {:?}",
                snapshot_window_size(&mut ctl)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_window_size(client: &mut TestClient, want: (u32, u32), timeout: Duration) {
    let start = Instant::now();
    loop {
        if snapshot_window_size(client) == want {
            return;
        }
        if start.elapsed() > timeout {
            panic!("wanted {want:?}, got {:?}", snapshot_window_size(client));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn geometry_changed_after(client: &mut TestClient, after: u64) -> (u64, usize) {
    match client.request(|request_id| ControlRequest::Events {
        version: PROTOCOL_VERSION,
        request_id,
        after_sequence: after,
        limit: None,
    }) {
        ControlResponseData::Events { batch } => {
            let n = batch
                .events
                .iter()
                .filter(|envelope| matches!(envelope.event, Event::GeometryChanged { .. }))
                .count();
            (batch.current_sequence, n)
        }
        other => panic!("events: {other:?}"),
    }
}

/// PT-200 review: two real TTY attaches — latest wins, keystrokes do not storm.
#[test]
fn two_tty_latest_wins_without_resize_storm() {
    let socket = socket_path();
    let _server = start_server(&socket);
    for _ in 0..50 {
        if attach_json(&socket).contains("READY") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut ctl = TestClient::connect(&socket);
    let _ = ctl.request(|request_id| ControlRequest::RegisterClient {
        version: PROTOCOL_VERSION,
        request_id,
    });
    assert_eq!(snapshot_window_size(&mut ctl), (80, 24));

    let (pair_a, mut child_a, transcript_a) = spawn_attach_pty(&socket, 12, 40);
    drop(pair_a.slave);
    let mut writer_a = pair_a.master.take_writer().expect("pty writer a");
    wait_transcript(&transcript_a, "READY", Duration::from_secs(5));
    wait_window_size(&mut ctl, (40, 12), Duration::from_secs(5));

    let (pair_b, mut child_b, transcript_b) = spawn_attach_pty(&socket, 18, 60);
    drop(pair_b.slave);
    let mut writer_b = pair_b.master.take_writer().expect("pty writer b");
    wait_transcript(&transcript_b, "READY", Duration::from_secs(5));
    wait_window_size(&mut ctl, (60, 18), Duration::from_secs(5));

    let (seq, _) = geometry_changed_after(&mut ctl, 0);
    for _ in 0..12 {
        writer_b.write_all(b"x").expect("type on latest");
        let _ = writer_b.flush();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            snapshot_window_size(&mut ctl),
            (60, 18),
            "keystrokes on the latest TTY must not ping-pong size"
        );
    }
    let (_, storm) = geometry_changed_after(&mut ctl, seq);
    assert_eq!(
        storm, 0,
        "same-size reports from the latest TTY must not emit GeometryChanged"
    );

    // Idle release is 750 ms. Wait so A can take the lease and reclaim.
    std::thread::sleep(Duration::from_millis(850));
    writer_a.write_all(b"y").expect("type on earlier TTY");
    let _ = writer_a.flush();
    wait_window_size(&mut ctl, (40, 12), Duration::from_secs(5));
    for _ in 0..8 {
        writer_a.write_all(b"y").expect("type on earlier TTY");
        let _ = writer_a.flush();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            snapshot_window_size(&mut ctl),
            (40, 12),
            "latest-wins size must stay after more keys"
        );
    }

    writer_a.write_all(&[0x1c, b'd']).expect("detach a");
    let _ = writer_a.flush();
    wait_transcript(&transcript_a, "[detached]", Duration::from_secs(3));
    let status_a = child_a.wait().expect("wait a");
    assert!(status_a.success(), "attach A exit {status_a:?}");

    writer_b.write_all(&[0x1c, b'd']).expect("detach b");
    let _ = writer_b.flush();
    wait_transcript(&transcript_b, "[detached]", Duration::from_secs(3));
    let status_b = child_b.wait().expect("wait b");
    assert!(status_b.success(), "attach B exit {status_b:?}");
}
