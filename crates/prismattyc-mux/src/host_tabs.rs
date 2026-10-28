//! Narrow CLI request transport for host-local tab names.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub pid: u32,
    pub generation: String,
    pub views: Vec<View>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub token: u64,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub token: u64,
    pub title: String,
    #[serde(default)]
    pub fixed_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub pid: u32,
    pub generation: String,
    pub view: u64,
    pub tab: u64,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    pub detail: String,
}

fn directory(socket: &Path) -> PathBuf {
    crate::component_restart::directory(socket)
}

fn manifest_path(socket: &Path, pid: u32) -> PathBuf {
    directory(socket).join(format!("host-tabs-{pid}.json"))
}

fn request_path(socket: &Path, id: &str) -> PathBuf {
    directory(socket).join(format!("host-tabs-{id}.request.json"))
}

fn response_path(socket: &Path, id: &str) -> PathBuf {
    directory(socket).join(format!("host-tabs-{id}.response.json"))
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "host tab record exceeds limit");
    Ok(bytes)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("host tab record parent")?;
    fs::create_dir_all(parent)?;
    crate::platform::set_mode(parent, 0o700)?;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), serial));
    let mut file = crate::platform::private_options()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn write_manifest(socket: &Path, manifest: &Manifest) -> Result<()> {
    atomic_json(&manifest_path(socket, manifest.pid), manifest)
}

pub fn remove_manifest(socket: &Path, pid: u32) {
    let _ = fs::remove_file(manifest_path(socket, pid));
}

pub fn read_manifest(socket: &Path, pid: u32) -> Result<Manifest> {
    Ok(serde_json::from_slice(&read_bounded(&manifest_path(
        socket, pid,
    ))?)?)
}

pub fn request(
    socket: &Path,
    manifest: &Manifest,
    view: u64,
    tab: u64,
    name: Option<String>,
) -> Result<Response> {
    ensure!(
        manifest.pid != 0 && !manifest.generation.is_empty(),
        "invalid host tab target"
    );
    ensure!(
        manifest
            .views
            .iter()
            .any(|item| item.token == view && item.tabs.iter().any(|item| item.token == tab)),
        "tab target is not in the selected view"
    );
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!(
        "{}-{}-{serial}",
        std::process::id(),
        crate::host_render_status::unix_ms()
    );
    let request = Request {
        id: id.clone(),
        pid: manifest.pid,
        generation: manifest.generation.clone(),
        view,
        tab,
        name,
    };
    atomic_json(&request_path(socket, &id), &request)?;
    let path = response_path(socket, &id);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = read_bounded(&path) {
            let _ = fs::remove_file(path);
            let response: Response = serde_json::from_slice(&bytes)?;
            ensure!(response.id == id, "host tab response did not match request");
            if !response.ok {
                bail!("{}", response.detail);
            }
            return Ok(response);
        }
        if Instant::now() >= deadline {
            bail!("host did not answer the tab request");
        }
        thread::sleep(Duration::from_millis(40));
    }
}

pub fn take_request(socket: &Path, pid: u32) -> Option<Request> {
    let entries = fs::read_dir(directory(socket)).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("host-tabs-") || !name.ends_with(".request.json") {
            continue;
        }
        let bytes = match read_bounded(&entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = fs::remove_file(entry.path());
                continue;
            }
        };
        let request: Request = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(_) => {
                let _ = fs::remove_file(entry.path());
                continue;
            }
        };
        if request.pid != pid
            || !request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            continue;
        }
        let _ = fs::remove_file(entry.path());
        return Some(request);
    }
    None
}

pub fn respond(
    socket: &Path,
    request: &Request,
    ok: bool,
    detail: impl Into<String>,
) -> Result<()> {
    atomic_json(
        &response_path(socket, &request.id),
        &Response {
            id: request.id.clone(),
            ok,
            detail: detail.into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_fixed_and_follow_focus_names() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-host-tabs-test-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let manifest = Manifest {
            pid: 7,
            generation: "process-generation".into(),
            views: vec![View {
                token: 11,
                tabs: vec![
                    Tab {
                        token: 3,
                        title: "main".into(),
                        fixed_name: None,
                    },
                    Tab {
                        token: 4,
                        title: "fallback".into(),
                        fixed_name: Some("Release Work".into()),
                    },
                ],
            }],
        };
        write_manifest(&socket, &manifest).unwrap();
        assert_eq!(
            read_manifest(&socket, 7).unwrap().views[0].tabs[1]
                .fixed_name
                .as_deref(),
            Some("Release Work")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn name_request_is_bound_to_process_generation_view_and_tab() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-host-tabs-request-test-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mux.sock");
        let manifest = Manifest {
            pid: std::process::id(),
            generation: "process-generation".into(),
            views: vec![View {
                token: 11,
                tabs: vec![Tab {
                    token: 4,
                    title: "fallback".into(),
                    fixed_name: None,
                }],
            }],
        };
        write_manifest(&socket, &manifest).unwrap();
        let worker_socket = socket.clone();
        let worker = std::thread::spawn(move || {
            let request = loop {
                if let Some(request) = take_request(&worker_socket, std::process::id()) {
                    break request;
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(request.generation, "process-generation");
            assert_eq!(request.view, 11);
            assert_eq!(request.tab, 4);
            assert_eq!(request.name.as_deref(), Some("Release Work"));
            respond(&worker_socket, &request, true, "tab name updated").unwrap();
        });
        let response = request(&socket, &manifest, 11, 4, Some("Release Work".into())).unwrap();
        worker.join().unwrap();
        assert_eq!(response.detail, "tab name updated");
        fs::remove_dir_all(dir).unwrap();
    }
}
