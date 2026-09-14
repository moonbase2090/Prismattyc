#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct Supervisor {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Supervisor {
    fn spawn() -> Self {
        let mut process = Command::new(env!("CARGO_BIN_EXE_pmux-mcp"))
            .args(["--as", "supervisor-test", "--supervise"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn supervisor");
        let stdin = process.stdin.take().expect("supervisor stdin");
        let stdout = process.stdout.take().expect("supervisor stdout");
        Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
        }
    }

    fn pid(&self) -> u32 {
        self.process.id().expect("supervisor pid")
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).expect("serialize MCP request");
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .expect("write MCP request");
        self.stdin.flush().await.expect("flush MCP request");
    }

    async fn response(&mut self, expected_id: Value) -> Value {
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(IO_TIMEOUT, self.stdout.read_line(&mut line))
                .await
                .expect("timed out waiting for MCP response")
                .expect("read MCP response");
            assert_ne!(read, 0, "supervisor stdout closed");
            let response: Value = serde_json::from_str(&line).expect("valid MCP response");
            if response.get("id") == Some(&expected_id) {
                return response;
            }
        }
    }

    async fn stop(&mut self) {
        let _ = self.process.kill().await;
        let _ = self.process.wait().await;
    }
}

#[tokio::test]
async fn child_exit_is_hidden_from_the_host_stdio_session() {
    let mut supervisor = Supervisor::spawn();
    supervisor
        .send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "supervisor-test", "version": "1"}
            }
        }))
        .await;
    assert!(supervisor.response(json!(1)).await.get("result").is_some());
    supervisor
        .send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await;
    supervisor
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .await;
    assert!(supervisor.response(json!(2)).await["result"]["tools"].is_array());

    let old_child = only_child_pid(supervisor.pid()).await;
    let status = Command::new("kill")
        .args(["-TERM", &old_child.to_string()])
        .status()
        .await
        .expect("signal child");
    assert!(status.success());
    let replacement = wait_for_replacement(supervisor.pid(), old_child).await;
    assert_ne!(replacement, old_child);

    supervisor
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": {}
        }))
        .await;
    assert!(supervisor.response(json!(3)).await["result"]["tools"].is_array());
    supervisor.stop().await;
}

async fn only_child_pid(parent: u32) -> u32 {
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        let pids = child_pids(parent);
        if let [pid] = pids.as_slice() {
            return *pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected one supervisor child, found {pids:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_replacement(parent: u32, old_child: u32) -> u32 {
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        let child = only_child_pid(parent).await;
        if child != old_child {
            return child;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "supervisor did not replace child {old_child}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Find immediate child PIDs of `parent`. Uses /proc on Linux and pgrep on macOS.
/// Synchronous: the Linux arm has no await, and pgrep is effectively instant,
/// so an async signature only trips `clippy::unused_async` on Linux.
fn child_pids(parent: u32) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{parent}/task/{parent}/children");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
            .collect()
    }
}
