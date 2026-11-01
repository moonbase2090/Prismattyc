// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use pmux_plugin_api::host::pmux::{activity::read, plugin::lifecycle};
use pmux_plugin_runtime::{ActivityProvider, Runtime};
use prismattyc_core::activity::{ActivityCursor, ActivityPage};

use super::schema::Schemas;
use super::wire::{self, Bootstrap, BuildIdentity, CommandSchema, Operation, Request, Response};

const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const INVOKE_TIMEOUT: Duration = Duration::from_secs(3);

/// The child handle has a separate lock from its pipes. Revocation never waits
/// behind a guest call, compiler, or blocked host import.
struct OwnedChild {
    child: Mutex<Child>,
    cancelled: AtomicBool,
}

impl OwnedChild {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Pipes {
    input: BufReader<ChildStdout>,
    output: BufWriter<ChildStdin>,
}

struct Process {
    child: Arc<OwnedChild>,
    pipes: Mutex<Pipes>,
}

pub(super) struct PreparedWorker(Process);
pub(super) struct ActiveWorker(Process);
pub(super) struct PendingWorker(Process);

#[derive(Clone)]
pub(super) struct Cancellation(Arc<OwnedChild>);

impl Cancellation {
    pub(super) fn cancel(&self) {
        self.0.cancel();
    }
}

impl Process {
    fn spawn(executable: &Path) -> Result<Self> {
        let mut child = Command::new(executable)
            .arg(super::INTERNAL_WORKER_ARG)
            .env_clear()
            .current_dir(
                executable
                    .parent()
                    .context("worker executable has no directory")?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn private plug-in worker")?;
        let input = BufReader::new(child.stdout.take().context("missing worker output")?);
        let output = BufWriter::new(child.stdin.take().context("missing worker input")?);
        Ok(Self {
            child: Arc::new(OwnedChild {
                child: Mutex::new(child),
                cancelled: AtomicBool::new(false),
            }),
            pipes: Mutex::new(Pipes { input, output }),
        })
    }

    fn exchange(
        &self,
        request: Request,
        timeout: Duration,
        provider: &mut dyn ActivityProvider,
    ) -> Result<Response> {
        ensure!(
            !self.child.cancelled.load(Ordering::Acquire),
            "plug-in worker was revoked"
        );
        let deadline = Instant::now() + timeout;
        let watchdog = Watchdog::start(self.child.clone(), timeout)?;
        let result = (|| {
            let mut pipes = self
                .pipes
                .lock()
                .map_err(|_| anyhow::anyhow!("worker channel failed"))?;
            wire::write(&mut pipes.output, &request)?;
            let mut imports = 0u16;
            loop {
                ensure!(
                    Instant::now() < deadline && !self.child.cancelled.load(Ordering::Acquire),
                    "plug-in worker deadline exceeded or revoked"
                );
                let response = wire::read(&mut pipes.input)?;
                if matches!(&response, Response::Endpoints | Response::Poll { .. }) {
                    imports += 1;
                    ensure!(imports <= 64, "plug-in host import budget exceeded");
                }
                let request = match response {
                    Response::Endpoints => Request::Endpoints(provider.endpoints()),
                    Response::Poll {
                        endpoint,
                        after,
                        limit,
                    } => Request::Page(provider.poll(endpoint, after, limit)),
                    Response::Failed(message) => bail!("plug-in worker failed: {message}"),
                    response => return Ok(response),
                };
                ensure!(
                    Instant::now() < deadline && !self.child.cancelled.load(Ordering::Acquire),
                    "plug-in worker deadline exceeded or revoked"
                );
                wire::write(&mut pipes.output, &request)?;
            }
        })();
        drop(watchdog);
        if result.is_err()
            || Instant::now() >= deadline
            || self.child.cancelled.load(Ordering::Acquire)
        {
            self.child.cancel();
            return result.and_then(|_| bail!("plug-in worker deadline exceeded or revoked"));
        }
        result
    }
}

pub(super) fn validate(
    executable: &Path,
    component: Vec<u8>,
    schemas: BTreeMap<String, CommandSchema>,
) -> Result<()> {
    let process = Process::spawn(executable)?;
    let result = process.exchange(
        Request::Bootstrap(Bootstrap {
            build: BuildIdentity::current(),
            component,
            operation: Operation::Validate { schemas },
        }),
        PREPARE_TIMEOUT,
        &mut Denied,
    );
    process.child.cancel();
    ensure!(
        matches!(result?, Response::Validated),
        "unexpected worker validation reply"
    );
    Ok(())
}

impl PreparedWorker {
    pub(super) fn activate(self) -> Result<ActiveWorker> {
        let response = self
            .0
            .exchange(Request::Activate, INVOKE_TIMEOUT, &mut Denied)?;
        ensure!(
            matches!(response, Response::Activated),
            "unexpected worker activation reply"
        );
        Ok(ActiveWorker(self.0))
    }
}

impl PendingWorker {
    pub(super) fn spawn(executable: &Path) -> Result<Self> {
        Ok(Self(Process::spawn(executable)?))
    }
    pub(super) fn cancellation(&self) -> Cancellation {
        Cancellation(self.0.child.clone())
    }
    pub(super) fn prepare(
        self,
        component: Vec<u8>,
        context: lifecycle::Context,
        assets: BTreeMap<String, Vec<u8>>,
        schemas: BTreeMap<String, CommandSchema>,
    ) -> Result<PreparedWorker> {
        let response = self.0.exchange(
            Request::Bootstrap(Bootstrap {
                build: BuildIdentity::current(),
                component,
                operation: Operation::Prepare {
                    context,
                    assets,
                    schemas,
                },
            }),
            PREPARE_TIMEOUT,
            &mut Denied,
        )?;
        ensure!(
            matches!(response, Response::Prepared),
            "unexpected worker readiness reply"
        );
        Ok(PreparedWorker(self.0))
    }
}

impl ActiveWorker {
    pub(super) fn failed(&self) -> bool {
        self.0.child.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn invoke(
        &self,
        event: lifecycle::Message,
        provider: &mut dyn ActivityProvider,
    ) -> Result<lifecycle::Reply> {
        match self
            .0
            .exchange(Request::Invoke(event), INVOKE_TIMEOUT, provider)?
        {
            Response::Reply(reply) => Ok(reply),
            Response::Rejected => bail!("command arguments do not match their schema"),
            _ => bail!("unexpected worker invocation reply"),
        }
    }

    pub(super) fn cancel(&self) {
        self.0.child.cancel();
    }
}

struct Watchdog {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn start(child: Arc<OwnedChild>, timeout: Duration) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pmux-plugin-deadline".into())
            .spawn(move || {
                if matches!(
                    receiver.recv_timeout(timeout),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    child.cancel();
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Denied;
impl ActivityProvider for Denied {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        Err(read::Error::NotActivated)
    }
    fn poll(
        &mut self,
        _: u64,
        _: Option<ActivityCursor>,
        _: u16,
    ) -> Result<ActivityPage, read::Error> {
        Err(read::Error::NotActivated)
    }
}

struct Broker {
    receiver: Arc<Mutex<mpsc::Receiver<Request>>>,
}

impl Broker {
    fn receive(&self) -> Result<Request, read::Error> {
        self.receiver
            .lock()
            .map_err(|_| read::Error::Revoked)?
            .recv()
            .map_err(|_| read::Error::Revoked)
    }
}

impl ActivityProvider for Broker {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
        wire::write(&mut std::io::stdout().lock(), &Response::Endpoints)
            .map_err(|_| read::Error::Revoked)?;
        match self.receive()? {
            Request::Endpoints(result) => result,
            _ => Err(read::Error::InvalidRequest),
        }
    }
    fn poll(
        &mut self,
        endpoint: u64,
        after: Option<ActivityCursor>,
        limit: u16,
    ) -> Result<ActivityPage, read::Error> {
        wire::write(
            &mut std::io::stdout().lock(),
            &Response::Poll {
                endpoint,
                after,
                limit,
            },
        )
        .map_err(|_| read::Error::Revoked)?;
        match self.receive()? {
            Request::Page(result) => result,
            _ => Err(read::Error::InvalidRequest),
        }
    }
}

pub(super) fn run() -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    // This thread sees parent EOF even during compilation or a guest call.
    // Only the private worker path exits here; no daemon state exists yet.
    std::thread::Builder::new()
        .name("pmux-plugin-parent".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            loop {
                let request = match wire::read(&mut input) {
                    Ok(request) => request,
                    Err(_) => std::process::exit(0),
                };
                if sender.send(request).is_err() {
                    return;
                }
            }
        })?;
    let receiver = Arc::new(Mutex::new(receiver));
    let result = run_requests(receiver);
    if let Err(error) = &result {
        let _ = wire::write(
            &mut std::io::stdout().lock(),
            &Response::Failed(wire::error_message(&error.to_string())),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "subprocess target for watchdog_terminates_a_blocked_native_child"]
    fn blocked_native_child() {
        std::thread::sleep(Duration::from_secs(2));
    }

    #[test]
    fn watchdog_terminates_a_blocked_native_child() {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["tests::blocked_native_child", "--ignored"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child = Arc::new(OwnedChild {
            child: Mutex::new(child),
            cancelled: AtomicBool::new(false),
        });
        let start = Instant::now();
        let watchdog = Watchdog::start(child.clone(), Duration::from_millis(50)).unwrap();
        while !child.cancelled.load(Ordering::Acquire) {
            assert!(start.elapsed() < Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(watchdog);
        assert!(!child.child.lock().unwrap().wait().unwrap().success());
    }
}

fn run_requests(receiver: Arc<Mutex<mpsc::Receiver<Request>>>) -> Result<()> {
    let receive = || {
        receiver
            .lock()
            .map_err(|_| anyhow::anyhow!("worker input failed"))?
            .recv()
            .context("parent closed worker input")
    };
    let Request::Bootstrap(bootstrap) = receive()? else {
        bail!("worker requires bootstrap");
    };
    ensure!(
        bootstrap.build == BuildIdentity::current(),
        "worker core/API/backend identity mismatch"
    );
    let runtime = Runtime::new()?;
    let (context, assets, schemas) = match bootstrap.operation {
        Operation::Validate { schemas } => {
            Schemas::compile(schemas)?;
            runtime.validate_component(&bootstrap.component)?;
            wire::write(&mut std::io::stdout().lock(), &Response::Validated)?;
            return Ok(());
        }
        Operation::Prepare {
            context,
            assets,
            schemas,
        } => (context, assets, Schemas::compile(schemas)?),
    };
    let prepared = runtime.prepare(
        &bootstrap.component,
        context,
        Box::new(Broker {
            receiver: receiver.clone(),
        }),
        assets,
    )?;
    wire::write(&mut std::io::stdout().lock(), &Response::Prepared)?;
    ensure!(
        matches!(receive()?, Request::Activate),
        "worker requires activation"
    );
    let mut active = prepared.activate();
    wire::write(&mut std::io::stdout().lock(), &Response::Activated)?;
    loop {
        match receive()? {
            Request::Invoke(event) => {
                let contribution = if let lifecycle::Message::Tool(request) = &event {
                    if schemas
                        .input(&request.contribution, &request.arguments)
                        .is_err()
                    {
                        wire::write(&mut std::io::stdout().lock(), &Response::Rejected)?;
                        continue;
                    }
                    Some(request.contribution.clone())
                } else {
                    None
                };
                let reply = active.handle(event)?;
                if let (Some(id), lifecycle::Reply::Tool(response)) = (contribution, &reply) {
                    schemas.output(&id, &response.json)?;
                }
                wire::write(&mut std::io::stdout().lock(), &Response::Reply(reply))?;
            }
            Request::Stop => {
                active.stop()?;
                wire::write(&mut std::io::stdout().lock(), &Response::Stopped)?;
                return Ok(());
            }
            _ => bail!("unexpected worker command"),
        }
    }
}
