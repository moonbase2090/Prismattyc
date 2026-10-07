// SPDX-License-Identifier: MPL-2.0
//! Long-lived pmuxd snapshot cache for periodic host polls.
//!
//! One background thread owns the socket. It snapshots once, then pulls
//! `Events` on a short cadence and snapshots again only when the sequence
//! moves. [`SnapshotClient::fresh`] clones the cache and never connects, so a
//! stopped pmuxd cannot spend the 2 s control timeout on the main thread.
//! The client is off unless `snapshot_client` is set; one-shot callers keep
//! [`crate::attach_log::live_snapshot`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime};

use prismattyc_mux::{ControlError, ControlErrorCode, Snapshot};

use crate::attach_log::{self, SnapshotSocket};
use crate::mux::Wake;

/// How often the client thread pulls events while the socket is healthy.
pub(crate) const CADENCE: Duration = Duration::from_millis(200);
/// First wait after a failed connect or a dropped socket.
const BACKOFF_START: Duration = Duration::from_millis(50);
/// Cap so a dead daemon does not spin, and a later restart is still noticed.
const BACKOFF_MAX: Duration = Duration::from_secs(2);

struct Cache {
    snapshot: Option<Snapshot>,
    /// When the snapshot request started, not when its bytes were published.
    /// The server can release its state lock and accept a newer layout while
    /// this response is still in flight, so a publish-time stamp would make
    /// the stale body look newer than that file.
    fetched_at: Option<SystemTime>,
    /// Set until the client thread has a snapshot from the current connection
    /// attempt. [`SnapshotClient::fresh`] returns `None` while this is set,
    /// matching a failed `live_snapshot` for periodic callers.
    stale: bool,
}

struct Inner {
    cache: Mutex<Cache>,
    connects: AtomicU64,
    wake: Wake,
}

/// Background snapshot cache. Drop stops the thread and joins it.
pub(crate) struct SnapshotClient {
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
    refresh_tx: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SnapshotClient {
    /// Follow [`attach_log::mux_socket`] on every connect attempt.
    pub(crate) fn spawn(wake: Wake) -> Self {
        Self::spawn_with(wake, || attach_log::mux_socket().ok())
    }

    /// Keep using `path`, including across reconnects. Tests pass a private socket.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn spawn_at(path: PathBuf, wake: Wake) -> Self {
        Self::spawn_with(wake, move || Some(path.clone()))
    }

    fn spawn_with(wake: Wake, socket_path: impl Fn() -> Option<PathBuf> + Send + 'static) -> Self {
        let inner = Arc::new(Inner {
            cache: Mutex::new(Cache {
                snapshot: None,
                fetched_at: None,
                stale: true,
            }),
            connects: AtomicU64::new(0),
            wake,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let inner_thread = Arc::clone(&inner);
        let stop_thread = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("pmux-snapshot".into())
            .spawn(move || run(socket_path, inner_thread, stop_thread, refresh_rx))
            .ok();
        Self {
            inner,
            stop,
            refresh_tx,
            thread,
        }
    }

    /// Latest snapshot, or `None` when the cache is missing or stale.
    /// Does not open a socket.
    pub(crate) fn fresh(&self) -> Option<Snapshot> {
        self.fresh_observed().map(|(_, snapshot)| snapshot)
    }

    /// Latest snapshot and the time it was read from pmuxd.
    pub(crate) fn fresh_observed(&self) -> Option<(SystemTime, Snapshot)> {
        let cache = lock(&self.inner.cache);
        if cache.stale {
            return None;
        }
        match (&cache.snapshot, cache.fetched_at) {
            (Some(snapshot), Some(fetched_at)) => Some((fetched_at, snapshot.clone())),
            _ => None,
        }
    }

    /// A cache with no background thread. The Linux window fixture publishes
    /// snapshots directly. Other targets do not compile that fixture.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn detached() -> Self {
        let inner = Arc::new(Inner {
            cache: Mutex::new(Cache {
                snapshot: None,
                fetched_at: None,
                stale: true,
            }),
            connects: AtomicU64::new(0),
            wake: Arc::new(|| {}),
        });
        let (refresh_tx, _refresh_rx) = mpsc::channel();
        Self {
            inner,
            stop: Arc::new(AtomicBool::new(false)),
            refresh_tx,
            thread: None,
        }
    }

    /// Install one snapshot without connecting. `fetched_at` is the time the
    /// caller claims the request started. Linux window tests use this to
    /// place a cache before or after a layout file.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn publish_for_test(&self, snapshot: Snapshot, fetched_at: SystemTime) {
        let mut cache = lock(&self.inner.cache);
        cache.snapshot = Some(snapshot);
        cache.fetched_at = Some(fetched_at);
        cache.stale = false;
    }

    /// Ask the client thread to pull events now. The caller does not wait.
    /// Periodic polls use the cadence; this is the on-demand path.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn request_refresh(&self) {
        let _ = self.refresh_tx.send(());
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn connects(&self) -> u64 {
        self.inner.connects.load(Ordering::Relaxed)
    }
}

impl Drop for SnapshotClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.refresh_tx.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `None` keeps today's blocking snapshot. `Some` reads the cache only.
pub(crate) fn snapshot_for_periodic(client: Option<&SnapshotClient>) -> Option<Snapshot> {
    snapshot_observed(client).map(|(_, snapshot)| snapshot)
}

/// Snapshot plus the time it was obtained. A live read is stamped now,
/// which is after the caller has already seen the layout file.
pub(crate) fn snapshot_observed(client: Option<&SnapshotClient>) -> Option<(SystemTime, Snapshot)> {
    match client {
        Some(client) => client.fresh_observed(),
        None => attach_log::live_snapshot().map(|snapshot| (SystemTime::now(), snapshot)),
    }
}

/// Session id → name for attach-tab regroup. A live client never falls
/// through to [`attach_log::session_names`], which opens its own socket.
pub(crate) fn periodic_session_names(client: Option<&SnapshotClient>) -> HashMap<String, String> {
    match client {
        Some(client) => client
            .fresh()
            .map(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .map(|session| (session.id.to_string(), session.name.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        None => attach_log::session_names(),
    }
}

/// Start the cache when the flag is on. The host reads the flag at startup.
pub(crate) fn start_snapshot_client(enabled: bool, wake: Wake) -> Option<Arc<SnapshotClient>> {
    enabled.then(|| Arc::new(SnapshotClient::spawn(wake)))
}

fn run(
    socket_path: impl Fn() -> Option<PathBuf>,
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
    refresh_rx: mpsc::Receiver<()>,
) {
    let mut backoff = Duration::ZERO;
    let mut conn: Option<SnapshotSocket> = None;
    let mut sequence = 0u64;
    while !stop.load(Ordering::Acquire) {
        if conn.is_none() {
            if pause(&refresh_rx, &stop, backoff) {
                break;
            }
            match open_and_snapshot(&socket_path, &inner) {
                Some((opened, seen)) => {
                    sequence = seen;
                    backoff = Duration::ZERO;
                    conn = Some(opened);
                }
                None => {
                    mark_stale(&inner);
                    backoff = grow(backoff);
                }
            }
            continue;
        }
        if pause(&refresh_rx, &stop, CADENCE) {
            break;
        }
        let mut failed = false;
        if let Some(connection) = conn.as_mut() {
            match connection.events_after(sequence) {
                Ok(batch) if batch.current_sequence == sequence && batch.events.is_empty() => {}
                Ok(_) => {
                    if !resnapshot(connection, &inner, &mut sequence) {
                        failed = true;
                    }
                }
                Err(error) if needs_snapshot(&error) => {
                    if !resnapshot(connection, &inner, &mut sequence) {
                        failed = true;
                    }
                }
                Err(_) => {
                    mark_stale(&inner);
                    failed = true;
                }
            }
        }
        if failed {
            conn = None;
            backoff = grow(backoff);
        }
    }
}

fn open_and_snapshot(
    socket_path: &impl Fn() -> Option<PathBuf>,
    inner: &Inner,
) -> Option<(SnapshotSocket, u64)> {
    let path = socket_path()?;
    let mut opened = SnapshotSocket::open(&path, || {
        // Count before register/snapshot reads. A blackhole daemon blocks
        // those reads for the 2 s timeout; the connect itself already happened.
        inner.connects.fetch_add(1, Ordering::Relaxed);
    })
    .ok()?;
    let observed_at = SystemTime::now();
    match opened.snapshot() {
        Ok(snapshot) => {
            let sequence = snapshot.sequence;
            publish(inner, snapshot, observed_at);
            Some((opened, sequence))
        }
        Err(_) => None,
    }
}

fn resnapshot(connection: &mut SnapshotSocket, inner: &Inner, sequence: &mut u64) -> bool {
    let observed_at = SystemTime::now();
    match connection.snapshot() {
        Ok(snapshot) => {
            *sequence = snapshot.sequence;
            publish(inner, snapshot, observed_at);
            true
        }
        Err(_) => {
            mark_stale(inner);
            false
        }
    }
}

fn publish(inner: &Inner, snapshot: Snapshot, observed_at: SystemTime) {
    let changed = {
        let mut cache = lock(&inner.cache);
        let changed = cache.snapshot.as_ref() != Some(&snapshot);
        cache.snapshot = Some(snapshot);
        cache.fetched_at = Some(observed_at);
        cache.stale = false;
        changed
    };
    // Wake after the mutex drops. The host wake handler re-enters `fresh`.
    if changed {
        (inner.wake)();
    }
}

fn mark_stale(inner: &Inner) {
    lock(&inner.cache).stale = true;
}

fn needs_snapshot(error: &anyhow::Error) -> bool {
    matches!(
        error
            .downcast_ref::<ControlError>()
            .map(|control| control.code),
        Some(
            ControlErrorCode::EventGap
                | ControlErrorCode::StaleSequence
                | ControlErrorCode::SnapshotRequired
        )
    )
}

fn grow(current: Duration) -> Duration {
    let next = if current < BACKOFF_START {
        BACKOFF_START
    } else {
        current.saturating_mul(2)
    };
    next.min(BACKOFF_MAX)
}

/// `true` when the thread should exit. A zero wait does not block.
fn pause(rx: &mpsc::Receiver<()>, stop: &AtomicBool, dur: Duration) -> bool {
    if stop.load(Ordering::Acquire) {
        return true;
    }
    if !dur.is_zero() {
        let _ = rx.recv_timeout(dur);
        while rx.try_recv().is_ok() {}
    }
    stop.load(Ordering::Acquire)
}

fn lock(cache: &Mutex<Cache>) -> MutexGuard<'_, Cache> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::{
        ControlPlane, ControlRequest, ControlResponseBody, ControlServer, Domain, WindowBounds,
        PROTOCOL_VERSION,
    };
    use std::io::ErrorKind;
    use std::path::Path;
    use std::time::Instant;

    use prismattyc_mux::local_socket::{UnixListener, UnixStream};

    fn noop_wake() -> Wake {
        Arc::new(|| {})
    }

    fn socket_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!(
            "/tmp/prism-snapshot-client-{}-{}-{n}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn boot_server(path: &Path) -> ControlServer {
        let _ = std::fs::remove_file(path);
        let domain = Domain::bootstrap("seat").expect("bootstrap domain");
        let window = domain
            .sessions()
            .next()
            .and_then(|session| session.windows.first().copied())
            .expect("bootstrap window");
        let plane = ControlPlane::new(
            domain,
            [WindowBounds {
                window_id: window.get(),
                cols: 80,
                rows: 24,
            }],
            Some(64),
        )
        .expect("control plane");
        ControlServer::bind(path, plane).expect("bind test pmuxd")
    }

    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    fn seat_cached(client: &SnapshotClient) -> bool {
        client.fresh().is_some_and(|snapshot| {
            snapshot.sessions.len() == 1 && snapshot.sessions[0].name == "seat"
        })
    }

    fn first_connection_cached(client: &SnapshotClient) -> bool {
        client.connects() == 1 && seat_cached(client)
    }

    /// Accepts and holds streams. The client's own socket stays blocking, so
    /// its read waits out the 2 s control timeout while nobody answers.
    struct Blackhole {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
        held: Arc<Mutex<Vec<UnixStream>>>,
    }

    impl Blackhole {
        fn bind(path: &Path) -> Self {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).expect("blackhole listen");
            listener.set_nonblocking(true).expect("blackhole poll");
            let stop = Arc::new(AtomicBool::new(false));
            let held = Arc::new(Mutex::new(Vec::new()));
            let stop_flag = Arc::clone(&stop);
            let held_flag = Arc::clone(&held);
            let thread = thread::spawn(move || {
                while !stop_flag.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            held_flag
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push(stream);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                stop,
                thread: Some(thread),
                held,
            }
        }
    }

    impl Drop for Blackhole {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }

    #[test]
    fn flag_off_does_not_start_a_client() {
        let wake = noop_wake();
        assert!(start_snapshot_client(false, wake).is_none());
        assert!(!crate::config::ConfigFile::default().snapshot_client_enabled());
    }

    #[test]
    fn fresh_read_stays_under_budget_while_server_is_stopped() {
        let path = socket_path();
        let server = boot_server(&path);
        let client = SnapshotClient::spawn_at(path.clone(), noop_wake());
        assert!(
            wait_until(Duration::from_secs(3), || first_connection_cached(&client)),
            "connects={} fresh={:?}",
            client.connects(),
            client.fresh().map(|snapshot| snapshot.sessions.len())
        );
        drop(server);
        let hole = Blackhole::bind(&path);
        assert!(
            wait_until(Duration::from_secs(5), || client.connects() >= 2),
            "client never reached the stopped daemon (connects={})",
            client.connects()
        );
        let started = Instant::now();
        let snapshot = snapshot_for_periodic(Some(&client));
        let names = periodic_session_names(Some(&client));
        let elapsed = started.elapsed();
        assert!(
            snapshot.is_none(),
            "a stopped daemon must look like a failed live snapshot"
        );
        assert!(
            names.is_empty(),
            "periodic names must not open their own socket"
        );
        assert!(
            elapsed < Duration::from_millis(8),
            "fresh() took {elapsed:?}; main-thread budget is 8 ms while pmuxd is stopped"
        );
        drop(hole);
    }

    #[test]
    fn steady_state_opens_one_connection() {
        let path = socket_path();
        let _server = boot_server(&path);
        let client = SnapshotClient::spawn_at(path.clone(), noop_wake());
        assert!(
            wait_until(Duration::from_secs(3), || first_connection_cached(&client)),
            "connects={}",
            client.connects()
        );
        thread::sleep(CADENCE * 3 + Duration::from_millis(50));
        assert_eq!(
            client.connects(),
            1,
            "steady state must stay on the first socket (0 new connects per second)"
        );
        assert!(first_connection_cached(&client));
    }

    #[test]
    fn cache_recovers_after_server_returns() {
        let path = socket_path();
        let server = boot_server(&path);
        let client = SnapshotClient::spawn_at(path.clone(), noop_wake());
        assert!(wait_until(Duration::from_secs(3), || {
            first_connection_cached(&client)
        }));
        drop(server);
        let hole = Blackhole::bind(&path);
        assert!(
            wait_until(Duration::from_secs(5), || client.connects() >= 2),
            "connects={}",
            client.connects()
        );
        assert!(client.fresh().is_none());
        drop(hole);
        let _server = boot_server(&path);
        client.request_refresh();
        assert!(
            wait_until(Duration::from_secs(3), || seat_cached(&client)),
            "cache did not recover after pmuxd returned (connects={})",
            client.connects()
        );
    }

    #[test]
    fn events_refresh_sees_a_renamed_pane() {
        let path = socket_path();
        let wakes = Arc::new(AtomicU64::new(0));
        let wakes_for_hook = Arc::clone(&wakes);
        let wake: Wake = Arc::new(move || {
            wakes_for_hook.fetch_add(1, Ordering::Relaxed);
        });
        let server = boot_server(&path);
        let client = SnapshotClient::spawn_at(path.clone(), wake);
        assert!(wait_until(Duration::from_secs(3), || {
            first_connection_cached(&client)
        }));
        let before = client.fresh().expect("seat snapshot");
        let pane_id = before.sessions[0].windows[0].panes[0].id;
        assert!(
            before.sessions[0].windows[0].panes[0].title.is_empty(),
            "bootstrap pane title starts empty"
        );
        let wakes_before = wakes.load(Ordering::Relaxed);
        {
            let plane = server.plane();
            let mut plane = plane.lock().expect("plane lock");
            let response = plane.handle(ControlRequest::RenamePane {
                version: PROTOCOL_VERSION,
                request_id: 1,
                pane_id,
                title: "build".into(),
            });
            assert!(
                matches!(response.body, ControlResponseBody::Ok { .. }),
                "{response:?}"
            );
        }
        client.request_refresh();
        let saw_title = wait_until(Duration::from_secs(3), || {
            client
                .fresh()
                .is_some_and(|snapshot| snapshot.sessions[0].windows[0].panes[0].title == "build")
        });
        assert!(saw_title, "cache kept the pre-rename snapshot");
        assert!(
            wakes.load(Ordering::Relaxed) > wakes_before,
            "a changed snapshot must wake the host"
        );
    }

    #[test]
    fn supplied_snapshot_is_the_shape_source() {
        let path = socket_path();
        let _server = boot_server(&path);
        let client = SnapshotClient::spawn_at(path.clone(), noop_wake());
        assert!(wait_until(Duration::from_secs(3), || {
            first_connection_cached(&client)
        }));
        let snapshot = client.fresh().expect("seat snapshot");
        let session_id = snapshot.sessions[0].id.to_string();
        let space = prismattyc_mux::SavedSpace {
            version: 1,
            id: snapshot.sessions[0].space_id.clone(),
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: vec![prismattyc_mux::SavedSpaceSession {
                name: "seat".into(),
                agent: None,
                windows: vec![prismattyc_mux::SavedWindow {
                    title: "not-the-live-title".into(),
                    cols: 80,
                    rows: 24,
                    root: prismattyc_mux::SavedNode::Leaf {
                        cwd: None,
                        program: None,
                        command: None,
                        title: None,
                    },
                }],
            }],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        let (matches, fingerprint) = crate::spaces_polish::saved_shapes(&space, Some(&snapshot));
        assert!(
            !matches,
            "a saved tree with a different window title must not count as unchanged"
        );
        assert!(
            fingerprint.contains(&session_id),
            "fingerprint must come from the supplied snapshot, got {fingerprint}"
        );
        let (missing_matches, missing_fingerprint) =
            crate::spaces_polish::saved_shapes(&space, None);
        assert!(missing_matches);
        assert!(
            missing_fingerprint.is_empty(),
            "a missing snapshot must not look like a layout change"
        );
    }

    /// Holds the first snapshot response so the test can write a layout file
    /// while that response is still in flight.
    ///
    /// The listen socket is nonblocking so the accept loop can notice
    /// `release`. An accepted stream inherits that flag on macOS, which made
    /// the first client read look like EOF and left this thread blocked in
    /// `read_line`. Drop shuts the sockets down before joining.
    struct HoldingProxy {
        release: Arc<AtomicBool>,
        held: Arc<AtomicBool>,
        seen: Arc<Mutex<Vec<String>>>,
        client_sock: Arc<Mutex<Option<UnixStream>>>,
        server_sock: Arc<Mutex<Option<UnixStream>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    fn note_line(seen: &Mutex<Vec<String>>, prefix: &str, line: &str) {
        let mut lines = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() >= 6 {
            return;
        }
        let trimmed = line.trim_end();
        let end = trimmed.len().min(180);
        lines.push(format!("{prefix}{}", &trimmed[..end]));
    }

    impl HoldingProxy {
        fn bind(listen: &Path, upstream: &Path) -> Self {
            let _ = std::fs::remove_file(listen);
            let listener = UnixListener::bind(listen).expect("holding proxy listen");
            listener.set_nonblocking(true).expect("holding proxy poll");
            let upstream = upstream.to_path_buf();
            let release = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(false));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let client_sock = Arc::new(Mutex::new(None));
            let server_sock = Arc::new(Mutex::new(None));
            let release_flag = Arc::clone(&release);
            let held_flag = Arc::clone(&held);
            let seen_flag = Arc::clone(&seen);
            let client_slot = Arc::clone(&client_sock);
            let server_slot = Arc::clone(&server_sock);
            let thread = thread::spawn(move || {
                let client = loop {
                    if release_flag.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => {
                            note_line(&seen_flag, "accept ", &error.to_string());
                            return;
                        }
                    }
                };
                if let Err(error) = client.set_nonblocking(false) {
                    note_line(&seen_flag, "blocking ", &error.to_string());
                    return;
                }
                match client.try_clone() {
                    Ok(stream) => {
                        *client_slot
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stream);
                    }
                    Err(error) => {
                        note_line(&seen_flag, "clone ", &error.to_string());
                        return;
                    }
                }
                let server = match UnixStream::connect(&upstream) {
                    Ok(stream) => stream,
                    Err(error) => {
                        note_line(&seen_flag, "upstream ", &error.to_string());
                        return;
                    }
                };
                match server.try_clone() {
                    Ok(stream) => {
                        *server_slot
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stream);
                    }
                    Err(error) => {
                        note_line(&seen_flag, "clone ", &error.to_string());
                        return;
                    }
                }
                let client_read = match client.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        note_line(&seen_flag, "clone ", &error.to_string());
                        return;
                    }
                };
                let server_write = match server.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        note_line(&seen_flag, "clone ", &error.to_string());
                        return;
                    }
                };
                let seen_forward = Arc::clone(&seen_flag);
                let forward = thread::spawn(move || {
                    use std::io::{BufRead, BufReader, Write};
                    let mut reader = BufReader::new(client_read);
                    let mut writer = server_write;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(error) => {
                                note_line(&seen_forward, "client-read ", &error.to_string());
                                break;
                            }
                        }
                        note_line(&seen_forward, "c ", &line);
                        if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
                            break;
                        }
                    }
                });
                use std::io::{BufRead, BufReader, Write};
                let mut reader = BufReader::new(server);
                let mut writer = client;
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) => {
                            note_line(&seen_flag, "server-read ", &error.to_string());
                            break;
                        }
                    }
                    note_line(&seen_flag, "s ", &line);
                    if line.contains("\"kind\":\"snapshot\"")
                        && !release_flag.load(Ordering::Acquire)
                    {
                        held_flag.store(true, Ordering::Release);
                        while !release_flag.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                let _ = forward.join();
            });
            Self {
                release,
                held,
                seen,
                client_sock,
                server_sock,
                thread: Some(thread),
            }
        }

        fn seen(&self) -> String {
            self.seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .join(" | ")
        }
    }

    impl Drop for HoldingProxy {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Release);
            for slot in [&self.client_sock, &self.server_sock] {
                if let Some(stream) = slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn delayed_snapshot_keeps_the_request_start_time() {
        let upstream = socket_path();
        let listen = socket_path();
        let _server = boot_server(&upstream);
        let proxy = HoldingProxy::bind(&listen, &upstream);
        let client = SnapshotClient::spawn_at(listen, noop_wake());
        assert!(
            wait_until(Duration::from_secs(3), || {
                proxy.held.load(Ordering::Acquire)
            }),
            "snapshot response was not held in flight; saw {}",
            proxy.seen()
        );
        // The helper's layout write lands while the old snapshot is held.
        let file_mtime = SystemTime::now();
        thread::sleep(Duration::from_millis(40));
        proxy.release.store(true, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(3), || client.fresh_observed().is_some()),
            "delayed snapshot was not published"
        );
        let (fetched_at, snapshot) = client.fresh_observed().expect("published snapshot");
        assert!(
            fetched_at <= file_mtime,
            "observation {fetched_at:?} must stay at the request start, not the publish time after {file_mtime:?}"
        );
        let space = prismattyc_mux::SavedSpace {
            version: 2,
            id: Some("a".into()),
            created_at_unix_ms: None,
            saved_at_unix: 0,
            sessions: Vec::new(),
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        let file = crate::attach_tabs::AttachTabsFile {
            tabs: vec![crate::attach_tabs::AttachTabRecord {
                title: "late".into(),
                sessions: vec!["999".into()],
                layout: None,
            }],
            space: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(
            crate::space_view::layout_ownership(
                Some(&space),
                Some(&(fetched_at, snapshot)),
                &file,
                Some(file_mtime),
                true,
            ),
            crate::space_view::LayoutOwnership::Pending,
            "a snapshot requested before the layout write must not deny it"
        );
    }
}
