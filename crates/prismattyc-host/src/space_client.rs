// SPDX-License-Identifier: MPL-2.0
//! Space file reads, team describe, and autosave run on one worker thread.
//! The main thread submits a job and applies the report on a later pump.
//! Drop joins the worker. It does not call `measure_subprocess_wait`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, ThreadId};

use prismattyc_mux::team_attention::AttentionRequest;
use prismattyc_mux::SavedSpace;
use prismattyc_mux::Snapshot;

use crate::mux::Wake;

enum Job {
    Poll { dir: PathBuf, name: String },
    Save(SaveJob),
    Refresh(RefreshJob),
}

enum Kind {
    Poll,
    Save,
    Refresh,
}

/// `pmux space save` arguments captured on the main thread after the
/// attach layout is already on disk.
pub(crate) struct SaveJob {
    pub pmux: PathBuf,
    pub name: String,
    pub view_path: Option<PathBuf>,
}

/// One heartbeat of Space chip facts. Paths and the snapshot are copied
/// in so the worker does not read host state or the environment.
pub(crate) struct RefreshJob {
    pub dir: PathBuf,
    pub names: Vec<String>,
    pub snapshot: Snapshot,
    pub requests: Vec<AttentionRequest>,
    pub now_ms: u64,
    pub current: Option<String>,
    pub owner: Option<String>,
}

pub(crate) enum Report {
    Poll {
        name: String,
        space: Option<SavedSpace>,
        #[allow(dead_code)]
        thread: ThreadId,
    },
    Save {
        ok: bool,
        #[cfg_attr(not(test), allow(dead_code))]
        thread: ThreadId,
    },
    Refresh {
        pane_names: HashMap<String, Vec<String>>,
        attention: HashMap<String, usize>,
        resolved: Option<(String, SavedSpace)>,
        #[allow(dead_code)]
        thread: ThreadId,
    },
}

struct Flags {
    poll: AtomicBool,
    save: AtomicBool,
    refresh: AtomicBool,
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

    pub(crate) fn submit_poll(&self, dir: PathBuf, name: String) -> bool {
        self.enqueue(Job::Poll { dir, name }, Kind::Poll)
    }

    pub(crate) fn submit_save(&self, job: SaveJob) -> bool {
        self.enqueue(Job::Save(job), Kind::Save)
    }

    pub(crate) fn submit_refresh(&self, job: RefreshJob) -> bool {
        self.enqueue(Job::Refresh(job), Kind::Refresh)
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
            Job::Poll { dir, name } => (Kind::Poll, run_poll(dir, name)),
            Job::Save(job) => (Kind::Save, run_save(job)),
            Job::Refresh(job) => (Kind::Refresh, run_refresh(job)),
        };
        let flag = match kind {
            Kind::Poll => &flags.poll,
            Kind::Save => &flags.save,
            Kind::Refresh => &flags.refresh,
        };
        // The report is queued before the flag drops so a settled caller
        // that sees `busy == 0` can drain it.
        let _ = reports.send(report);
        flag.store(false, Ordering::Release);
        flags.busy.fetch_sub(1, Ordering::AcqRel);
        wake();
    }
}

fn run_poll(dir: PathBuf, name: String) -> Report {
    let space = prismattyc_mux::load_space(&dir, &name).ok();
    Report::Poll {
        name,
        space,
        thread: thread::current().id(),
    }
}

fn run_save(job: SaveJob) -> Report {
    let mut command = Command::new(&job.pmux);
    command.arg("space").arg("save").arg(&job.name);
    if let Some(path) = &job.view_path {
        command.arg("--view-path").arg(path);
    }
    let ok = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
        .is_ok_and(|status| status.success());
    Report::Save {
        ok,
        thread: thread::current().id(),
    }
}

fn run_refresh(job: RefreshJob) -> Report {
    let mut pane_names = HashMap::new();
    let mut attention = HashMap::new();
    for name in &job.names {
        let Ok(space) = prismattyc_mux::load_space(&job.dir, name) else {
            continue;
        };
        pane_names.insert(
            name.clone(),
            crate::space_view::pane_names(&space, &job.snapshot),
        );
        let details = prismattyc_mux::space_team::describe(
            name,
            &space,
            Default::default(),
            Some(&job.snapshot),
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
        pane_names,
        attention,
        resolved,
        thread: thread::current().id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

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
            view_path: None,
        }));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "submit blocked for {:?}",
            started.elapsed()
        );
        assert!(!client.submit_save(SaveJob {
            pmux: PathBuf::from("true"),
            name: "demo".into(),
            view_path: None,
        }));
        let caller = thread::current().id();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found = None;
        while Instant::now() < deadline {
            for report in client.drain() {
                if let Report::Save { ok, thread } = report {
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
}
