// SPDX-License-Identifier: MIT
//! Space reads, snapshots, and commands run on one worker thread.
//! The main thread submits a job and applies the report on a later pump.
//! Drop joins the worker. It does not call `measure_subprocess_wait`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, ThreadId};
use std::time::SystemTime;

use prismattyc_mux::team_attention::AttentionRequest;
use prismattyc_mux::SavedSpace;
use prismattyc_mux::Snapshot;

use crate::mux::Wake;

enum Job {
    Poll(PollJob),
    Save(SaveJob),
    Refresh(RefreshJob),
    Command(CommandJob),
}

enum Kind {
    Poll,
    Save,
    Refresh,
    Command,
}

pub(crate) enum Operation {
    Rename { old: String, new: String },
    Delete { name: String },
    Undo { path: PathBuf },
    PaneTitle { session: String, title: String },
}

impl Operation {
    fn args(&self) -> Vec<String> {
        match self {
            Self::Rename { old, new } => {
                vec!["space".into(), "rename".into(), old.clone(), new.clone()]
            }
            Self::Delete { name } => vec!["space".into(), "rm".into(), name.clone()],
            Self::Undo { path } => vec![
                "space".into(),
                "undo".into(),
                path.to_string_lossy().into_owned(),
            ],
            Self::PaneTitle { session, title } => {
                let mut args = vec!["rename-pane".into(), "--session".into(), session.clone()];
                if !title.is_empty() {
                    args.push(title.clone());
                }
                args
            }
        }
    }
}

pub(crate) struct CommandJob {
    pub pmux: PathBuf,
    pub dir: PathBuf,
    pub operation: Operation,
    pub current: Option<String>,
    pub owner: Option<String>,
    pub epoch: u64,
}

pub(crate) struct PollJob {
    pub dir: PathBuf,
    pub name: String,
    pub snapshot: SnapshotInput,
    pub epoch: u64,
}

pub(crate) enum SnapshotInput {
    Cached(Option<(SystemTime, Snapshot)>),
    Fetch(PathBuf),
}

impl SnapshotInput {
    pub(crate) fn capture(client: Option<&crate::snapshot_client::SnapshotClient>) -> Self {
        match client {
            Some(client) => Self::Cached(client.fresh_observed()),
            None => crate::attach_log::mux_socket()
                .ok()
                .map_or(Self::Cached(None), Self::Fetch),
        }
    }

    fn read(self) -> Option<(SystemTime, Snapshot)> {
        match self {
            Self::Cached(snapshot) => snapshot,
            Self::Fetch(socket) => {
                let started = SystemTime::now();
                crate::attach_log::live_snapshot_at(&socket).map(|snapshot| (started, snapshot))
            }
        }
    }
}

/// `pmux space save` arguments captured on the main thread.
/// `view` is the arrangement at claim time, not the shared attach path.
pub(crate) struct SaveJob {
    pub pmux: PathBuf,
    pub name: String,
    pub view: Option<prismattyc_mux::attach_tabs::AttachTabsFile>,
    /// Parent of the owner-only directory that holds the private view.
    /// `None` uses the process temp dir. The view file is not written here.
    pub view_dir: Option<PathBuf>,
    pub generation: u64,
    /// Space visit that claimed this save. Compared when the report arrives.
    pub epoch: u64,
}

/// One heartbeat of Space chip facts, captured without reading host state
/// from the worker. Without a snapshot cache, the worker fetches one.
pub(crate) struct RefreshJob {
    pub dir: PathBuf,
    pub snapshot: SnapshotInput,
    pub requests: Vec<AttentionRequest>,
    pub now_ms: u64,
    pub current: Option<String>,
    pub owner: Option<String>,
    /// Space visit (`space_epoch`) when this refresh was submitted.
    pub generation: u64,
}

pub(crate) enum Report {
    Poll {
        name: String,
        space: Option<SavedSpace>,
        snapshot: Option<Snapshot>,
        epoch: u64,
        #[allow(dead_code)]
        thread: ThreadId,
    },
    Save {
        ok: bool,
        name: String,
        generation: u64,
        epoch: u64,
        #[allow(dead_code)]
        thread: ThreadId,
    },
    Refresh {
        directory: crate::space_rail::Directory,
        snapshot: Option<(SystemTime, Snapshot)>,
        pane_names: HashMap<String, Vec<String>>,
        attention: HashMap<String, usize>,
        resolved: Option<(String, SavedSpace)>,
        generation: u64,
        owner: Option<String>,
        #[allow(dead_code)]
        thread: ThreadId,
    },
    Command {
        job: CommandJob,
        result: Result<(), String>,
        directory: crate::space_rail::Directory,
        resolved: Option<(String, SavedSpace)>,
        #[allow(dead_code)]
        thread: ThreadId,
    },
}

struct Flags {
    poll: AtomicBool,
    save: AtomicBool,
    refresh: AtomicBool,
    command: AtomicBool,
    busy: AtomicUsize,
}

pub(crate) struct Client {
    jobs: Option<Sender<Job>>,
    reports: Receiver<Report>,
    flags: Arc<Flags>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Client {
    pub(crate) fn spawn(wake: Wake) -> Self {
        let (job_tx, job_rx) = mpsc::channel();
        let (report_tx, report_rx) = mpsc::channel();
        let flags = Arc::new(Flags {
            poll: AtomicBool::new(false),
            save: AtomicBool::new(false),
            refresh: AtomicBool::new(false),
            command: AtomicBool::new(false),
            busy: AtomicUsize::new(0),
        });
        let flags_thread = Arc::clone(&flags);
        let thread = thread::Builder::new()
            .name("pmux-space".into())
            .spawn(move || worker(job_rx, report_tx, flags_thread, wake))
            .ok();
        Self {
            jobs: thread.as_ref().map(|_| job_tx),
            reports: report_rx,
            flags,
            thread,
        }
    }

    pub(crate) fn submit_poll(&self, job: PollJob) -> bool {
        self.enqueue(Job::Poll(job), Kind::Poll)
    }

    pub(crate) fn submit_save(&self, job: SaveJob) -> bool {
        self.enqueue(Job::Save(job), Kind::Save)
    }

    pub(crate) fn submit_refresh(&self, job: RefreshJob) -> bool {
        self.enqueue(Job::Refresh(job), Kind::Refresh)
    }

    pub(crate) fn submit_command(&self, job: CommandJob) -> bool {
        self.enqueue(Job::Command(job), Kind::Command)
    }

    pub(crate) fn refresh_busy(&self) -> bool {
        self.flags.refresh.load(Ordering::Acquire)
    }

    /// Linux window tests wait on this. Other builds do not call it.
    #[cfg_attr(not(all(test, target_os = "linux")), allow(dead_code))]
    pub(crate) fn busy(&self) -> bool {
        self.flags.busy.load(Ordering::Acquire) > 0
    }

    pub(crate) fn drain(&self) -> Vec<Report> {
        let mut reports = Vec::new();
        while let Ok(report) = self.reports.try_recv() {
            reports.push(report);
        }
        reports
    }

    fn enqueue(&self, job: Job, kind: Kind) -> bool {
        let Some(jobs) = &self.jobs else {
            return false;
        };
        let flag = self.flag(kind);
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.flags.busy.fetch_add(1, Ordering::AcqRel);
        if jobs.send(job).is_err() {
            self.flags.busy.fetch_sub(1, Ordering::AcqRel);
            flag.store(false, Ordering::Release);
            return false;
        }
        true
    }

    fn flag(&self, kind: Kind) -> &AtomicBool {
        match kind {
            Kind::Poll => &self.flags.poll,
            Kind::Save => &self.flags.save,
            Kind::Refresh => &self.flags.refresh,
            Kind::Command => &self.flags.command,
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Closing the job channel lets the worker finish its current command
        // and leave. Join only here, never on the pump.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn worker(jobs: Receiver<Job>, reports: Sender<Report>, flags: Arc<Flags>, wake: Wake) {
    while let Ok(job) = jobs.recv() {
        let (kind, report) = match job {
            Job::Poll(job) => (Kind::Poll, run_poll(job)),
            Job::Save(job) => (Kind::Save, run_save(job)),
            Job::Refresh(job) => (Kind::Refresh, run_refresh(job)),
            Job::Command(job) => (Kind::Command, run_command(job)),
        };
        let flag = match kind {
            Kind::Poll => &flags.poll,
            Kind::Save => &flags.save,
            Kind::Refresh => &flags.refresh,
            Kind::Command => &flags.command,
        };
        // The report is queued before the flag drops so a settled caller
        // that sees `busy == 0` can drain it.
        let _ = reports.send(report);
        flag.store(false, Ordering::Release);
        flags.busy.fetch_sub(1, Ordering::AcqRel);
        wake();
    }
}

fn run_poll(job: PollJob) -> Report {
    let space = prismattyc_mux::load_space(&job.dir, &job.name).ok();
    Report::Poll {
        name: job.name,
        space,
        snapshot: job.snapshot.read().map(|(_, snapshot)| snapshot),
        epoch: job.epoch,
        thread: thread::current().id(),
    }
}

fn run_save(job: SaveJob) -> Report {
    let failed = Report::Save {
        ok: false,
        name: job.name.clone(),
        generation: job.generation,
        epoch: job.epoch,
        thread: thread::current().id(),
    };
    let private = if let Some(view) = job.view.as_ref() {
        let dir = job.view_dir.clone().unwrap_or_else(std::env::temp_dir);
        // A captured view is required. Falling through without `--view-path`
        // would make `pmux space save` read the shared cache again.
        match write_private_view(&dir, view) {
            Ok(path) => Some(path),
            Err(_) => return failed,
        }
    } else {
        None
    };
    let mut command = Command::new(&job.pmux);
    prismattyc_mux::platform::hide_console(&mut command);
    command.arg("space").arg("save").arg(&job.name);
    if let Some(path) = &private {
        command.arg("--view-path").arg(path);
    }
    let ok = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
        .is_ok_and(|status| status.success());
    if let Some(path) = private {
        let _ = std::fs::remove_file(&path);
        // Only the directory this save created. A file that sits directly in
        // `view_dir` must not take the process temp directory with it.
        if let Some(parent) = path.parent() {
            if parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pmux-save-view-"))
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    Report::Save {
        ok,
        name: job.name,
        generation: job.generation,
        epoch: job.epoch,
        thread: thread::current().id(),
    }
}

/// Write `view` as `view.json` inside a new owner-only directory under `dir`.
/// The directory name is random. Exclusive create does not follow a symlink
/// that already occupies the file path. Callers delete the file and that
/// directory after `pmux space save`.
fn write_private_view(
    dir: &std::path::Path,
    view: &prismattyc_mux::attach_tabs::AttachTabsFile,
) -> std::io::Result<PathBuf> {
    let private_dir = dir.join(random_view_dir_name()?);
    create_private_dir(&private_dir)?;
    let path = private_dir.join("view.json");
    if let Err(error) = write_view_exclusive(&path, view) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&private_dir);
        return Err(error);
    }
    match path.canonicalize() {
        Ok(absolute) => Ok(absolute),
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir(&private_dir);
            Err(error)
        }
    }
}

fn random_view_dir_name() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut name = String::with_capacity("pmux-save-view-".len() + bytes.len() * 2);
    name.push_str("pmux-save-view-");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        name.push(char::from(HEX[(byte >> 4) as usize]));
        name.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    Ok(name)
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        // `mkdir` applies the umask, which can only clear bits. 0o700 stays
        // owner-only.
        builder.mode(0o700);
        // Fails when the name exists, including as a symlink.
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// Create `path` and write `view`. A path that already exists, including a
/// symlink, is an error and is not followed.
fn write_view_exclusive(
    path: &std::path::Path,
    view: &prismattyc_mux::attach_tabs::AttachTabsFile,
) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(view)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn run_refresh(job: RefreshJob) -> Report {
    let directory = crate::space_rail::Directory::read(&job.dir);
    let snapshot = job.snapshot.read();
    let mut pane_names = HashMap::new();
    let mut attention = HashMap::new();
    for name in &directory.names {
        let Ok(space) = prismattyc_mux::load_space(&job.dir, name) else {
            continue;
        };
        pane_names.insert(
            name.clone(),
            snapshot.as_ref().map_or_else(Vec::new, |(_, snapshot)| {
                crate::space_view::pane_names(&space, snapshot)
            }),
        );
        let details = prismattyc_mux::space_team::describe(
            name,
            &space,
            Default::default(),
            snapshot.as_ref().map(|(_, snapshot)| snapshot),
            &job.requests,
            job.now_ms,
        );
        if details.sessions_needing_input > 0 {
            attention.insert(name.clone(), details.sessions_needing_input);
        }
    }
    let resolved = job
        .current
        .as_deref()
        .and_then(|name| crate::space_view::resolve_space(&job.dir, name, job.owner.as_deref()));
    Report::Refresh {
        directory,
        snapshot,
        pane_names,
        attention,
        resolved,
        generation: job.generation,
        owner: job.owner,
        thread: thread::current().id(),
    }
}

fn run_command(job: CommandJob) -> Report {
    let args = job.operation.args();
    let mut command = Command::new(&job.pmux);
    prismattyc_mux::platform::hide_console(&mut command);
    let result = match command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("pmux {} exited {status}", args.join(" "))),
        Err(error) => Err(format!("could not run pmux: {error}")),
    };
    let directory = crate::space_rail::Directory::read(&job.dir);
    let resolved = job
        .current
        .as_deref()
        .and_then(|name| crate::space_view::resolve_space(&job.dir, name, job.owner.as_deref()));
    Report::Command {
        job,
        result,
        directory,
        resolved,
        thread: thread::current().id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn refresh_and_poll_submit_without_waiting_for_a_snapshot_response() {
        use prismattyc_mux::local_socket::UnixListener;

        for poll in [false, true] {
            let dir = scratch_dir(if poll {
                "poll-snapshot"
            } else {
                "refresh-snapshot"
            });
            let socket = PathBuf::from("/tmp").join(format!(
                "space-snapshot-{}-{}.sock",
                std::process::id(),
                SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let saved = SavedSpace {
                version: prismattyc_mux::OWNED_SPACE_VERSION,
                id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
                created_at_unix_ms: Some(1),
                saved_at_unix: 1,
                sessions: Vec::new(),
                tabs: Vec::new(),
                active_tab: 0,
                focused_session: None,
            };
            prismattyc_mux::save_space(&dir, "desk", &saved).unwrap();
            let client = Client::spawn(Arc::new(|| {}));
            let snapshot = SnapshotInput::Fetch(socket.clone());
            assert!(if poll {
                client.submit_poll(PollJob {
                    dir: dir.clone(),
                    name: "desk".into(),
                    snapshot,
                    epoch: 17,
                })
            } else {
                client.submit_refresh(RefreshJob {
                    dir: dir.clone(),
                    snapshot,
                    requests: Vec::new(),
                    now_ms: 1,
                    current: Some("desk".into()),
                    owner: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
                    generation: 17,
                })
            });
            assert!(
                client.drain().is_empty(),
                "submission waited for the unanswered socket"
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "worker did not connect");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            assert!(
                client.drain().is_empty(),
                "the accepted socket has not answered"
            );
            drop(stream);
            let report = loop {
                if let Some(report) = client.drain().into_iter().next() {
                    break report;
                }
                assert!(
                    Instant::now() < deadline,
                    "snapshot failure was not reported"
                );
                thread::sleep(Duration::from_millis(5));
            };
            match report {
                Report::Poll {
                    name,
                    space,
                    snapshot,
                    epoch,
                    thread,
                } => {
                    assert!(poll);
                    assert_eq!(name, "desk");
                    assert_eq!(
                        space.unwrap().id.as_deref(),
                        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    );
                    assert!(snapshot.is_none());
                    assert_eq!(epoch, 17);
                    assert_ne!(thread, thread::current().id());
                }
                Report::Refresh {
                    directory,
                    snapshot,
                    resolved,
                    generation,
                    thread,
                    ..
                } => {
                    assert!(!poll);
                    assert_eq!(directory.names, ["desk"]);
                    assert_eq!(resolved.unwrap().0, "desk");
                    assert!(snapshot.is_none());
                    assert_eq!(generation, 17);
                    assert_ne!(thread, thread::current().id());
                }
                _ => panic!("unexpected report"),
            }
            drop(client);
            drop(listener);
            std::fs::remove_file(socket).unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn menu_commands_return_while_the_helper_is_blocked_and_report_its_result() {
        use std::os::unix::fs::PermissionsExt;

        struct Gate {
            dir: PathBuf,
            release: Option<Sender<()>>,
            watchdog: Option<thread::JoinHandle<()>>,
        }
        impl Drop for Gate {
            fn drop(&mut self) {
                if let Some(release) = self.release.take() {
                    let _ = release.send(());
                }
                if let Some(watchdog) = self.watchdog.take() {
                    watchdog.join().unwrap();
                }
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        for (index, operation, expected, exit) in [
            (
                0,
                Operation::Rename {
                    old: "before".into(),
                    new: "after".into(),
                },
                vec!["space", "rename", "before", "after"],
                0,
            ),
            (
                1,
                Operation::Delete {
                    name: "before".into(),
                },
                vec!["space", "rm", "before"],
                0,
            ),
            (
                2,
                Operation::Undo {
                    path: PathBuf::from("undo.json"),
                },
                vec!["space", "undo", "undo.json"],
                7,
            ),
            (
                3,
                Operation::PaneTitle {
                    session: "7".into(),
                    title: "new title".into(),
                },
                vec!["rename-pane", "--session", "7", "new title"],
                0,
            ),
        ] {
            let client = Client::spawn(Arc::new(|| {}));
            let dir = std::env::temp_dir().join(format!(
                "space-command-{}-{index}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&dir).unwrap();
            let helper = dir.join("pmux-gated");
            std::fs::write(&helper, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf started > \"$0.started\"\nwhile [ ! -f \"$0.release\" ]; do sleep 0.01; done\nexit {exit}\n")).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            let timed_out = Arc::new(AtomicBool::new(false));
            let timeout_flag = Arc::clone(&timed_out);
            let release_path = helper.with_extension("release");
            let (release, wait_release) = mpsc::channel();
            let watchdog = thread::spawn(move || {
                if wait_release.recv_timeout(Duration::from_secs(5)).is_err() {
                    timeout_flag.store(true, Ordering::Release);
                }
                std::fs::write(release_path, b"release").unwrap();
            });
            let gate = Gate {
                dir: dir.clone(),
                release: Some(release),
                watchdog: Some(watchdog),
            };
            assert!(client.submit_command(CommandJob {
                pmux: helper.clone(),
                dir: dir.clone(),
                operation,
                current: None,
                owner: None,
                epoch: 17
            }));
            assert!(
                !timed_out.load(Ordering::Acquire),
                "submitting waited for the blocked command"
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while !helper.with_extension("started").exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                std::fs::read_to_string(helper.with_extension("started")).unwrap(),
                "started"
            );
            assert!(
                client.drain().is_empty(),
                "a blocked helper has no completion"
            );
            assert!(!client.submit_command(CommandJob {
                pmux: helper.clone(),
                dir: dir.clone(),
                operation: Operation::Delete {
                    name: "second".into()
                },
                current: None,
                owner: None,
                epoch: 18
            }));
            gate.release.as_ref().unwrap().send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let report = loop {
                if let Some(report) = client.drain().into_iter().next() {
                    break report;
                }
                assert!(
                    Instant::now() < deadline,
                    "released command did not complete"
                );
                thread::sleep(Duration::from_millis(5));
            };
            let Report::Command {
                job,
                result,
                thread,
                ..
            } = report
            else {
                panic!("expected a command completion")
            };
            assert_eq!(job.epoch, 17);
            assert_ne!(thread, thread::current().id());
            assert_eq!(
                std::fs::read_to_string(helper.with_extension("args"))
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                expected
            );
            if exit == 0 {
                assert_eq!(result, Ok(()));
            } else {
                assert_eq!(
                    result,
                    Err("pmux space undo undo.json exited exit status: 7".into())
                );
            }
            assert!(!timed_out.load(Ordering::Acquire));
            drop(gate);
            drop(client);
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_returns_before_the_command_finishes_on_another_thread() {
        let dir = std::env::temp_dir().join(format!("space-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("pmux-sleep");
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "sleep 0.4").unwrap();
            writeln!(file, "exit 0").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let client = Client::spawn(Arc::new(|| {}));
        let started = Instant::now();
        assert!(client.submit_save(SaveJob {
            pmux: script,
            name: "demo".into(),
            view: None,
            view_dir: None,
            generation: 1,
            epoch: 1,
        }));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "submit blocked for {:?}",
            started.elapsed()
        );
        assert!(!client.submit_save(SaveJob {
            pmux: PathBuf::from("true"),
            name: "demo".into(),
            view: None,
            view_dir: None,
            generation: 2,
            epoch: 1,
        }));
        let caller = thread::current().id();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found = None;
        while Instant::now() < deadline {
            for report in client.drain() {
                if let Report::Save { ok, thread, .. } = report {
                    found = Some((ok, thread));
                }
            }
            if found.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let (ok, thread) = found.expect("save report");
        assert!(ok);
        assert_ne!(thread, caller);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn delayed_save_keeps_the_captured_view_after_the_shared_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "pmux-save-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.json");
        let capture = dir.join("captured.json");
        let used = dir.join("used-path");
        let script = dir.join("pmux-capture");
        let view_a = prismattyc_mux::attach_tabs::AttachTabsFile {
            tabs: vec![prismattyc_mux::attach_tabs::AttachTabRecord {
                title: "review-a".into(),
                sessions: vec!["review-a-1".into()],
                layout: None,
                fixed_name: None,
            }],
            space: Some("review-a".into()),
            ..Default::default()
        };
        let empty_b = prismattyc_mux::attach_tabs::AttachTabsFile {
            tabs: Vec::new(),
            space: Some("review-b".into()),
            ..Default::default()
        };
        prismattyc_mux::attach_tabs::save(&shared, &empty_b).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\npath=\nprev=\nfor arg in \"$@\"; do\n  if [ \"$prev\" = \"--view-path\" ]; then\n    path=$arg\n  fi\n  prev=$arg\ndone\nprintf '%s' \"$path\" > '{}'\ncp \"$path\" '{}'\n",
                used.display(),
                capture.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let report = run_save(SaveJob {
            pmux: script,
            name: "review-a".into(),
            view: Some(view_a),
            view_dir: None,
            generation: 4,
            epoch: 1,
        });
        let Report::Save {
            ok,
            name,
            generation,
            epoch,
            ..
        } = report
        else {
            panic!("save report");
        };
        assert!(ok);
        assert_eq!(name, "review-a");
        assert_eq!(generation, 4);
        assert_eq!(epoch, 1);
        let saved = prismattyc_mux::attach_tabs::load(&capture).expect("captured view");
        assert_eq!(saved.space.as_deref(), Some("review-a"));
        assert_eq!(saved.tabs[0].sessions, ["review-a-1"]);
        let used_path = PathBuf::from(std::fs::read_to_string(&used).unwrap());
        assert_ne!(used_path, shared);
        assert!(used_path.is_absolute());
        assert!(
            !used_path.exists(),
            "private view is removed after the save"
        );
        let private_dir = used_path.parent().expect("private view directory");
        assert!(
            private_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pmux-save-view-")),
            "view is not written directly in the shared temp directory"
        );
        assert!(
            !private_dir.exists(),
            "private view directory is removed after the save"
        );
        let shared_now = prismattyc_mux::attach_tabs::load(&shared).unwrap();
        assert!(shared_now.tabs.is_empty());
        assert_eq!(shared_now.space.as_deref(), Some("review-b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_view_write_failure_does_not_save_the_shared_cache() {
        let dir = std::env::temp_dir().join(format!(
            "pmux-save-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("launched");
        let script = dir.join("pmux-capture");
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").unwrap();
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let view = prismattyc_mux::attach_tabs::AttachTabsFile {
            tabs: vec![prismattyc_mux::attach_tabs::AttachTabRecord {
                title: "review-a".into(),
                sessions: vec!["review-a-1".into()],
                layout: None,
                fixed_name: None,
            }],
            space: Some("review-a".into()),
            ..Default::default()
        };
        let report = run_save(SaveJob {
            pmux: script,
            name: "review-a".into(),
            view: Some(view),
            view_dir: Some(not_a_dir),
            generation: 9,
            epoch: 2,
        });
        let Report::Save {
            ok,
            name,
            generation,
            epoch,
            ..
        } = report
        else {
            panic!("save report");
        };
        assert!(!ok);
        assert_eq!(name, "review-a");
        assert_eq!(generation, 9);
        assert_eq!(epoch, 2);
        assert!(!marker.exists(), "pmux space save must not run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn captured_view() -> prismattyc_mux::attach_tabs::AttachTabsFile {
        prismattyc_mux::attach_tabs::AttachTabsFile {
            tabs: vec![prismattyc_mux::attach_tabs::AttachTabRecord {
                title: "review-a".into(),
                sessions: vec!["review-a-1".into()],
                layout: None,
                fixed_name: None,
            }],
            space: Some("review-a".into()),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_view_write_does_not_follow_a_symlink() {
        let dir = scratch_dir("pmux-save-symlink");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep-me").unwrap();
        let link = dir.join("view.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let error = write_view_exclusive(&link, &captured_view())
            .expect_err("a pre-created symlink must not be followed");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep-me");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_view_is_an_owner_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("pmux-save-private");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let planted = dir.join("view.json");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let path = write_private_view(&dir, &captured_view()).unwrap();
        let parent = path.parent().expect("private directory");
        assert_eq!(path.file_name().unwrap(), "view.json");
        assert_ne!(parent, dir.canonicalize().unwrap());
        let meta = std::fs::symlink_metadata(parent).unwrap();
        assert!(meta.file_type().is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        let saved = prismattyc_mux::attach_tabs::load(&path).expect("written view");
        assert_eq!(saved.space.as_deref(), Some("review-a"));
        assert_eq!(saved.tabs[0].sessions, ["review-a-1"]);
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        assert!(planted.symlink_metadata().unwrap().file_type().is_symlink());
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(parent).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
