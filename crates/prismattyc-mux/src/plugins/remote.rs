// SPDX-License-Identifier: MIT
//! Cancellable native metadata transports and a bounded consumer cache.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use pmux_plugin_api::host::pmux::activity::read;
use prismattyc_core::activity::{ActivityCursor, ActivityPage, ActivityRecord, ActivityReset};
use serde::{Deserialize, Serialize};

use super::endpoint::{Endpoint, EndpointSource};
use super::wire::BuildIdentity;

pub const INTERNAL_ENDPOINT_ARG: &str = "--internal-plugin-endpoint-worker";
const FRAME_BYTES: usize = 1024 * 1024;
const INTERVAL: Duration = Duration::from_millis(250);

#[derive(Serialize, Deserialize)]
struct Bootstrap {
    build: BuildIdentity,
    endpoint: Endpoint,
}

#[derive(Serialize, Deserialize)]
enum Update {
    Page(ActivityPage),
    Error(read::Error),
}

#[derive(Default)]
struct Snapshot {
    epoch: Option<String>,
    records: VecDeque<ActivityRecord>,
    oldest: u64,
    through: u64,
    error: Option<read::Error>,
    caught_up: bool,
}

impl Snapshot {
    fn apply(&mut self, page: ActivityPage) -> Result<()> {
        validate_page(&page)?;
        if self.epoch.as_deref() != Some(&page.epoch) || page.reset.is_some() {
            self.records.clear();
            self.through = 0;
            self.caught_up = false;
        } else if page.cursor.sequence < self.through {
            self.error = None;
            return Ok(());
        }
        for record in page.records {
            if record.sequence > self.through {
                self.records.push_back(record);
            }
        }
        self.records
            .retain(|record| record.sequence >= page.oldest_sequence);
        while self.records.len() > 512 {
            self.records.pop_front();
        }
        self.epoch = Some(page.epoch);
        self.oldest = self
            .records
            .front()
            .map_or(page.oldest_sequence, |record| record.sequence);
        self.through = page.cursor.sequence;
        self.error = None;
        self.caught_up |= !page.has_more;
        Ok(())
    }

    fn page(&self, after: Option<ActivityCursor>, limit: u16) -> Result<ActivityPage, read::Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.caught_up {
            return Err(read::Error::Connecting);
        }
        let epoch = self.epoch.clone().ok_or(read::Error::Connecting)?;
        let reset = after.as_ref().and_then(|cursor| {
            if cursor.epoch != epoch {
                Some(ActivityReset::Restarted)
            } else if cursor.sequence > self.through {
                Some(ActivityReset::AheadOfFeed)
            } else if cursor.sequence.saturating_add(1) < self.oldest {
                Some(ActivityReset::RetentionGap)
            } else {
                None
            }
        });
        let after = if reset.is_some() { None } else { after };
        let records = self
            .records
            .iter()
            .filter(|record| {
                after
                    .as_ref()
                    .is_none_or(|cursor| record.sequence > cursor.sequence)
            })
            .take(usize::from(limit))
            .cloned()
            .collect::<Vec<_>>();
        let sequence = records
            .last()
            .map_or(self.through, |record| record.sequence);
        Ok(ActivityPage {
            epoch: epoch.clone(),
            oldest_sequence: self.oldest,
            current_sequence: self.through,
            records,
            cursor: ActivityCursor { epoch, sequence },
            has_more: sequence < self.through,
            reset,
        })
    }
}

fn validate_page(page: &ActivityPage) -> Result<()> {
    ensure!(
        !page.epoch.is_empty()
            && page.epoch.len() <= 128
            && page.epoch.is_ascii()
            && !page.epoch.chars().any(char::is_control),
        "invalid remote epoch"
    );
    ensure!(
        page.records.len() <= 256
            && page.cursor.epoch == page.epoch
            && page.cursor.sequence <= page.current_sequence,
        "invalid remote page cursor"
    );
    ensure!(
        page.oldest_sequence <= page.current_sequence.saturating_add(1),
        "invalid remote retention range"
    );
    let mut previous = None;
    for record in &page.records {
        record
            .event
            .validate_metadata()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            record.sequence >= page.oldest_sequence
                && record.sequence <= page.cursor.sequence
                && previous.is_none_or(|sequence| record.sequence > sequence),
            "invalid remote record ordering"
        );
        previous = Some(record.sequence);
    }
    ensure!(
        !page.has_more
            || (!page.records.is_empty() && page.cursor.sequence < page.current_sequence),
        "invalid remote continuation"
    );
    Ok(())
}

/// The background thread holds the cache, never this lease. Last-lease drop
/// closes the worker's parent pipe and stops every owned native helper.
pub(super) struct Remote {
    state: Arc<Mutex<Snapshot>>,
    stopped: Arc<AtomicBool>,
}

impl Remote {
    pub fn start(endpoint: Endpoint, active: Arc<AtomicUsize>) -> Result<Arc<Self>> {
        endpoint.validate()?;
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 32).then_some(count + 1)
            })
            .map_err(|_| anyhow::anyhow!("remote reader limit reached"))?;
        struct Reservation(Arc<AtomicUsize>);
        impl Drop for Reservation {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let reservation = Reservation(active);
        let remote = Arc::new(Self {
            state: Arc::new(Mutex::new(Snapshot::default())),
            stopped: Arc::new(AtomicBool::new(false)),
        });
        let state = remote.state.clone();
        let stopped = remote.stopped.clone();
        let executable = std::env::current_exe()?;
        std::thread::Builder::new()
            .name("plugin-endpoint".into())
            .spawn(move || {
                let _reservation = reservation;
                let result = (|| -> Result<()> {
                    let process = Command::new(executable)
                        .arg(INTERNAL_ENDPOINT_ARG)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()?;
                    let mut owned = OwnedProcess::new(process);
                    let mut parent = owned
                        .child
                        .stdin
                        .take()
                        .context("endpoint worker has no input")?;
                    let bootstrap = Bootstrap {
                        build: BuildIdentity::current(),
                        endpoint,
                    };
                    let mut bytes = serde_json::to_vec(&bootstrap)?;
                    ensure!(
                        bytes.len() <= 64 * 1024,
                        "endpoint bootstrap exceeds its budget"
                    );
                    bytes.push(b'\n');
                    parent.write_all(&bytes)?;
                    let mut output = Pipe::new(
                        owned
                            .child
                            .stdout
                            .take()
                            .context("endpoint worker has no output")?,
                    )?;
                    let mut pending = Vec::new();
                    while !stopped.load(Ordering::Acquire) {
                        match output.chunk()? {
                            Chunk::Data(chunk) => {
                                pending.extend(chunk);
                                consume_lines(&mut pending, |line| {
                                    match serde_json::from_slice::<Update>(line)? {
                                        Update::Page(page) => state.lock().unwrap().apply(page)?,
                                        Update::Error(error) => {
                                            state.lock().unwrap().error = Some(error)
                                        }
                                    }
                                    Ok(())
                                })?;
                            }
                            Chunk::Idle => std::thread::sleep(Duration::from_millis(20)),
                            Chunk::End => break,
                        }
                    }
                    drop(parent);
                    Ok(())
                })();
                if result.is_err() || !stopped.load(Ordering::Acquire) {
                    state.lock().unwrap().error = Some(read::Error::Unreachable);
                }
            })?;
        Ok(remote)
    }

    pub fn page(
        &self,
        after: Option<ActivityCursor>,
        limit: u16,
    ) -> Result<ActivityPage, read::Error> {
        self.state
            .lock()
            .map_err(|_| read::Error::Unreachable)?
            .page(after, limit)
    }

    pub fn epoch(&self) -> Option<String> {
        let state = self.state.lock().ok()?;
        state.error.is_none().then(|| state.epoch.clone()).flatten()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

enum Chunk {
    Data(Vec<u8>),
    Idle,
    End,
}
struct Pipe(ChildStdout);
impl Pipe {
    fn new(output: ChildStdout) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = output.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self(output))
    }
    fn chunk(&mut self) -> io::Result<Chunk> {
        let mut buffer = [0u8; 4096];
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe};
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    Ok(Chunk::End)
                } else {
                    Err(error)
                };
            }
            if available == 0 {
                return Ok(Chunk::Idle);
            }
            let maximum = buffer.len().min(available as usize);
            return match self.0.read(&mut buffer[..maximum])? {
                0 => Ok(Chunk::End),
                count => Ok(Chunk::Data(buffer[..count].to_vec())),
            };
        }
        #[cfg(not(windows))]
        match self.0.read(&mut buffer) {
            Ok(0) => Ok(Chunk::End),
            Ok(count) => Ok(Chunk::Data(buffer[..count].to_vec())),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(Chunk::Idle)
            }
            Err(error) => Err(error),
        }
    }
}

fn consume_lines(
    pending: &mut Vec<u8>,
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
        ensure!(end <= FRAME_BYTES, "remote frame exceeds its byte budget");
        consume(&pending[..end])?;
        pending.drain(..=end);
    }
    ensure!(
        pending.len() <= FRAME_BYTES,
        "remote frame exceeds its byte budget"
    );
    Ok(())
}

struct OwnedProcess {
    child: Child,
    graceful: bool,
    #[cfg(unix)]
    group: bool,
    #[cfg(windows)]
    job: Option<crate::platform::ProbeJob>,
}
impl OwnedProcess {
    fn new(child: Child) -> Self {
        Self {
            child,
            graceful: true,
            #[cfg(unix)]
            group: false,
            #[cfg(windows)]
            job: None,
        }
    }
    fn command(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            let (child, job) = crate::platform::spawn_probe(command)?;
            Ok(Self {
                child,
                graceful: false,
                job: Some(job),
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                child: command.spawn()?,
                graceful: false,
                group: true,
            })
        }
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.graceful {
            let deadline = Instant::now() + Duration::from_secs(1);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        #[cfg(unix)]
        if self.group {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            self.job.take();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn emit(update: Update) -> Result<()> {
    let mut bytes = serde_json::to_vec(&update)?;
    ensure!(
        bytes.len() <= FRAME_BYTES,
        "native endpoint reply exceeds its byte budget"
    );
    bytes.push(b'\n');
    let mut output = io::stdout().lock();
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

fn pause(duration: Duration, stopped: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn run_worker() -> Result<()> {
    use std::io::BufRead;
    let mut input = io::BufReader::new(io::stdin());
    let mut bytes = Vec::new();
    (&mut input)
        .take(64 * 1024 + 1)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= 64 * 1024 && bytes.last() == Some(&b'\n'),
        "invalid endpoint bootstrap"
    );
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes)?;
    ensure!(
        bootstrap.build == BuildIdentity::current(),
        "endpoint worker build mismatch"
    );
    bootstrap.endpoint.validate()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let parent = stopped.clone();
    std::thread::spawn(move || {
        let mut byte = [0];
        let _ = input.read(&mut byte);
        parent.store(true, Ordering::Release);
    });
    let mut cursor = None;
    let mut backoff = Duration::from_secs(1);
    while !stopped.load(Ordering::Acquire) {
        match &bootstrap.endpoint.source {
            EndpointSource::Socket { path } => match socket_page(path, cursor.clone(), &stopped)
                .and_then(|page| {
                    validate_page(&page).map_err(|_| read::Error::IncompatibleSchema)?;
                    Ok(page)
                }) {
                Ok(page) => {
                    let more = page.has_more;
                    cursor = Some(page.cursor.clone());
                    emit(Update::Page(page))?;
                    if !more {
                        pause(INTERVAL, &stopped);
                    }
                }
                Err(error) => {
                    emit(Update::Error(error))?;
                    pause(Duration::from_secs(1), &stopped);
                }
            },
            source => match command(source, &bootstrap.endpoint.label, &mut cursor, &stopped) {
                Ok(true) => {
                    backoff = Duration::from_secs(1);
                }
                Ok(false) => {
                    backoff = Duration::from_secs(1);
                    pause(INTERVAL, &stopped);
                }
                Err(_) if !stopped.load(Ordering::Acquire) => {
                    emit(Update::Error(read::Error::Unreachable))?;
                    pause(backoff, &stopped);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                Err(_) => {}
            },
        }
    }
    Ok(())
}

fn socket_page(
    path: &std::path::Path,
    cursor: Option<ActivityCursor>,
    stopped: &AtomicBool,
) -> Result<ActivityPage, read::Error> {
    use crate::{
        ControlErrorCode, ControlRequest, ControlResponse, ControlResponseBody,
        ControlResponseData, PROTOCOL_VERSION,
    };
    let mut socket = crate::local_socket::connect_timeout(path, Duration::from_millis(500))
        .map_err(|_| read::Error::Unreachable)?;
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| read::Error::Unreachable)?;
    socket
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| read::Error::Unreachable)?;
    let mut request = serde_json::to_vec(&ControlRequest::ReadActivity {
        version: PROTOCOL_VERSION,
        request_id: 1,
        cursor,
        limit: 256,
    })
    .map_err(|_| read::Error::InvalidRequest)?;
    request.push(b'\n');
    socket
        .write_all(&request)
        .map_err(|_| read::Error::Unreachable)?;
    let mut frame = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        let mut chunk = [0; 4096];
        match socket.read(&mut chunk) {
            Ok(0) => return Err(read::Error::Unreachable),
            Ok(count) => {
                frame.extend_from_slice(&chunk[..count]);
                if frame.len() > FRAME_BYTES {
                    return Err(read::Error::InvalidRequest);
                }
                if frame.contains(&b'\n') {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(read::Error::Unreachable),
        }
    }
    if stopped.load(Ordering::Acquire) {
        return Err(read::Error::Revoked);
    }
    let response: ControlResponse =
        serde_json::from_slice(&frame).map_err(|_| read::Error::IncompatibleSchema)?;
    if response.version != PROTOCOL_VERSION || response.request_id != 1 {
        return Err(read::Error::IncompatibleSchema);
    }
    match response.body {
        ControlResponseBody::Ok {
            response: ControlResponseData::Activity { page },
        } => Ok(page),
        ControlResponseBody::Error { error }
            if error.code == ControlErrorCode::ActivityDisabled =>
        {
            Err(read::Error::FeedDisabled)
        }
        ControlResponseBody::Error { error } if error.code == ControlErrorCode::InvalidJson => {
            Err(read::Error::IncompatibleSchema)
        }
        _ => Err(read::Error::Unreachable),
    }
}

fn command(
    source: &EndpointSource,
    label: &str,
    cursor: &mut Option<ActivityCursor>,
    stopped: &AtomicBool,
) -> Result<bool> {
    let (argv, tail) = match source {
        EndpointSource::CommandQuery { argv } => (argv, false),
        EndpointSource::CommandTail { argv } => (argv, true),
        EndpointSource::Socket { .. } => anyhow::bail!("expected command endpoint"),
    };
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !tail {
        command.args(["activity", "poll", "--limit", "256", "--host-label", label]);
        if let Some(cursor) = cursor.as_ref() {
            // SSH joins remote arguments through a shell. Only native validated
            // hex and canonical decimal values may be appended to this prefix.
            ensure!(
                !cursor.epoch.is_empty()
                    && cursor.epoch.len() <= 64
                    && cursor.epoch.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "remote cursor is not shell-safe"
            );
            command.args(["--cursor", &format!("{}:{}", cursor.epoch, cursor.sequence)]);
        }
    }
    let mut owned = OwnedProcess::command(&mut command)?;
    let mut pipe = Pipe::new(owned.child.stdout.take().context("command has no output")?)?;
    let mut pending = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rate_start = Instant::now();
    let mut pages = 0u16;
    while !stopped.load(Ordering::Acquire) {
        if !tail && Instant::now() >= deadline {
            anyhow::bail!("endpoint command deadline exceeded");
        }
        match pipe.chunk()? {
            Chunk::Data(chunk) => {
                pending.extend(chunk);
                if tail {
                    consume_lines(&mut pending, |line| {
                        if rate_start.elapsed() >= Duration::from_secs(1) {
                            pages = 0;
                            rate_start = Instant::now();
                        }
                        pages += 1;
                        ensure!(pages <= 64, "endpoint stream exceeded its page rate");
                        let value: serde_json::Value = serde_json::from_slice(line)?;
                        if value["type"] == "activity" {
                            publish_page(value["page"].clone(), cursor)?;
                        } else if value["type"] == "disconnected" {
                            emit(Update::Error(read::Error::Unreachable))?;
                        } else if let Some(code) = value
                            .pointer("/error/code")
                            .and_then(serde_json::Value::as_str)
                        {
                            emit(Update::Error(
                                if matches!(code, "feed_disabled" | "activity_disabled") {
                                    read::Error::FeedDisabled
                                } else {
                                    read::Error::Unreachable
                                },
                            ))?;
                        }
                        Ok(())
                    })?;
                } else {
                    ensure!(
                        pending.len() <= FRAME_BYTES,
                        "endpoint command output exceeds its byte budget"
                    );
                }
            }
            Chunk::Idle => std::thread::sleep(Duration::from_millis(20)),
            Chunk::End => {
                let mut more = false;
                if !tail {
                    let value: serde_json::Value = serde_json::from_slice(&pending)?;
                    if value
                        .pointer("/error/code")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|code| matches!(code, "feed_disabled" | "activity_disabled"))
                    {
                        emit(Update::Error(read::Error::FeedDisabled))?;
                    } else {
                        more = publish_page(value["page"].clone(), cursor)?;
                    }
                }
                return Ok(more);
            }
        }
    }
    Ok(false)
}

fn publish_page(value: serde_json::Value, cursor: &mut Option<ActivityCursor>) -> Result<bool> {
    let page: ActivityPage = serde_json::from_value(value)?;
    validate_page(&page)?;
    *cursor = Some(page.cursor.clone());
    let more = page.has_more;
    emit(Update::Page(page))?;
    Ok(more)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(epoch: &str, start: u64, end: u64, current: u64) -> ActivityPage {
        serde_json::from_value(serde_json::json!({
            "epoch":epoch,"oldest_sequence":1,"current_sequence":current,
            "records":(start..=end).map(|sequence| serde_json::json!({
                "sequence":sequence,"received_at_ms":1000,"event":{
                    "schema_version":1,"seat":"remote-seat","run_id":"run-1","event_id":format!("event-{sequence}"),"source":"hooks","kind":"waiting"
                }
            })).collect::<Vec<_>>(),
            "cursor":{"epoch":epoch,"sequence":end},"has_more":end<current,"reset":null
        })).unwrap()
    }

    #[test]
    fn a_remote_snapshot_waits_for_catch_up_and_keeps_the_source_cursor() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(page("abc1", 1, 256, 300)).unwrap();
        assert!(matches!(
            snapshot.page(None, 256),
            Err(read::Error::Connecting)
        ));
        snapshot.apply(page("abc1", 257, 300, 300)).unwrap();
        let first = snapshot.page(None, 256).unwrap();
        assert_eq!(
            (
                first.epoch.as_str(),
                first.records.len(),
                first.cursor.sequence,
                first.current_sequence,
                first.has_more
            ),
            ("abc1", 256, 256, 300, true)
        );
        let second = snapshot.page(Some(first.cursor), 256).unwrap();
        assert_eq!(
            (
                second.records.len(),
                second.records[0].sequence,
                second.cursor.sequence,
                second.has_more
            ),
            (44, 257, 300, false)
        );
        snapshot.apply(page("abc1", 1, 256, 300)).unwrap();
        assert_eq!(snapshot.page(None, 256).unwrap().current_sequence, 300);
    }

    #[test]
    fn cache_retention_and_epoch_changes_produce_real_reset_pages() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(page("abc1", 1, 256, 600)).unwrap();
        snapshot.apply(page("abc1", 257, 512, 600)).unwrap();
        snapshot.apply(page("abc1", 513, 600, 600)).unwrap();
        assert_eq!(snapshot.records.len(), 512);
        let reset = snapshot
            .page(
                Some(ActivityCursor {
                    epoch: "abc1".into(),
                    sequence: 0,
                }),
                256,
            )
            .unwrap();
        assert_eq!(
            (
                reset.oldest_sequence,
                reset.records[0].sequence,
                reset.reset
            ),
            (89, 89, Some(ActivityReset::RetentionGap))
        );
        snapshot.apply(page("def2", 1, 2, 2)).unwrap();
        let reset = snapshot
            .page(
                Some(ActivityCursor {
                    epoch: "abc1".into(),
                    sequence: 600,
                }),
                256,
            )
            .unwrap();
        assert_eq!(
            (
                reset.epoch.as_str(),
                reset.records.len(),
                reset.cursor.sequence,
                reset.reset
            ),
            ("def2", 2, 2, Some(ActivityReset::Restarted))
        );
    }
}
