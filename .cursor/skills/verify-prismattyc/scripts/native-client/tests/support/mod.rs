// SPDX-License-Identifier: MIT
use native_client_probe::control::{Control, Reply};
use native_component_proof::service::NativeService;
use std::{
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub fn install(host: &mut NativeService, wake: &Receiver<()>, now: Instant) {
    loop {
        host.poll(now).unwrap();
        if host.scene().is_some() {
            return;
        }
        wake.recv_timeout(Duration::from_millis(200)).unwrap();
    }
}

pub fn finish(client: &mut Control) -> Reply {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        if let Some(reply) = client.step(Instant::now()).unwrap() {
            return reply;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "control reply deadline");
        let mut descriptor = libc::pollfd {
            fd: client.raw_fd(),
            events: if client.wants_write() {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().max(1) as i32) };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert!(ready > 0, "control reply readiness");
    }
}
