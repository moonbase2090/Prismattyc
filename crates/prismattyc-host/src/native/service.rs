// SPDX-License-Identifier: MIT
//! A single native authority keeps control and ring progress independent of UI paint.

use super::{
    authority::{
        AuthorityError, DesktopNative, FrameWork, Geometry, Installation, OfferReceipt,
        PreparedOffer,
    },
    readiness::Wake,
    scene::NativeScene,
    transport::AVAILABILITY,
    wire12::PtyEpoch,
};
use std::{
    io,
    net::Shutdown,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const COMMAND_BOUND: Duration = Duration::from_millis(100);
const FRAME_TURN: Duration = Duration::from_millis(20);

type Task = Box<dyn FnOnce(&mut Worker) + Send>;
enum Message {
    Ready,
    Call(Task),
    Stop,
}

struct Shared {
    available: AtomicBool,
    ready_pending: AtomicBool,
    epoch: AtomicU64,
    max_turn_us: AtomicU64,
    wake: Mutex<Option<Wake>>,
}
impl Shared {
    fn wake(&self) {
        let wake = self.wake.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
    fn invalidate(&self) {
        self.available.store(false, Ordering::Release);
        self.wake();
    }
}

struct Snapshot {
    epoch: PtyEpoch,
    geometry: Geometry,
    scene: Option<Arc<NativeScene>>,
    installation: Option<Installation>,
}
struct Worker {
    native: DesktopNative,
    shared: Arc<Shared>,
    clock: Instant,
    next_frame: Instant,
    notified: bool,
    #[cfg(test)]
    progress: Option<mpsc::Sender<u64>>,
    #[cfg(test)]
    before_frame: Option<Box<dyn FnOnce() + Send>>,
}
impl Worker {
    fn now(&mut self, supplied: Instant) -> Instant {
        self.clock = self.clock.max(supplied).max(Instant::now());
        self.clock
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            epoch: self.native.epoch(),
            geometry: self.native.geometry(),
            scene: self.native.scene().cloned(),
            installation: self.native.installation(),
        }
    }
    fn publish(&mut self, changed: bool) {
        self.shared
            .epoch
            .store(self.native.epoch().get(), Ordering::Release);
        if !self.native.available() {
            self.shared.invalidate();
        }
        if (changed || self.native.has_pending_frame()) && !self.notified {
            self.notified = true;
            self.shared.wake();
        }
    }
    fn turn(&mut self) -> Result<(), AuthorityError> {
        let started = Instant::now();
        let now = self.now(started);
        let mut changed = self.native.poll_for_service(now, FrameWork::Keep)?;
        if now >= self.next_frame {
            #[cfg(test)]
            if self.native.awaiting_initial_frame() {
                if let Some(before) = self.before_frame.take() {
                    before();
                }
            }
            let now = self.now_clock();
            changed |= self.native.poll_for_service(now, FrameWork::Prepare)?;
            self.next_frame = self.clock + FRAME_TURN;
        }
        self.publish(changed);
        #[cfg(test)]
        if let (Some(progress), Some(sequence)) = (&self.progress, self.native.pending_sequence()) {
            let _ = progress.send(sequence.get());
        }
        self.shared.max_turn_us.fetch_max(
            started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        Ok(())
    }
    fn now_clock(&mut self) -> Instant {
        self.now(Instant::now())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.invalidate();
        self.native.close();
    }
}

pub struct NativeService {
    sender: mpsc::SyncSender<Message>,
    shutdown: UnixStream,
    shared: Arc<Shared>,
    cached: Snapshot,
    last_advance: Instant,
    #[cfg(test)]
    exited: mpsc::Receiver<()>,
}
impl NativeService {
    pub fn new(
        socket: UnixStream,
        geometry: Geometry,
        now: Instant,
    ) -> Result<Self, AuthorityError> {
        let shutdown = socket.try_clone()?;
        let shared = Arc::new(Shared {
            available: AtomicBool::new(true),
            ready_pending: AtomicBool::new(false),
            epoch: AtomicU64::new(1),
            max_turn_us: AtomicU64::new(0),
            wake: Mutex::new(None),
        });
        let (sender, receiver) = mpsc::sync_channel(2);
        let ready = sender.clone();
        let worker_shared = Arc::clone(&shared);
        let (initialized, initial) = mpsc::sync_channel(1);
        #[cfg(test)]
        let (finished, exited) = mpsc::channel();
        thread::Builder::new()
            .name("native-authority".into())
            .spawn(move || {
                #[cfg(test)]
                let _completion = Completion(finished);
                // Build the non-Send mapping owner here. No mutable mapping crosses threads.
                let readiness = Arc::clone(&worker_shared);
                let result = DesktopNative::new(socket, geometry, now).and_then(|mut native| {
                    native.start_watcher(Arc::new(move || {
                        if !readiness.ready_pending.swap(true, Ordering::AcqRel)
                            && ready.try_send(Message::Ready).is_err()
                        {
                            readiness.invalidate();
                        }
                    }))?;
                    Ok(native)
                });
                match result {
                    Ok(native) => {
                        let mut worker = Worker {
                            native,
                            shared: worker_shared,
                            clock: now,
                            next_frame: now,
                            notified: false,
                            #[cfg(test)]
                            progress: None,
                            #[cfg(test)]
                            before_frame: None,
                        };
                        if initialized.send(Ok(worker.snapshot())).is_ok() {
                            loop {
                                if !worker.shared.available.load(Ordering::Acquire) {
                                    break;
                                }
                                let now = worker.now_clock();
                                let message = if let Some(deadline) = worker.native.next_wakeup() {
                                    receiver.recv_timeout(
                                        deadline
                                            .min(worker.next_frame)
                                            .saturating_duration_since(now),
                                    )
                                } else {
                                    receiver
                                        .recv()
                                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                                };
                                match message {
                                    Ok(Message::Stop)
                                    | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                    Ok(Message::Call(task)) => task(&mut worker),
                                    Ok(Message::Ready) => {
                                        worker.shared.ready_pending.store(false, Ordering::Release);
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                }
                                if !worker.shared.available.load(Ordering::Acquire) {
                                    break;
                                }
                                if let Err(error) = worker.turn() {
                                    eprintln!("prismattyc-host: {error}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        worker_shared.invalidate();
                        let _ = initialized.send(Err(error));
                    }
                }
            })?;
        let cached = match initial.recv_timeout(COMMAND_BOUND) {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                shared.invalidate();
                let _ = shutdown.shutdown(Shutdown::Both);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native worker startup deadline",
                )
                .into());
            }
        };
        Ok(Self {
            sender,
            shutdown,
            shared,
            cached,
            last_advance: now,
            #[cfg(test)]
            exited,
        })
    }
    fn request<R: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut Worker) -> Result<R, AuthorityError> + Send + 'static,
    ) -> Result<R, AuthorityError> {
        if !self.available() {
            return Err(AuthorityError::Protocol("native worker unavailable"));
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let task = Box::new(move |worker: &mut Worker| {
            let result = operation(worker);
            worker.publish(false);
            let _ = sender.send((result, worker.snapshot()));
        });
        let response = if self.sender.try_send(Message::Call(task)).is_ok() {
            receiver.recv_timeout(COMMAND_BOUND).ok()
        } else {
            None
        };
        let Some((result, snapshot)) = response else {
            self.close();
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "native worker command deadline").into(),
            );
        };
        self.cached = snapshot;
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn available(&self) -> bool {
        self.shared.available.load(Ordering::Acquire) && AVAILABILITY.available()
    }
    pub fn epoch(&self) -> PtyEpoch {
        PtyEpoch::new(self.shared.epoch.load(Ordering::Acquire)).unwrap()
    }
    pub fn geometry(&self) -> Geometry {
        self.cached.geometry
    }
    pub fn scene(&self) -> Option<&Arc<NativeScene>> {
        if !self.available() || self.cached.epoch != self.epoch() {
            None
        } else {
            self.cached.scene.as_ref()
        }
    }
    fn discard_revoked_cache(&mut self) -> bool {
        if self.cached.scene.is_some() && self.scene().is_none() {
            self.cached.scene = None;
            true
        } else {
            false
        }
    }
    pub fn set_wake(&mut self, wake: Wake) -> io::Result<()> {
        let mut current = self.shared.wake.lock().unwrap_or_else(|p| p.into_inner());
        if current.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        *current = Some(wake);
        Ok(())
    }
    pub fn prepare_offer(
        &mut self,
        id: NonZeroU32,
        now: Instant,
    ) -> Result<PreparedOffer, AuthorityError> {
        self.request(move |w| {
            let now = w.now(now);
            w.native.prepare_offer(id, now)
        })
    }
    pub fn authorize_written_offer(&mut self, receipt: OfferReceipt) {
        let _ = self.request(move |w| {
            w.native.authorize_written_offer(receipt);
            Ok(())
        });
    }
    pub fn pty_bytes(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        let invalidated = self.discard_revoked_cache();
        self.request(move |w| {
            let now = w.now(now);
            w.native.pty_bytes(now)
        })
        .map(|changed| changed || invalidated)
    }
    pub fn resize(&mut self, geometry: Geometry, now: Instant) -> Result<bool, AuthorityError> {
        let invalidated = self.discard_revoked_cache();
        self.request(move |w| {
            let now = w.now(now);
            w.native.resize(geometry, now)
        })
        .map(|changed| changed || invalidated)
    }
    pub fn poll(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        self.last_advance = now;
        let old = self.cached.scene.clone();
        self.request(move |w| {
            let now = w.now(now);
            w.notified = false;
            w.native.poll_for_service(now, FrameWork::InstallPending)
        })?;
        if let Some(token) = self.cached.installation {
            if !self.request(move |w| {
                let now = w.now_clock();
                w.native.acknowledge_installation(token, now)
            })? {
                self.cached.scene = None;
            }
        }
        Ok(match (old.as_ref(), self.scene()) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        })
    }
    pub fn expire(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        let invalidated = self.discard_revoked_cache();
        self.request(move |w| {
            let now = w.now(now);
            w.native.expire(now)
        })
        .map(|changed| changed || invalidated)
    }
    pub fn next_wakeup(&self) -> Option<Instant> {
        // Control and lease deadlines belong to the worker; the UI advances visible scenes.
        (self.available() && (self.cached.scene.is_some() || self.cached.installation.is_some()))
            .then_some(self.last_advance + FRAME_TURN)
    }
    pub fn max_turn_us(&self) -> u64 {
        self.shared.max_turn_us.load(Ordering::Relaxed)
    }

    pub fn close(&mut self) {
        self.cached.scene = None;
        self.shared.invalidate();
        let _ = self.shutdown.shutdown(Shutdown::Both);
        let _ = self.sender.try_send(Message::Stop);
    }
}
impl Drop for NativeService {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
struct Completion(mpsc::Sender<()>);
#[cfg(test)]
impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
