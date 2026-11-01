// SPDX-License-Identifier: MIT

//! Background readers, one per host. Network and process I/O never runs on the UI thread.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use prismattyc_mux::activity::{exchange, ActivityCursor, ActivityPage, ActivityRejected};
use prismattyc_mux::{ControlErrorCode, ControlRequest, ControlResponseData, PROTOCOL_VERSION};
use serde_json::Value;

pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum Source {
    /// A local pmuxd control socket.
    Socket(PathBuf),
    /// A command whose stdout carries `tail` JSON lines, run without a shell.
    Command(Vec<String>),
}

pub enum Update {
    Page(ActivityPage),
    Disconnected,
    Disabled,
}

pub struct HostUpdate {
    pub host: usize,
    pub update: Update,
}

pub fn read_page(socket: &Path, cursor: Option<ActivityCursor>) -> Result<ActivityPage> {
    match exchange(
        socket,
        ControlRequest::ReadActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            cursor,
            limit: 256,
        },
    )? {
        ControlResponseData::Activity { page } => Ok(page),
        _ => bail!("unexpected activity response"),
    }
}

fn is_disabled(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ActivityRejected>()
        .is_some_and(|rejected| rejected.code == ControlErrorCode::ActivityDisabled)
}

/// Parses one `tail` output line. Unknown or end lines yield `None`.
pub fn parse_tail_line(line: &str) -> Option<Update> {
    let value: Value = serde_json::from_str(line).ok()?;
    if let Some(code) = value.pointer("/error/code").and_then(Value::as_str) {
        return Some(if code == "feed_disabled" || code == "activity_disabled" {
            Update::Disabled
        } else {
            Update::Disconnected
        });
    }
    match value.get("type").and_then(Value::as_str)? {
        "activity" => serde_json::from_value(value.get("page")?.clone())
            .ok()
            .map(Update::Page),
        "disconnected" => Some(Update::Disconnected),
        _ => None,
    }
}

pub struct Feed {
    pub updates: mpsc::Receiver<HostUpdate>,
    stopped: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Option<Child>>>>,
    workers: Vec<JoinHandle<()>>,
}

impl Feed {
    pub fn start(sources: Vec<Source>) -> Self {
        // Bounded: a page keeps its cursor in the worker until the UI takes it.
        let (sender, updates) = mpsc::sync_channel(4 * sources.len().max(1));
        let stopped = Arc::new(AtomicBool::new(false));
        let children = Arc::new(Mutex::new(
            (0..sources.len()).map(|_| None).collect::<Vec<_>>(),
        ));
        let workers = sources
            .into_iter()
            .enumerate()
            .map(|(host, source)| {
                let sender = sender.clone();
                let stopped = Arc::clone(&stopped);
                let children = Arc::clone(&children);
                thread::spawn(move || match source {
                    Source::Socket(socket) => poll_socket(host, &socket, &sender, &stopped),
                    Source::Command(argv) => run_command(host, &argv, &sender, &stopped, &children),
                })
            })
            .collect();
        Self {
            updates,
            stopped,
            children,
            workers,
        }
    }
}

fn poll_socket(
    host: usize,
    socket: &Path,
    sender: &mpsc::SyncSender<HostUpdate>,
    stopped: &AtomicBool,
) {
    let mut cursor = None;
    while !stopped.load(Ordering::Relaxed) {
        let (update, next, has_more) = match read_page(socket, cursor.clone()) {
            Ok(page) => {
                let next = page.cursor.clone();
                let has_more = page.has_more;
                (Update::Page(page), Some(next), has_more)
            }
            Err(error) if is_disabled(&error) => (Update::Disabled, None, false),
            Err(_) => (Update::Disconnected, None, false),
        };
        if !deliver(sender, HostUpdate { host, update }, stopped) {
            break;
        }
        if let Some(next) = next {
            cursor = Some(next);
        }
        if !has_more {
            pause(POLL_INTERVAL, stopped);
        }
    }
}

fn run_command(
    host: usize,
    argv: &[String],
    sender: &mpsc::SyncSender<HostUpdate>,
    stopped: &AtomicBool,
    children: &Mutex<Vec<Option<Child>>>,
) {
    let mut backoff = RESTART_MIN;
    while !stopped.load(Ordering::Relaxed) {
        let started = Instant::now();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Remote diagnostics would corrupt the terminal UI; the host row reports the outage.
            .stderr(Stdio::null());
        // Its own process group, so stopping it also stops anything it started that holds stdout.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let spawned = command.spawn();
        let mut disabled = false;
        if let Ok(mut child) = spawned {
            let stdout = child.stdout.take();
            if let Ok(mut slots) = children.lock() {
                slots[host] = Some(child);
            }
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Some(update) = parse_tail_line(&line) else {
                        continue;
                    };
                    disabled = matches!(update, Update::Disabled);
                    if !deliver(sender, HostUpdate { host, update }, stopped) {
                        return;
                    }
                }
            }
            reap(children, host);
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        if !disabled {
            let update = HostUpdate {
                host,
                update: Update::Disconnected,
            };
            if !deliver(sender, update, stopped) {
                break;
            }
        }
        if started.elapsed() > RESTART_MAX {
            backoff = RESTART_MIN;
        }
        pause(backoff, stopped);
        backoff = (backoff * 2).min(RESTART_MAX);
    }
}

fn reap(children: &Mutex<Vec<Option<Child>>>, host: usize) {
    let child = children
        .lock()
        .ok()
        .and_then(|mut slots| slots[host].take());
    if let Some(mut child) = child {
        stop(&mut child);
        let _ = child.wait();
    }
}

/// Stops a command and everything in its process group.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: the child leads its own process group, so -pid names only that group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn pause(duration: Duration, stopped: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stopped.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn deliver(
    sender: &mpsc::SyncSender<HostUpdate>,
    mut update: HostUpdate,
    stopped: &AtomicBool,
) -> bool {
    while !stopped.load(Ordering::Relaxed) {
        match sender.try_send(update) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(pending)) => {
                update = pending;
                thread::sleep(Duration::from_millis(20));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
    false
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // Killing the children closes their stdout, which unblocks the line readers.
        if let Ok(mut slots) = self.children.lock() {
            for child in slots.iter_mut().filter_map(Option::as_mut) {
                stop(child);
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_activity_lines_carry_the_page() {
        let line = r#"{"type":"activity","seat_filter":null,"page":{"epoch":"e","oldest_sequence":1,"current_sequence":2,"records":[],"cursor":{"epoch":"e","sequence":2},"has_more":false,"reset":null}}"#;
        match parse_tail_line(line) {
            Some(Update::Page(page)) => assert_eq!(page.cursor.sequence, 2),
            _ => panic!("expected a page"),
        }
    }

    #[test]
    fn tail_outages_and_start_errors_are_classified() {
        assert!(matches!(
            parse_tail_line(r#"{"type":"disconnected","retrying":true,"cursor":null}"#),
            Some(Update::Disconnected)
        ));
        assert!(matches!(
            parse_tail_line(r#"{"error":{"code":"feed_disabled","message":"x"}}"#),
            Some(Update::Disabled)
        ));
        assert!(matches!(
            parse_tail_line(r#"{"error":{"code":"daemon_unreachable"}}"#),
            Some(Update::Disconnected)
        ));
    }

    #[test]
    fn end_lines_and_noise_are_ignored() {
        assert!(parse_tail_line(r#"{"type":"end","reason":"max_events"}"#).is_none());
        assert!(parse_tail_line("not json").is_none());
        assert!(parse_tail_line(r#"{"type":"activity"}"#).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_command_source_delivers_pages_then_reports_its_exit() {
        let line = r#"{"type":"activity","seat_filter":null,"page":{"epoch":"e","oldest_sequence":1,"current_sequence":7,"records":[],"cursor":{"epoch":"e","sequence":7},"has_more":false,"reset":null}}"#;
        let feed = Feed::start(vec![Source::Command(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf '%s\\n' '{line}'"),
        ])]);
        let first = feed
            .updates
            .recv_timeout(Duration::from_secs(5))
            .expect("page");
        assert_eq!(first.host, 0);
        assert!(matches!(first.update, Update::Page(ref page) if page.cursor.sequence == 7));
        let second = feed
            .updates
            .recv_timeout(Duration::from_secs(5))
            .expect("exit");
        assert!(matches!(second.update, Update::Disconnected));
    }

    #[cfg(unix)]
    mod daemon {
        use super::super::*;
        use prismattyc_mux::{ControlError, ControlResponse, ControlResponseBody};
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::AtomicUsize;

        fn page(sequence: u64, has_more: bool) -> ActivityPage {
            serde_json::from_value(serde_json::json!({
                "epoch": "e", "oldest_sequence": 1, "current_sequence": 2, "records": [],
                "cursor": {"epoch": "e", "sequence": sequence}, "has_more": has_more, "reset": null
            }))
            .expect("page")
        }

        /// A one-socket stand-in for pmuxd that answers each request with the next body
        /// (repeating the last) and records each request's cursor sequence.
        fn fake_daemon(
            bodies: Vec<ControlResponseBody>,
        ) -> (PathBuf, Arc<Mutex<Vec<Option<u64>>>>) {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "pam-{}-{}.sock",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind");
            let cursors = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&cursors);
            thread::spawn(move || {
                for (index, stream) in listener.incoming().enumerate() {
                    let Ok(mut stream) = stream else { return };
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap_or_default();
                    seen.lock()
                        .unwrap()
                        .push(request.pointer("/cursor/sequence").and_then(Value::as_u64));
                    let body = bodies[index.min(bodies.len() - 1)].clone();
                    let reply = ControlResponse {
                        version: PROTOCOL_VERSION,
                        request_id: 1,
                        body,
                    };
                    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap());
                }
            });
            (path, cursors)
        }

        fn error(code: &str) -> ControlError {
            serde_json::from_value(serde_json::json!({"code": code, "message": "rejected"}))
                .expect("error")
        }

        fn ok(page: ActivityPage) -> ControlResponseBody {
            ControlResponseBody::Ok {
                response: ControlResponseData::Activity { page },
            }
        }

        #[test]
        fn a_socket_source_follows_pending_pages_at_once_then_resumes_from_its_cursor() {
            let (path, cursors) = fake_daemon(vec![
                ok(page(1, true)),
                ok(page(2, false)),
                ok(page(2, false)),
            ]);
            let feed = Feed::start(vec![Source::Socket(path)]);
            let first = feed
                .updates
                .recv_timeout(Duration::from_secs(5))
                .expect("first");
            let started = Instant::now();
            let second = feed
                .updates
                .recv_timeout(Duration::from_secs(5))
                .expect("second");
            let gap = started.elapsed();
            assert!(matches!(first.update, Update::Page(ref p) if p.cursor.sequence == 1));
            assert!(matches!(second.update, Update::Page(ref p) if p.cursor.sequence == 2));
            assert!(
                gap < Duration::from_millis(200),
                "a pending page waited {gap:?}"
            );
            let third = Instant::now();
            feed.updates
                .recv_timeout(Duration::from_secs(5))
                .expect("third");
            assert!(
                third.elapsed() >= Duration::from_millis(200),
                "a caught-up feed polls at its interval"
            );
            let seen = cursors.lock().unwrap().clone();
            assert_eq!(&seen[..3], &[None, Some(1), Some(2)]);
        }

        #[test]
        fn a_feed_that_is_turned_off_is_reported_as_disabled_not_disconnected() {
            let (path, _) = fake_daemon(vec![ControlResponseBody::Error {
                error: error("activity_disabled"),
            }]);
            let feed = Feed::start(vec![Source::Socket(path)]);
            let update = feed
                .updates
                .recv_timeout(Duration::from_secs(5))
                .expect("update");
            assert!(matches!(update.update, Update::Disabled));
        }

        #[test]
        fn other_rejections_are_disconnects() {
            let (path, _) = fake_daemon(vec![ControlResponseBody::Error {
                error: error("invalid_request"),
            }]);
            let feed = Feed::start(vec![Source::Socket(path)]);
            let update = feed
                .updates
                .recv_timeout(Duration::from_secs(5))
                .expect("update");
            assert!(matches!(update.update, Update::Disconnected));
        }

        #[test]
        fn an_exiting_command_is_restarted_with_growing_backoff() {
            let feed = Feed::start(vec![Source::Command(vec![
                "/bin/sh".into(),
                "-c".into(),
                "exit 0".into(),
            ])]);
            let deadline = Instant::now() + Duration::from_millis(3_500);
            let mut exits = 0;
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                if let Ok(update) = feed.updates.recv_timeout(left) {
                    assert!(matches!(update.update, Update::Disconnected));
                    exits += 1;
                }
            }
            // Runs start at about 0 s, 1 s and 3 s: backoff doubles from one second. A loaded
            // machine can push the third run past the window; faster restarts give four or more.
            assert!((2..=3).contains(&exits), "{exits} restarts in 3.5 s");
        }

        #[test]
        fn dropping_the_feed_stops_a_running_command() {
            let pidfile = std::env::temp_dir().join(format!("pam-child-{}", std::process::id()));
            let _ = std::fs::remove_file(&pidfile);
            // The sleep is a grandchild that holds stdout open after its shell is gone.
            let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
            let feed = Feed::start(vec![Source::Command(vec![
                "/bin/sh".into(),
                "-c".into(),
                script,
            ])]);
            let deadline = Instant::now() + Duration::from_secs(5);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                {
                    break pid;
                }
                assert!(Instant::now() < deadline, "child did not start");
                thread::sleep(Duration::from_millis(20));
            };
            let dropped = Instant::now();
            drop(feed);
            assert!(
                dropped.elapsed() < Duration::from_secs(2),
                "drop waited for the command"
            );
            let _ = std::fs::remove_file(&pidfile);
            // SAFETY: signal 0 only checks that the process exists.
            assert_ne!(
                unsafe { libc::kill(pid, 0) },
                0,
                "child still running after drop"
            );
        }
    }

    #[test]
    fn an_unreachable_socket_reports_a_disconnect() {
        let feed = Feed::start(vec![Source::Socket(PathBuf::from(
            "/nonexistent/activity.sock",
        ))]);
        let update = feed
            .updates
            .recv_timeout(Duration::from_secs(5))
            .expect("update");
        assert!(matches!(update.update, Update::Disconnected));
    }
}
