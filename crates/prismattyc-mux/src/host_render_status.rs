//! Read-only snapshots of the registered host's render guards.
use crate::host_register::{host_pid_path_from_socket, live_host_pid};
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn status_path(pid_path: &Path) -> PathBuf {
    pid_path.with_extension("render.json")
}

const SNAPSHOT_MAX_AGE_MS: u64 = 5_000;

fn snapshot_is_stale(age_ms: u64) -> bool {
    age_ms > SNAPSHOT_MAX_AGE_MS
}

/// Publish atomically. Only the registered host may write the snapshot.
pub fn publish(pid_path: &Path, pid: u32, status: &Value) -> io::Result<()> {
    if live_host_pid(pid_path) != Some(pid) || status["host_pid"].as_u64() != Some(u64::from(pid)) {
        return Err(io::Error::other("render status host identity mismatch"));
    }
    let path = status_path(pid_path);
    let temporary = path.with_extension(format!("json.{pid}.tmp"));
    fs::write(&temporary, serde_json::to_vec(status)?)?;
    fs::rename(temporary, path)
}

/// Reject old binaries, dead owners, replaced owners, and stale heartbeats.
pub fn read(socket: &Path) -> io::Result<Value> {
    let pid_path = host_pid_path_from_socket(socket);
    let pid =
        live_host_pid(&pid_path).ok_or_else(|| io::Error::other("no registered live host"))?;
    let mut status: Value = serde_json::from_slice(&fs::read(status_path(&pid_path)).map_err(|error| {
        io::Error::new(error.kind(), format!("render status unavailable; the running host must support render-status: {error}"))
    })?)?;
    if status["schema_version"] != 1
        || status["host_pid"].as_u64() != Some(u64::from(pid))
        || live_host_pid(&pid_path) != Some(pid)
    {
        return Err(io::Error::other(
            "render status host identity or schema mismatch",
        ));
    }
    let sampled = status["sampled_at_unix_ms"]
        .as_u64()
        .ok_or_else(|| io::Error::other("render status has no timestamp"))?;
    let age = unix_ms()
        .checked_sub(sampled)
        .ok_or_else(|| io::Error::other("render status timestamp is in the future"))?;
    let all_windows_occluded = status["windows"].as_array().is_some_and(|windows| {
        !windows.is_empty()
            && windows
                .iter()
                .all(|window| window["occluded"].as_bool() == Some(true))
    });
    if snapshot_is_stale(age) && !all_windows_occluded {
        return Err(io::Error::other(format!(
            "render status is stale ({age} ms old)"
        )));
    }
    status["snapshot_stale"] = snapshot_is_stale(age).into();
    status["snapshot_age_ms"] = age.into();
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_register::register_host_pid;

    #[test]
    fn snapshot_staleness_has_strict_five_second_boundary() {
        assert!(!snapshot_is_stale(0));
        assert!(!snapshot_is_stale(4_999));
        assert!(!snapshot_is_stale(SNAPSHOT_MAX_AGE_MS));
        assert!(snapshot_is_stale(SNAPSHOT_MAX_AGE_MS + 1));
    }

    #[test]
    fn query_checks_owner_and_freshness() {
        let dir = std::env::temp_dir().join(format!(
            "pmux-render-status-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("pmux.sock");
        let pid_path = host_pid_path_from_socket(&socket);
        assert!(read(&socket).is_err());
        let pid = std::process::id();
        register_host_pid(&pid_path, pid).unwrap();
        assert!(read(&socket).is_err());
        let mut status = serde_json::json!({"schema_version": 1, "host_pid": pid, "sampled_at_unix_ms": unix_ms(), "windows": []});
        publish(&pid_path, pid, &status).unwrap();
        assert_eq!(read(&socket).unwrap()["host_pid"], pid);
        status["sampled_at_unix_ms"] = 0.into();
        publish(&pid_path, pid, &status).unwrap();
        assert!(read(&socket).unwrap_err().to_string().contains("stale"));
        status["windows"] = serde_json::json!([{ "occluded": true }]);
        publish(&pid_path, pid, &status).unwrap();
        let stale_occluded = read(&socket).unwrap();
        assert_eq!(stale_occluded["snapshot_stale"], true);
        status["host_pid"] = (pid + 1).into();
        assert!(publish(&pid_path, pid, &status).is_err());
        fs::write(status_path(&pid_path), serde_json::to_vec(&status).unwrap()).unwrap();
        assert!(read(&socket).unwrap_err().to_string().contains("identity"));
        fs::remove_dir_all(dir).unwrap();
    }
}
