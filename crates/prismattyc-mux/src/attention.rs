//! Hive ↔ Prismattyc mail loop (ADR-0039).
//!
//! Forward: `cells.list` + `attention.peek` → join pane by `bound_pid` →
//! `MailAttentionSet` (Hive `queue_rev`) → `InjectMail` → `inject.report`.
//! Reverse: when peek is drained, clear `pane.mail` so attach/host drop the
//! letter. Operator RPC only — does not watch `hive.db`.

use crate::local_socket::UnixStream;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::control::{
    ControlPlane, ControlRequest, ControlResponseBody, ControlResponseData, MailInjectOutcome,
    PROTOCOL_VERSION,
};
use crate::supervisor::read_operator_lease;

const PEEK_TIMEOUT: Duration = Duration::from_millis(400);
const TICK: Duration = Duration::from_millis(250);

/// `true` when Hive says this cell's inbox is empty.
pub(crate) fn hive_attention_is_drained(peek: &Value) -> bool {
    if peek.get("found").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    peek.get("depth").and_then(Value::as_u64) == Some(0)
}

/// HIVE_SOCKET, else `$XDG_RUNTIME_DIR/hive/control.sock` when it exists.
fn hive_control_socket() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("HIVE_SOCKET") {
        let path = PathBuf::from(raw.trim());
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = Path::new(&base).join("hive").join("control.sock");
    path.exists().then_some(path)
}

/// hived `attention.peek` allows only `cell` (additionalProperties: false).
fn peek_request(cell: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "attention.peek",
        "params": { "cell": cell },
    })
}

fn hive_rpc(socket: &Path, lease: Option<&str>, method: &str, params: Value) -> Option<Value> {
    let mut stream = UnixStream::connect(socket).ok()?;
    let _ = stream.set_read_timeout(Some(PEEK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PEEK_TIMEOUT));
    let mut hello = serde_json::json!({
        "protocol": 2,
        "plane": "control",
        "client": "prismattyc-mux-attention",
    });
    if let Some(token) = lease.filter(|t| !t.is_empty()) {
        hello["lease"] = Value::String(token.to_string());
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut payload = serde_json::to_string(&hello).ok()?;
    payload.push('\n');
    payload.push_str(&serde_json::to_string(&request).ok()?);
    payload.push('\n');
    stream.write_all(payload.as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).ok()?;
    let parsed: Value = serde_json::from_str(reply.trim()).ok()?;
    if parsed.get("error").is_some() {
        return None;
    }
    parsed.get("result").cloned()
}

fn peek_cell(socket: &Path, lease: Option<&str>, cell: &str) -> Option<Value> {
    hive_rpc(
        socket,
        lease,
        "attention.peek",
        peek_request(cell)["params"].clone(),
    )
}

fn list_cell_ids(socket: &Path, lease: Option<&str>) -> Vec<String> {
    let Some(result) = hive_rpc(socket, lease, "cells.list", serde_json::json!({})) else {
        return Vec::new();
    };
    let rows = result
        .get("items")
        .or_else(|| result.get("rows"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            row.get("id")
                .or_else(|| row.get("cell"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn gen_from_cell(cell: &str) -> u64 {
    cell.rsplit('@')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn remaining_attempts(peek: &Value) -> u32 {
    let used = peek.get("attempts").and_then(Value::as_u64).unwrap_or(0);
    3u32.saturating_sub(used as u32)
}

fn next_req() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn register_worker(plane: &Mutex<ControlPlane>) -> Option<u64> {
    let mut guard = plane
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard
        .handle(ControlRequest::RegisterClient {
            version: PROTOCOL_VERSION,
            request_id: next_req(),
        })
        .body
    {
        ControlResponseBody::Ok {
            response: ControlResponseData::ClientRegistered { client_id },
        } => Some(client_id),
        _ => None,
    }
}

fn apply_hive_campaign(
    plane: &Mutex<ControlPlane>,
    client_id: u64,
    socket: &Path,
    lease: Option<&str>,
    peek: &Value,
) {
    let Some(cell) = peek.get("cell").and_then(Value::as_str) else {
        return;
    };
    let Some(bound_pid) = peek.get("bound_pid").and_then(Value::as_u64) else {
        return;
    };
    let Some(queue_rev) = peek.get("queue_rev").and_then(Value::as_u64) else {
        return;
    };
    let depth = peek.get("depth").and_then(Value::as_u64).unwrap_or(0);
    if depth == 0 {
        return;
    }
    let wake = peek.get("wake").and_then(Value::as_str).unwrap_or("");
    let gen = gen_from_cell(cell);
    let pane_id = {
        let guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.pane_for_bound_pid(bound_pid as u32)
    };
    let Some(pane_id) = pane_id else {
        return;
    };
    {
        let mut guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = guard.handle(ControlRequest::MailAttentionSet {
            version: PROTOCOL_VERSION,
            request_id: next_req(),
            client_id,
            pane_id,
            cell: cell.to_string(),
            gen,
            queue_rev,
            depth: depth.min(u32::MAX as u64) as u32,
            wake: None,
            bound_pid: Some(bound_pid as u32),
        });
    }
    if matches!(wake, "stuck" | "exhausted") || remaining_attempts(peek) == 0 {
        return;
    }
    let outcome = {
        let mut guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard
            .handle(ControlRequest::InjectMail {
                version: PROTOCOL_VERSION,
                request_id: next_req(),
                client_id,
                pane_id,
                queue_rev,
                remaining_attempts: remaining_attempts(peek),
            })
            .body
        {
            ControlResponseBody::Ok {
                response: ControlResponseData::MailInject { outcome, .. },
            } => Some(outcome),
            _ => None,
        }
    };
    let result = match outcome {
        Some(MailInjectOutcome::Wrote) => "wrote",
        Some(MailInjectOutcome::Stuck) => "stuck",
        _ => return,
    };
    let _ = hive_rpc(
        socket,
        lease,
        "inject.report",
        serde_json::json!({
            "schema": 1,
            "cell": cell,
            "bound_pid": bound_pid,
            "queue_rev": queue_rev,
            "result": result,
        }),
    );
}

fn tick(plane: &Mutex<ControlPlane>, socket: &Path, lease: Option<&str>, client_id: Option<u64>) {
    if let Some(client_id) = client_id {
        for cell in list_cell_ids(socket, lease) {
            let Some(peek) = peek_cell(socket, lease, &cell) else {
                continue;
            };
            if hive_attention_is_drained(&peek) {
                continue;
            }
            apply_hive_campaign(plane, client_id, socket, lease, &peek);
        }
    }
    let targets = {
        let guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.lit_mail_cells()
    };
    for (pane_id, cell) in targets {
        let Some(peek) = peek_cell(socket, lease, &cell) else {
            continue;
        };
        if !hive_attention_is_drained(&peek) {
            continue;
        }
        let mut guard = plane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = guard.clear_mail_after_hive_drain(pane_id);
    }
}

/// Poll Hive attention until `stop`.
pub(crate) fn spawn_reconciler(
    plane: Arc<Mutex<ControlPlane>>,
    stop: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    let socket = hive_control_socket()?;
    let lease = read_operator_lease(&socket);
    let client_id = register_worker(&plane);
    thread::Builder::new()
        .name("prism-hive-attention".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                tick(&plane, &socket, lease.as_deref(), client_id);
                thread::sleep(TICK);
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::hive_attention_is_drained;
    use serde_json::json;

    #[test]
    fn found_false_is_drained() {
        assert!(hive_attention_is_drained(
            &json!({"schema": 1, "found": false})
        ));
    }

    #[test]
    fn depth_zero_is_drained() {
        assert!(hive_attention_is_drained(&json!({
            "schema": 1,
            "bound_pid": 1,
            "queue_rev": 4,
            "depth": 0
        })));
    }

    #[test]
    fn peek_params_are_cell_only() {
        let req = super::peek_request("43@1");
        let params = req["params"].as_object().expect("params");
        assert_eq!(params.keys().collect::<Vec<_>>(), ["cell"]);
        assert_eq!(params["cell"], "43@1");
        assert_eq!(req["method"], "attention.peek");
    }

    #[test]
    fn positive_depth_stays_lit() {
        assert!(!hive_attention_is_drained(&json!({
            "schema": 1,
            "found": true,
            "bound_pid": 9,
            "queue_rev": 2,
            "depth": 1
        })));
    }

    #[test]
    fn gen_from_full_and_short_cell() {
        assert_eq!(
            super::gen_from_cell("cell:00000000000000000000000000000061@1"),
            1
        );
        assert_eq!(super::gen_from_cell("5f@2"), 2);
    }

    #[test]
    fn remaining_attempts_from_peek() {
        assert_eq!(super::remaining_attempts(&json!({"attempts": 0})), 3);
        assert_eq!(super::remaining_attempts(&json!({"attempts": 2})), 1);
        assert_eq!(super::remaining_attempts(&json!({"attempts": 9})), 0);
    }
}
