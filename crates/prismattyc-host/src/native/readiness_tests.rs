// SPDX-License-Identifier: MIT
use super::*;
use std::{
    io::{Read, Write},
    sync::{mpsc, Mutex},
    time::Duration,
};

fn pair() -> (Arc<UnixStream>, UnixStream) {
    let (host, client) = fd::socket_pair().unwrap();
    (Arc::new(host.into()), client.into())
}
fn watch(socket: Arc<UnixStream>, wake: Wake) -> (Readiness, mpsc::Receiver<()>) {
    let shared = Arc::new(Shared::new());
    let (tx, rx) = mpsc::channel();
    *shared.exited.lock().unwrap() = Some(tx);
    (Readiness::start(socket, wake, shared).unwrap(), rx)
}
fn receive(rx: &mpsc::Receiver<()>) {
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn readiness_coalesces_until_an_owner_receive_and_never_consumes_the_bytes() {
    let (socket, mut client) = pair();
    let (tx, rx) = mpsc::channel();
    let (watcher, exited) = watch(
        Arc::clone(&socket),
        Arc::new(move || {
            tx.send(()).unwrap();
        }),
    );
    client.write_all(b"a").unwrap();
    receive(&rx);
    assert!(!watcher.observed_socket_watched());
    client.write_all(b"b").unwrap();
    assert!(!watcher.observed_socket_watched());
    assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    let mut bytes = [0; 2];
    socket.as_ref().read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, *b"ab");
    watcher.set_armed(true).unwrap();
    assert!(watcher.observed_socket_watched());
    client.write_all(b"c").unwrap();
    receive(&rx);
    let mut byte = [0];
    socket.as_ref().read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'c']);
    drop(watcher);
    receive(&exited);
}

#[test]
fn closing_an_unarmed_peer_does_not_rearm_or_repeat_wakes() {
    let (socket, mut client) = pair();
    let (tx, rx) = mpsc::channel();
    let (watcher, exited) = watch(
        socket,
        Arc::new(move || {
            tx.send(()).unwrap();
        }),
    );
    client.write_all(b"held").unwrap();
    receive(&rx);
    assert!(!watcher.observed_socket_watched());
    drop(client);
    assert!(!watcher.observed_socket_watched());
    assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    watcher.set_armed(true).unwrap();
    receive(&rx);
    assert!(!watcher.observed_socket_watched());
    drop(watcher);
    receive(&exited);
}

#[test]
fn dropping_a_watcher_never_joins_a_blocked_wake_and_eventually_closes_its_socket() {
    let (socket, mut client) = pair();
    let retained = Arc::downgrade(&socket);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let finish = Mutex::new(finish_rx);
    let (watcher, exited) = watch(
        socket,
        Arc::new(move || {
            entered_tx.send(()).unwrap();
            finish.lock().unwrap().recv().unwrap();
        }),
    );
    client.write_all(b"wake").unwrap();
    receive(&entered_rx);
    let (drop_tx, drop_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(watcher);
        drop_tx.send(()).unwrap();
    });
    receive(&drop_rx);
    assert!(retained.upgrade().is_some());
    finish_tx.send(()).unwrap();
    receive(&exited);
    dropper.join().unwrap();
    assert!(retained.upgrade().is_none());
}
