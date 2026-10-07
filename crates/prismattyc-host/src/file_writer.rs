//! Latest-wins file writes that must not block the host event loop.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use prismattyc_mux::attach_tabs::AttachTabsFile;
use serde_json::Value;

pub(super) type Stamp = (SystemTime, u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WriteKey {
    RenderStatus(PathBuf),
    AttachTabs(PathBuf),
    ComponentHeartbeat { socket: PathBuf, component: String },
}

enum WriteJob {
    RenderStatus {
        pid_path: PathBuf,
        pid: u32,
        status: Value,
    },
    AttachTabs {
        path: PathBuf,
        file: AttachTabsFile,
    },
    ComponentHeartbeat {
        socket: PathBuf,
        component: String,
    },
}

impl WriteJob {
    fn key(&self) -> WriteKey {
        match self {
            Self::RenderStatus { pid_path, .. } => WriteKey::RenderStatus(pid_path.clone()),
            Self::AttachTabs { path, .. } => WriteKey::AttachTabs(path.clone()),
            Self::ComponentHeartbeat { socket, component } => WriteKey::ComponentHeartbeat {
                socket: socket.clone(),
                component: component.clone(),
            },
        }
    }

    fn completion_kind(&self) -> CompletionKind {
        match self {
            Self::RenderStatus { .. } => CompletionKind::RenderStatus,
            Self::AttachTabs { path, .. } => CompletionKind::AttachTabs { path: path.clone() },
            Self::ComponentHeartbeat { .. } => CompletionKind::ComponentHeartbeat,
        }
    }
}

struct PendingJob {
    id: u64,
    job: WriteJob,
}

#[derive(Default)]
struct QueueState {
    pending: HashMap<WriteKey, PendingJob>,
    active: HashSet<WriteKey>,
    shutdown: bool,
}

struct Shared {
    state: Mutex<QueueState>,
    changed: Condvar,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub(super) struct Handle {
    shared: Arc<Shared>,
}

pub(super) struct FileWriter {
    handle: Handle,
    completed: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CompletionKind {
    RenderStatus,
    AttachTabs { path: PathBuf },
    ComponentHeartbeat,
}

pub(super) struct Completion {
    pub id: u64,
    pub kind: CompletionKind,
    pub result: Result<(), String>,
    pub stamp: Option<Stamp>,
}

impl FileWriter {
    pub(super) fn new(wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
            next_id: AtomicU64::new(1),
        });
        let handle = Handle {
            shared: shared.clone(),
        };
        let (complete, completed) = mpsc::channel();
        let worker_wake = wake.clone();
        let worker = thread::Builder::new()
            .name("prismattyc-file-writer".into())
            .spawn(move || writer_loop(shared, complete, worker_wake))?;
        Ok(Self {
            handle,
            completed,
            worker: Some(worker),
        })
    }

    pub(super) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub(super) fn drain(&self) -> impl Iterator<Item = Completion> + '_ {
        self.completed.try_iter()
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        {
            let mut state = self
                .handle
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.shutdown = true;
        }
        self.handle.shared.changed.notify_one();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("prismattyc-host: file writer thread panicked");
            }
        }
    }
}

impl Handle {
    pub(super) fn attach_tabs_write_pending(&self, path: &Path) -> bool {
        let key = WriteKey::AttachTabs(path.to_path_buf());
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.contains_key(&key) || state.active.contains(&key)
    }

    pub(super) fn wait_for_attach_tabs(&self, path: &Path) {
        self.wait_for(WriteKey::AttachTabs(path.to_path_buf()));
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn wait_for_render_status(&self, pid_path: &Path) {
        self.wait_for(WriteKey::RenderStatus(pid_path.to_path_buf()));
    }

    #[cfg(target_os = "macos")]
    pub(super) fn wait_for_component_heartbeat(&self, socket: &Path, component: &str) {
        self.wait_for(WriteKey::ComponentHeartbeat {
            socket: socket.to_path_buf(),
            component: component.to_string(),
        });
    }

    fn wait_for(&self, key: WriteKey) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.pending.contains_key(&key) || state.active.contains(&key) {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(super) fn render_status(
        &self,
        pid_path: PathBuf,
        pid: u32,
        status: Value,
    ) -> io::Result<u64> {
        self.submit(WriteJob::RenderStatus {
            pid_path,
            pid,
            status,
        })
    }

    pub(super) fn attach_tabs(&self, path: PathBuf, file: AttachTabsFile) -> io::Result<u64> {
        self.submit(WriteJob::AttachTabs { path, file })
    }

    pub(super) fn component_heartbeat(
        &self,
        socket: PathBuf,
        component: impl Into<String>,
    ) -> io::Result<u64> {
        self.submit(WriteJob::ComponentHeartbeat {
            socket,
            component: component.into(),
        })
    }

    fn submit(&self, job: WriteJob) -> io::Result<u64> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let key = job.key();
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "file writer is shutting down",
            ));
        }
        queue_latest(&mut state.pending, key, PendingJob { id, job });
        self.shared.changed.notify_one();
        Ok(id)
    }
}

fn queue_latest(
    pending: &mut HashMap<WriteKey, PendingJob>,
    key: WriteKey,
    job: PendingJob,
) -> Option<u64> {
    pending.insert(key, job).map(|superseded| superseded.id)
}

fn writer_loop(
    shared: Arc<Shared>,
    complete: mpsc::Sender<Completion>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let batch = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.pending.is_empty() && !state.shutdown {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.pending.is_empty() && state.shutdown {
                return;
            }
            let jobs = state
                .pending
                .drain()
                .map(|(_, job)| job)
                .collect::<Vec<_>>();
            state
                .active
                .extend(jobs.iter().map(|pending| pending.job.key()));
            jobs
        };
        for pending in batch {
            let key = pending.job.key();
            if complete.send(write_one(pending)).is_err() {
                return;
            }
            shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active
                .remove(&key);
            shared.changed.notify_all();
            wake();
        }
    }
}

fn write_one(pending: PendingJob) -> Completion {
    let kind = pending.job.completion_kind();
    let (result, stamp) = match pending.job {
        WriteJob::RenderStatus {
            pid_path,
            pid,
            status,
        } => (
            prismattyc_mux::host_render_status::publish(&pid_path, pid, &status)
                .map_err(|error| error.to_string()),
            None,
        ),
        WriteJob::AttachTabs { path, file } => {
            let result =
                prismattyc_mux::attach_tabs::save(&path, &file).map_err(|error| error.to_string());
            let stamp = result.is_ok().then(|| file_stamp(&path)).flatten();
            (result, stamp)
        }
        WriteJob::ComponentHeartbeat { socket, component } => (
            prismattyc_mux::component_restart::register(&socket, &component)
                .map_err(|error| error.to_string()),
            None,
        ),
    };
    Completion {
        id: pending.id,
        kind,
        result,
        stamp,
    }
}

fn file_stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::host_pid_path_from_socket;
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "prismattyc-file-writer-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn pending_write_is_replaced_by_newest_for_same_path() {
        let path = PathBuf::from("attach-tabs.json");
        let key = WriteKey::AttachTabs(path.clone());
        let first = PendingJob {
            id: 1,
            job: WriteJob::AttachTabs {
                path: path.clone(),
                file: AttachTabsFile::default(),
            },
        };
        let latest_file = AttachTabsFile {
            active_tab: 7,
            ..Default::default()
        };
        let latest = PendingJob {
            id: 2,
            job: WriteJob::AttachTabs {
                path,
                file: latest_file.clone(),
            },
        };
        let mut pending = HashMap::new();

        assert_eq!(queue_latest(&mut pending, key.clone(), first), None);
        assert_eq!(queue_latest(&mut pending, key.clone(), latest), Some(1));

        assert_eq!(pending.len(), 1);
        let PendingJob { id, job } = pending.remove(&key).unwrap();
        assert_eq!(id, 2);
        let WriteJob::AttachTabs { file, .. } = job else {
            panic!("latest attach-tabs write was replaced with a different job kind");
        };
        assert_eq!(file, latest_file);
    }

    #[test]
    fn writer_persists_attach_render_status_and_component_heartbeat() {
        let dir = test_dir("writes");
        let socket = dir.join("pmux.sock");
        let pid_path = host_pid_path_from_socket(&socket);
        let pid = std::process::id();
        prismattyc_mux::register_host_pid(&pid_path, pid).unwrap();
        let status = json!({
            "schema_version": 1,
            "host_pid": pid,
            "sampled_at_unix_ms": prismattyc_mux::host_render_status::unix_ms(),
            "writer_test": "render-status-written"
        });
        let attach_path = dir.join("pmux.attach-tabs.json");
        let attach_file = AttachTabsFile {
            active_tab: 3,
            ..Default::default()
        };
        let writer = FileWriter::new(Arc::new(|| {})).unwrap();
        let handle = writer.handle();
        let render_id = handle
            .render_status(pid_path.clone(), pid, status.clone())
            .unwrap();
        let attach_id = handle
            .attach_tabs(attach_path.clone(), attach_file.clone())
            .unwrap();
        let heartbeat_id = handle.component_heartbeat(socket.clone(), "host").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut completions = HashMap::new();
        while completions.len() < 3 && std::time::Instant::now() < deadline {
            for completion in writer.drain() {
                completions.insert(completion.id, completion);
            }
            thread::yield_now();
        }

        let render = completions.remove(&render_id).unwrap();
        assert_eq!(render.kind, CompletionKind::RenderStatus);
        assert_eq!(render.result, Ok(()));
        assert_eq!(
            prismattyc_mux::host_render_status::read(&socket).unwrap()["writer_test"],
            "render-status-written"
        );

        let attach = completions.remove(&attach_id).unwrap();
        assert_eq!(
            attach.kind,
            CompletionKind::AttachTabs {
                path: attach_path.clone()
            }
        );
        assert_eq!(attach.result, Ok(()));
        assert_eq!(
            attach.stamp.map(|(_, len)| len),
            Some(fs::metadata(&attach_path).unwrap().len())
        );
        assert_eq!(
            prismattyc_mux::attach_tabs::load(&attach_path).as_ref(),
            Some(&attach_file)
        );

        let heartbeat = completions.remove(&heartbeat_id).unwrap();
        assert_eq!(heartbeat.kind, CompletionKind::ComponentHeartbeat);
        assert_eq!(heartbeat.result, Ok(()));
        let heartbeat_path =
            prismattyc_mux::component_restart::directory(&socket).join(format!("host-{pid}.json"));
        let heartbeat: Value = serde_json::from_slice(&fs::read(heartbeat_path).unwrap()).unwrap();
        assert_eq!(heartbeat["pid"], pid);
        assert_eq!(heartbeat["component"], "host");

        drop(handle);
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }
}
