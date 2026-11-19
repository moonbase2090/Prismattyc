// SPDX-License-Identifier: MIT
//! Coalesced socket readiness without reading transport bytes or joining on UI.

use prismattyc_process::fd;
use std::{
    io::{self, Read},
    net::Shutdown,
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

pub type Wake = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    armed: AtomicBool,
    stopped: AtomicBool,
    failed: AtomicBool,
    #[cfg(test)]
    probes: std::sync::Mutex<Vec<std::sync::mpsc::Sender<bool>>>,
    #[cfg(test)]
    exited: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
}
impl Shared {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            #[cfg(test)]
            probes: std::sync::Mutex::default(),
            #[cfg(test)]
            exited: std::sync::Mutex::default(),
        }
    }
}

pub struct Readiness {
    control: UnixStream,
    shared: Arc<Shared>,
}

impl Readiness {
    pub fn new(socket: Arc<UnixStream>, wake: Wake) -> io::Result<Self> {
        Self::start(socket, wake, Arc::new(Shared::new()))
    }

    fn start(socket: Arc<UnixStream>, wake: Wake, shared: Arc<Shared>) -> io::Result<Self> {
        let (control, receiver) = fd::socket_pair()?;
        let control = UnixStream::from(control);
        let receiver = UnixStream::from(receiver);
        control.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("native-readiness".into())
            .spawn(move || {
                run(&socket, receiver, &worker, &wake);
                drop(socket);
                #[cfg(test)]
                if let Some(done) = worker.exited.lock().unwrap().take() {
                    let _ = done.send(());
                }
            })?;
        Ok(Self { control, shared })
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// Rearm after the owner's receive attempt, or pause while it cannot read.
    pub fn set_armed(&self, armed: bool) -> io::Result<()> {
        self.shared.armed.store(armed, Ordering::Release);
        self.poke()
    }

    fn poke(&self) -> io::Result<()> {
        loop {
            // The private sender owns this endpoint. Nonblocking status also
            // bounds Darwin send; MSG_NOSIGNAL prevents a stopped worker's pipe signal.
            let sent = unsafe {
                libc::send(
                    self.control.as_raw_fd(),
                    [1u8].as_ptr().cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if sent == 1 {
                return Ok(());
            }
            if sent == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    #[cfg(test)]
    pub(super) fn observed_socket_watched(&self) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared.probes.lock().unwrap().push(tx);
        self.poke().unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap()
    }
}

impl Drop for Readiness {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        let _ = self.control.shutdown(Shutdown::Both);
    }
}

fn run(socket: &UnixStream, mut receiver: UnixStream, shared: &Shared, wake: &Wake) {
    while !shared.stopped.load(Ordering::Acquire) {
        let armed = shared.armed.load(Ordering::Acquire);
        let mut descriptors = [
            libc::pollfd {
                fd: receiver.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            // A zero event mask still reports POLLHUP. Omit the socket entirely
            // while paused, including when an unread peer has already closed.
            libc::pollfd {
                fd: if armed { socket.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        #[cfg(test)]
        for probe in shared.probes.lock().unwrap().drain(..) {
            let _ = probe.send(descriptors[1].fd >= 0);
        }
        let count = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if count < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            shared.failed.store(true, Ordering::Release);
            wake();
            return;
        }
        if shared.stopped.load(Ordering::Acquire) {
            return;
        }
        if descriptors[0].revents != 0 {
            match receiver.read(&mut [0; 4096]) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    shared.failed.store(true, Ordering::Release);
                    wake();
                    return;
                }
            }
        }
        if descriptors[1].revents != 0 && shared.armed.swap(false, Ordering::AcqRel) {
            wake();
        }
    }
}

#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;
