//! Cooperative component restart requests. A component handles its own state;
//! the coordinator never signals an arbitrary PID or replays guest input.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub pid: u32,
    pub created_ms: u64,
    pub generation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub pid: u32,
    #[cfg(windows)]
    pub generation: String,
    pub status: String,
    pub detail: String,
    pub version: String,
}

pub fn directory(socket: &Path) -> PathBuf {
    socket.with_extension("components")
}
pub fn request_path(socket: &Path, component: &str, pid: u32) -> PathBuf {
    directory(socket).join(format!("{component}-{pid}.request.json"))
}
pub fn response_path(socket: &Path, id: &str) -> PathBuf {
    directory(socket).join(format!("{id}.response.json"))
}

fn generation() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION.get_or_init(|| {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(4097)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "component record exceeds limit");
    Ok(bytes)
}

pub fn coordinator_lock(socket: &Path) -> Result<std::fs::File> {
    let directory = directory(socket);
    fs::create_dir_all(&directory)?;
    crate::platform::set_mode(&directory, 0o700)?;
    let file = crate::platform::private_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("restart.lock"))?;
    crate::platform::try_lock_exclusive(&file).context("another restart is in progress")?;
    Ok(file)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("component request parent")?;
    fs::create_dir_all(parent)?;
    crate::platform::set_mode(parent, 0o700)?;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = path.with_extension(format!("{}-{serial}.tmp", generation()));
    let mut file = crate::platform::private_options()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn request(socket: &Path, component: &str, pid: u32) -> Result<Request> {
    ensure!(
        matches!(component, "host" | "mcp"),
        "unknown restart component"
    );
    let created_ms = crate::host_render_status::unix_ms();
    let id = format!("{component}-{pid}-{created_ms}-{}", std::process::id());
    let heartbeat: serde_json::Value = serde_json::from_slice(&read_bounded(
        &directory(socket).join(format!("{component}-{pid}.json")),
    )?)?;
    let generation = heartbeat["generation"]
        .as_str()
        .context("component has no generation identity")?
        .to_owned();
    let request = Request {
        id,
        pid,
        created_ms,
        generation,
    };
    atomic_json(&request_path(socket, component, pid), &request)?;
    Ok(request)
}

pub fn take(socket: &Path, component: &str) -> Option<Request> {
    let pid = std::process::id();
    let path = request_path(socket, component, pid);
    let data = read_bounded(&path).ok()?;
    let _ = fs::remove_file(path);
    if data.len() > 4096 {
        return None;
    }
    let request: Request = serde_json::from_slice(&data).ok()?;
    let age = crate::host_render_status::unix_ms().checked_sub(request.created_ms)?;
    (request.pid == pid
        && request.generation == generation()
        && age <= 30_000
        && request
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then_some(request)
}

pub fn respond(socket: &Path, request: &Request, status: &str, detail: &str) -> Result<()> {
    atomic_json(
        &response_path(socket, &request.id),
        &Response {
            id: request.id.clone(),
            pid: request.pid,
            #[cfg(windows)]
            generation: request.generation.clone(),
            status: status.into(),
            detail: detail.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
    )
}

pub fn register(socket: &Path, component: &str) -> Result<()> {
    register_version(socket, component, env!("CARGO_PKG_VERSION"))
}
pub fn register_version(socket: &Path, component: &str, version: &str) -> Result<()> {
    atomic_json(
        &directory(socket).join(format!("{component}-{}.json", std::process::id())),
        &serde_json::json!({"pid":std::process::id(),"component":component,"generation":generation(),"version":version,"supervisor_version":env!("CARGO_PKG_VERSION"),"sampled_ms":crate::host_render_status::unix_ms()}),
    )
}

pub fn live_components(socket: &Path, component: &str) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(directory(socket)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let pid = name
                .strip_prefix(&format!("{component}-"))?
                .strip_suffix(".json")?
                .parse::<u32>()
                .ok()?;
            let value: serde_json::Value =
                serde_json::from_slice(&read_bounded(&entry.path()).ok()?).ok()?;
            let sampled = value["sampled_ms"].as_u64()?;
            (crate::procinfo::pid_alive(pid)
                && value["pid"].as_u64() == Some(pid as u64)
                && crate::host_render_status::unix_ms()
                    .checked_sub(sampled)
                    .is_some_and(|age| age < 5000))
            .then_some(pid)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_targets_generation_and_serializes_coordinators() {
        let root = std::env::temp_dir().join(format!("restart-{}", generation()));
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("mux.sock");
        register(&socket, "host").unwrap();
        assert_eq!(live_components(&socket, "host"), vec![std::process::id()]);
        let lock = coordinator_lock(&socket).unwrap();
        assert!(coordinator_lock(&socket).is_err());
        let request = request(&socket, "host", std::process::id()).unwrap();
        assert_eq!(take(&socket, "host").unwrap().id, request.id);
        assert!(take(&socket, "host").is_none());
        let mut stale = request.clone();
        stale.generation = "old-process".into();
        atomic_json(&request_path(&socket, "host", std::process::id()), &stale).unwrap();
        assert!(take(&socket, "host").is_none());
        respond(&socket, &request, "deferred", "owns a local terminal").unwrap();
        let response: Response =
            serde_json::from_slice(&read_bounded(&response_path(&socket, &request.id)).unwrap())
                .unwrap();
        assert_eq!(response.status, "deferred");
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
}
