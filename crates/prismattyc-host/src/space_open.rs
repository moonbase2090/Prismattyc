//! Serialize host requests across the CLI child and the host cache apply.

use std::collections::VecDeque;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant, SystemTime};

use crate::attach_tabs::AttachTabsMode;

pub type CacheStamp = Option<(SystemTime, u64)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Create,
    Switch,
    NewWindow,
}

#[derive(Debug)]
pub struct Request {
    pub session_name: Option<String>,
    pub name: String,
    pub mode: Mode,
}

struct Active {
    request: Request,
    child: Child,
    before: CacheStamp,
    applied: Option<bool>,
    exited: Option<(ExitStatus, Instant)>,
    started_at: Instant,
    stderr: Option<std::sync::mpsc::Receiver<String>>,
}

impl Drop for Active {
    fn drop(&mut self) {
        // A closed window must not leave a delayed helper writing its cache.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct Opens {
    active: Option<Active>,
    queue: VecDeque<Request>,
    /// Keep a failed or unobserved result intact until a later cache applies.
    unapplied: bool,
}

pub struct Completion {
    pub name: String,
    pub error: Option<String>,
    pub applied: Option<bool>,
    pub mode: Mode,
    pub session_name: Option<String>,
}

fn capture_stderr(child: &mut Child) -> Option<std::sync::mpsc::Receiver<String>> {
    use std::io::Read;
    let mut stderr = child.stderr.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("space-helper-stderr".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.by_ref().take(4096).read_to_end(&mut bytes);
            let message: String = String::from_utf8_lossy(&bytes)
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            let _ = tx.send(message.trim().to_string());
            // Keep draining after the bounded diagnostic so a noisy helper cannot
            // fill its pipe and stall the UI operation.
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        })
        .ok()?;
    Some(rx)
}

impl Opens {
    pub fn busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    pub fn blocks_persist(&self) -> bool {
        self.busy() || self.unapplied
    }

    pub fn enqueue(&mut self, name: &str, mode: Mode) {
        self.queue.push_back(Request {
            session_name: None,
            name: name.into(),
            mode,
        });
    }

    pub fn next(&mut self) -> Option<Request> {
        if self.active.is_some() {
            return None;
        }
        self.queue.pop_front()
    }

    pub fn enqueue_named(&mut self, name: &str, session_name: String) {
        self.enqueue(name, Mode::Create);
        if let Some(request) = self.queue.back_mut() {
            request.session_name = Some(session_name);
        }
    }

    pub fn started(&mut self, request: Request, mut child: Child, before: CacheStamp) {
        let stderr = capture_stderr(&mut child);
        self.active = Some(Active {
            request,
            child,
            before,
            applied: None,
            exited: None,
            started_at: Instant::now(),
            stderr,
        });
    }

    pub fn cache_applied(
        &mut self,
        stamp: CacheStamp,
        name: Option<&str>,
        mode: AttachTabsMode,
        success: bool,
    ) {
        self.unapplied = !success;
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let expected_mode = match active.request.mode {
            Mode::Create | Mode::Switch => AttachTabsMode::Switch,
            Mode::NewWindow => return,
        };
        if stamp.is_some()
            && stamp != active.before
            && name == Some(active.request.name.as_str())
            && mode == expected_mode
        {
            active.applied = Some(success);
        }
    }

    pub fn poll(&mut self, current: CacheStamp) -> Option<Completion> {
        let active = self.active.as_mut()?;
        if active.exited.is_none() {
            match active.child.try_wait() {
                Ok(Some(status)) => active.exited = Some((status, Instant::now())),
                Ok(None) if active.started_at.elapsed() >= Duration::from_secs(30) => {
                    return Some(self.finish(Some(
                        "pmux timed out after 30 seconds; sessions may have changed".into(),
                    )));
                }
                Ok(None) => return None,
                Err(error) => return Some(self.finish(Some(format!("wait for pmux: {error}")))),
            }
        }
        let (status, exited_at) = active.exited.as_ref().unwrap();
        let failed_without_write =
            !status.success() && current == active.before && active.applied.is_none();
        let error = if !status.success() {
            let detail = active
                .stderr
                .as_ref()
                .and_then(|rx| rx.recv_timeout(Duration::from_millis(20)).ok())
                .filter(|message| !message.is_empty());
            Some(detail.unwrap_or_else(|| format!("pmux exited {status}")))
        } else if active.request.mode == Mode::NewWindow || active.applied == Some(true) {
            None
        } else if active.applied == Some(false) {
            Some("host could not apply the layout".into())
        } else if exited_at.elapsed() >= Duration::from_secs(2) {
            Some("host did not apply the requested layout".into())
        } else {
            return None;
        };
        let previous_unapplied = self.unapplied;
        let completed = self.finish(error);
        if failed_without_write {
            // Validation failed before a cache write. Keep the current view
            // live and preserve only fences from an earlier failed apply.
            self.unapplied = previous_unapplied;
        }
        Some(completed)
    }

    fn finish(&mut self, error: Option<String>) -> Completion {
        let active = self.active.take().unwrap();
        if error.is_some() && active.request.mode != Mode::NewWindow {
            self.unapplied = true;
        }
        Completion {
            name: active.request.name.clone(),
            error,
            applied: active.applied,
            mode: active.request.mode,
            session_name: active.request.session_name.clone(),
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn start(opens: &mut Opens, name: &str, mode: Mode, before: CacheStamp) {
        opens.enqueue(name, mode);
        let request = opens.next().unwrap();
        let child = Command::new("/bin/sh")
            .args(["-c", "read answer"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        opens.started(request, child, before);
    }

    fn exit_child(opens: &mut Opens, success: bool) {
        use std::io::Write;
        let active = opens.active.as_mut().unwrap();
        let mut stdin = active.child.stdin.take().unwrap();
        if success {
            stdin.write_all(b"done\n").unwrap();
        }
        drop(stdin);
        let status = active.child.wait().unwrap();
        active.exited = Some((status, Instant::now() - Duration::from_secs(3)));
    }

    #[test]
    fn dropping_open_kills_reaps_and_prevents_late_cache_writes() {
        use std::io::{BufRead, BufReader};
        let dir = std::env::temp_dir().join(format!("space-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let late = dir.join("late-cache");
        let _ = std::fs::remove_file(&late);
        let mut child=Command::new("python3").args(["-u","-c",
            "import pathlib,sys,time; print('ready',flush=True); time.sleep(0.4); pathlib.Path(sys.argv[1]).write_text('late')"])
            .arg(&late).stdout(Stdio::piped()).spawn().unwrap();
        let pid = child.id();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let mut opens = Opens::default();
        opens.enqueue("cancelled", Mode::Switch);
        let request = opens.next().unwrap();
        opens.started(request, child, None);
        drop(opens);
        assert!(
            matches!(
                rustix::process::waitpid(
                    rustix::process::Pid::from_raw(pid as i32),
                    rustix::process::WaitOptions::NOHANG
                ),
                Err(rustix::io::Errno::CHILD)
            ),
            "helper must already be reaped"
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(!late.exists(), "cancelled helper wrote cache later");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queue_waits_for_apply_and_child_exit_in_either_order() {
        for apply_first in [false, true] {
            let mut opens = Opens::default();
            start(&mut opens, "a", Mode::Switch, None);
            opens.enqueue("b", Mode::Create);
            opens.enqueue("c", Mode::Switch);
            assert!(opens.blocks_persist());
            assert!(opens.next().is_none());
            let stamp = Some((SystemTime::now(), 12));
            if apply_first {
                opens.cache_applied(stamp, Some("a"), AttachTabsMode::Switch, true);
                assert!(
                    opens.poll(None).is_none(),
                    "cache apply cannot reap a live helper"
                );
                assert!(opens.blocks_persist());
                exit_child(&mut opens, true);
            } else {
                exit_child(&mut opens, true);
                opens.active.as_mut().unwrap().exited.as_mut().unwrap().1 = Instant::now();
                assert!(
                    opens.poll(None).is_none(),
                    "exit is not proof of a host apply"
                );
                assert!(opens.blocks_persist());
                opens.cache_applied(stamp, Some("a"), AttachTabsMode::Switch, true);
            }
            let done = opens.poll(None).unwrap();
            assert_eq!(done.name, "a");
            assert!(done.error.is_none());
            assert!(opens.blocks_persist(), "queued requests also fence writes");
            let next = opens.next().unwrap();
            assert_eq!(next.name, "b");
            assert_eq!(next.mode, Mode::Create);
            assert_eq!(opens.next().unwrap().name, "c");
            assert!(!opens.blocks_persist());
        }
    }

    #[test]
    fn old_unrelated_and_wrong_mode_caches_do_not_complete_an_open() {
        let stamp = Some((SystemTime::now(), 12));
        let fresh = Some((SystemTime::now(), 13));
        for (observed, name, mode) in [
            (None, "a", AttachTabsMode::Switch),
            (stamp, "a", AttachTabsMode::Switch),
            (fresh, "b", AttachTabsMode::Switch),
            (fresh, "a", AttachTabsMode::Add),
        ] {
            let mut opens = Opens::default();
            start(&mut opens, "a", Mode::Switch, stamp);
            opens.cache_applied(observed, Some(name), mode, true);
            exit_child(&mut opens, true);
            assert!(opens
                .poll(None)
                .unwrap()
                .error
                .unwrap()
                .contains("did not apply"));
            assert!(opens.blocks_persist());
            opens.cache_applied(fresh, Some("a"), AttachTabsMode::Switch, true);
            assert!(!opens.blocks_persist(), "a later successful apply recovers");
        }
    }

    #[test]
    fn failed_child_or_partial_apply_keeps_the_cache_fenced() {
        for child_success in [false, true] {
            let mut opens = Opens::default();
            start(&mut opens, "a", Mode::Switch, None);
            opens.cache_applied(
                Some((SystemTime::now(), 12)),
                Some("a"),
                AttachTabsMode::Switch,
                false,
            );
            exit_child(&mut opens, child_success);
            assert!(opens.poll(None).unwrap().error.is_some());
            assert!(!opens.busy());
            assert!(opens.blocks_persist());
        }
    }

    #[test]
    fn new_window_finishes_without_requiring_a_shared_cache_write() {
        let mut opens = Opens::default();
        start(&mut opens, "a", Mode::NewWindow, None);
        exit_child(&mut opens, true);
        assert!(opens.poll(None).unwrap().error.is_none());
        assert!(!opens.blocks_persist());
        assert!(opens.poll(None).is_none());
    }

    #[test]
    fn hung_helper_is_reaped_before_queued_retry() {
        let mut opens = Opens::default();
        start(&mut opens, "a", Mode::Switch, None);
        opens.enqueue("b", Mode::Switch);
        opens.active.as_mut().unwrap().started_at = Instant::now() - Duration::from_secs(31);
        let completion = opens.poll(None).unwrap();
        assert!(completion.error.unwrap().contains("timed out"));
        assert_eq!(completion.applied, None);
        assert!(opens.blocks_persist());
        assert_eq!(opens.next().unwrap().name, "b");
        assert!(!opens.busy());
    }

    #[test]
    fn failed_create_returns_bounded_diagnostic_and_retry_name() {
        let mut opens = Opens::default();
        opens.enqueue_named("work", "taken".into());
        let request = opens.next().unwrap();
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "printf 'session name taken is already in use\\n' >&2; exit 1",
            ])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        opens.started(request, child, None);
        let deadline = Instant::now() + Duration::from_secs(3);
        let completed = loop {
            if let Some(result) = opens.poll(None) {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(completed.session_name.as_deref(), Some("taken"));
        assert_eq!(
            completed.error.as_deref(),
            Some("session name taken is already in use")
        );
        assert!(
            !opens.blocks_persist(),
            "a rejected name must not freeze the current Space"
        );
        assert!(!opens.busy());
        opens.enqueue_named("work", "available".into());
        assert_eq!(
            opens.next().unwrap().session_name.as_deref(),
            Some("available")
        );
    }

    #[test]
    fn verbose_helper_cannot_block_on_stderr() {
        let mut opens = Opens::default();
        opens.enqueue("work", Mode::Switch);
        let request = opens.next().unwrap();
        let child = Command::new("/bin/sh")
            .args(["-c", "head -c 1048576 /dev/zero >&2; exit 1"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        opens.started(request, child, None);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = opens.poll(None) {
                assert!(result.error.unwrap().len() <= 4096);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
