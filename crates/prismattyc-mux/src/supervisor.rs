//! Hive supervisor client for Prismattyc mux-server (ADR-0037 lane B–C).
//!
//! One long-lived intent: act as an **operator-privileged** Hive control client and
//! emit `supervisor.cell_exited` on pane liveness edges. The send path is
//! **best-effort and non-blocking from the mux's point of view** — notices are
//! enqueued with `try_send`; a full or disconnected queue drops + logs rather than
//! stalling PTY drain or the control loop.
//!
//! Auth rides the operator socket lease (ADR-0023), never a cell token. Distinct from
//! the stream-plane `CellExited` record (Kind 16).
//!
//! ## Environ hygiene (load-bearing)
//!
//! hived's `cell_from_environ` reads `/proc/<pid>/environ`, which is the **initial**
//! environment at `execve` time — `std::env::remove_var` does **not** clear it. If the
//! mux process was started with `HIVE_CELL_TOKEN` in its env (env-fold dogfood path),
//! an in-process connect to `HIVE_SOCKET` is attributed as Cell and OperatorOnly
//! `supervisor.cell_exited` is refused. Each send therefore runs in a short-lived
//! child started with `env_clear()` so the connecting peer's `/proc/.../environ` has
//! no cell token.

use crate::local_socket::UnixStream;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::live::CellExitNotice;

/// Bounded queue so a stuck hived cannot back up the mux forever.
const QUEUE_CAP: usize = 64;
/// Hard cap on a single notice attempt (connect + hello + request + read).
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(200);

/// Hidden argv marker: `pmuxd --internal-hive-supervisor-send` reads one
/// JSON job from stdin, sends it, and exits. Must be the first argument after argv0.
pub const INTERNAL_SEND_ARG: &str = "--internal-hive-supervisor-send";

/// Non-blocking sink for cell-exit notices. Clones cheaply (channel sender).
#[derive(Clone, Debug)]
pub struct SupervisorSink {
    tx: SyncSender<CellExitNotice>,
}

impl SupervisorSink {
    /// Spawn a worker that delivers notices to `hive_socket` using the optional
    /// operator lease. Returns a sink the mux drain path can `try_send` into.
    pub fn spawn(hive_socket: PathBuf, lease: Option<String>) -> Self {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAP);
        thread::Builder::new()
            .name("pmux-hive-supervisor".into())
            .spawn(move || worker_loop(rx, hive_socket, lease))
            .expect("spawn pmux-hive-supervisor worker");
        Self { tx }
    }

    /// Enqueue a notice without blocking. Full/disconnected ⇒ drop (best-effort).
    pub fn try_emit(&self, notice: CellExitNotice) {
        match self.tx.try_send(notice) {
            Ok(()) => {}
            Err(TrySendError::Full(n)) => {
                eprintln!(
                    "prismattyc-mux: supervisor.cell_exited queue full; dropped notice for cell {}",
                    n.cell
                );
            }
            Err(TrySendError::Disconnected(n)) => {
                eprintln!(
                    "prismattyc-mux: supervisor worker gone; dropped notice for cell {}",
                    n.cell
                );
            }
        }
    }
}

fn worker_loop(rx: Receiver<CellExitNotice>, hive_socket: PathBuf, lease: Option<String>) {
    while let Ok(notice) = rx.recv() {
        if let Err(err) = send_cell_exited(&hive_socket, lease.as_deref(), &notice) {
            eprintln!(
                "prismattyc-mux: supervisor.cell_exited failed for cell {}: {err}",
                notice.cell
            );
        }
    }
}

/// Build the JSON-RPC request body for `supervisor.cell_exited` (wire contract B).
pub fn cell_exited_params(notice: &CellExitNotice) -> serde_json::Value {
    let mut status = serde_json::Map::new();
    if let Some(code) = notice.exit_code {
        status.insert("exit_code".into(), serde_json::json!(code));
    }
    if let Some(sig) = notice.signal.as_ref() {
        status.insert("signal".into(), serde_json::json!(sig));
    }
    let mut params = serde_json::Map::new();
    params.insert("cell".into(), serde_json::json!(notice.cell));
    if let Some(pid) = notice.pid {
        params.insert("pid".into(), serde_json::json!(pid));
    }
    params.insert("status".into(), serde_json::Value::Object(status));
    serde_json::Value::Object(params)
}

#[derive(Debug, Serialize, Deserialize)]
struct SendJob {
    hive_socket: PathBuf,
    lease: Option<String>,
    notice: NoticeWire,
}

#[derive(Debug, Serialize, Deserialize)]
struct NoticeWire {
    cell: String,
    pid: Option<u32>,
    exit_code: Option<u32>,
    signal: Option<String>,
}

impl From<&CellExitNotice> for NoticeWire {
    fn from(n: &CellExitNotice) -> Self {
        Self {
            cell: n.cell.clone(),
            pid: n.pid,
            exit_code: n.exit_code,
            signal: n.signal.clone(),
        }
    }
}

impl NoticeWire {
    fn to_notice(&self) -> CellExitNotice {
        CellExitNotice {
            pane_id: 0,
            cell: self.cell.clone(),
            pid: self.pid,
            exit_code: self.exit_code,
            signal: self.signal.clone(),
        }
    }
}

/// Deliver one notice from a **clean-env child** of the current executable.
///
/// Parent may still have `HIVE_CELL_TOKEN` in `/proc/self/environ` (exec-time);
/// the child is `env_clear()`'d so hived cannot cell-attribute the connect.
fn send_cell_exited(
    hive_socket: &Path,
    lease: Option<&str>,
    notice: &CellExitNotice,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let job = SendJob {
        hive_socket: hive_socket.to_path_buf(),
        lease: lease.map(str::to_owned),
        notice: NoticeWire::from(notice),
    };
    let body = serde_json::to_vec(&job).map_err(|e| e.to_string())?;

    let mut child = Command::new(&exe)
        .arg(INTERNAL_SEND_ARG)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn clean sender: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&body)
            .map_err(|e| format!("write job: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait sender: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "clean sender exited {}: {err}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Entry for `pmuxd --internal-hive-supervisor-send` (stdin = one [`SendJob`] JSON).
///
/// Runs in a process whose `/proc/self/environ` has no seat token (`env_clear` at spawn).
pub fn run_internal_send() -> Result<(), String> {
    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .map_err(|e| format!("read job: {e}"))?;
    let job: SendJob = serde_json::from_slice(&raw).map_err(|e| format!("parse job: {e}"))?;
    send_cell_exited_inprocess(
        &job.hive_socket,
        job.lease.as_deref(),
        &job.notice.to_notice(),
    )
}

fn send_cell_exited_inprocess(
    hive_socket: &Path,
    lease: Option<&str>,
    notice: &CellExitNotice,
) -> Result<(), String> {
    let mut stream = UnixStream::connect(hive_socket)
        .map_err(|e| format!("connect {}: {e}", hive_socket.display()))?;
    stream
        .set_read_timeout(Some(ATTEMPT_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(ATTEMPT_TIMEOUT))
        .map_err(|e| format!("set_write_timeout: {e}"))?;

    let mut hello = serde_json::json!({
        "protocol": 2,
        "plane": "control",
        "client": "pmuxd",
    });
    if let Some(token) = lease {
        if !token.is_empty() {
            hello["lease"] = serde_json::Value::String(token.to_string());
        }
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "supervisor.cell_exited",
        "params": cell_exited_params(notice),
    });

    let mut payload = serde_json::to_string(&hello).map_err(|e| e.to_string())?;
    payload.push('\n');
    payload.push_str(&serde_json::to_string(&request).map_err(|e| e.to_string())?);
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut reply = String::new();
    let mut reader = BufReader::new(&stream);
    match reader.read_line(&mut reply) {
        Ok(0) => Ok(()),
        Ok(_) => {
            // Surface OperatorOnly / other refusals so the parent worker logs them.
            if reply.contains("\"error\"") {
                return Err(format!("hived refused: {}", reply.trim()));
            }
            Ok(())
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            Ok(())
        }
        Err(e) => Err(format!("read reply: {e}")),
    }
}

/// Read the operator lease file next to a Hive control socket, if present.
/// Mirrors hive-cli: `<socket-basename>.lease` beside the socket.
pub fn read_operator_lease(hive_socket: &Path) -> Option<String> {
    let name = hive_socket.file_name()?.to_string_lossy().into_owned();
    let path = hive_socket.with_file_name(format!("{name}.lease"));
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_exited_params_omit_absent_fields_and_allow_empty_status() {
        let notice = CellExitNotice {
            pane_id: 1,
            cell: "cell:1@1".into(),
            pid: None,
            exit_code: None,
            signal: None,
        };
        let params = cell_exited_params(&notice);
        assert_eq!(params["cell"], "cell:1@1");
        assert!(params.get("pid").is_none());
        assert_eq!(params["status"], serde_json::json!({}));
    }

    #[test]
    fn cell_exited_params_include_pid_and_status_when_present() {
        let notice = CellExitNotice {
            pane_id: 2,
            cell: "cell:2@4".into(),
            pid: Some(4242),
            exit_code: Some(0),
            signal: Some("Terminated".into()),
        };
        let params = cell_exited_params(&notice);
        assert_eq!(params["pid"], 4242);
        assert_eq!(params["status"]["exit_code"], 0);
        assert_eq!(params["status"]["signal"], "Terminated");
    }

    fn sample_notice() -> CellExitNotice {
        CellExitNotice {
            pane_id: 1,
            cell: "cell:1@1".into(),
            pid: Some(9),
            exit_code: Some(0),
            signal: None,
        }
    }

    fn temp_hive_sock() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pt231-hive-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hive.sock");
        (dir, sock)
    }

    #[test]
    fn send_cell_exited_inprocess_connect_error() {
        let err = send_cell_exited_inprocess(
            Path::new("/no/such/pt231-hive.sock"),
            None,
            &sample_notice(),
        )
        .unwrap_err();
        assert!(err.contains("connect"), "{err}");
    }

    #[test]
    fn send_cell_exited_inprocess_writes_lease_and_accepts_eof() {
        use crate::local_socket::UnixListener;
        let (dir, sock) = temp_hive_sock();
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut hello = String::new();
            reader.read_line(&mut hello).unwrap();
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let _ = tx.send((hello, request));
        });
        send_cell_exited_inprocess(&sock, Some("lease-token"), &sample_notice()).unwrap();
        let (hello, request) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(hello.contains("lease-token"), "{hello}");
        assert!(request.contains("supervisor.cell_exited"), "{request}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn send_cell_exited_inprocess_omits_empty_lease_and_surfaces_error() {
        use crate::local_socket::UnixListener;
        let (dir, sock) = temp_hive_sock();
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut hello = String::new();
            reader.read_line(&mut hello).unwrap();
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            stream.write_all(b"{\"error\":\"OperatorOnly\"}\n").unwrap();
            let _ = tx.send(hello);
        });
        let err = send_cell_exited_inprocess(&sock, Some(""), &sample_notice()).unwrap_err();
        assert!(err.contains("hived refused"), "{err}");
        let hello = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!hello.contains("lease"), "{hello}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
