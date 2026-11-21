// SPDX-License-Identifier: MIT
use super::super::{
    frame::{literals as frames, Glyph},
    scene::tests::Producer,
    wire12::literals,
};
use super::*;
use prismattyc_process::fd;
use std::{
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, OwnedFd},
};

fn frame_literal() -> Vec<u8> {
    let mut bytes = frames::FRAME_INITIAL.to_vec();
    bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
    bytes[96..104].copy_from_slice(&1u64.to_le_bytes());
    bytes
}
fn control(literal: &[u8], sequence: u64) -> Vec<u8> {
    let mut bytes = literal.to_vec();
    bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
    bytes[24..32].copy_from_slice(&1u64.to_le_bytes());
    bytes
}
fn receive(client: &UnixStream, rights: usize) -> (Vec<u8>, Option<OwnedFd>) {
    // Protocol deadlines use the fixture clock; this only bounds a hung fixture.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = vec![0; 4];
    let mut used = 0;
    let mut descriptors = Vec::new();
    while used < bytes.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "native reply deadline");
        let mut event = libc::pollfd {
            fd: client.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut event, 1, remaining.as_millis().max(1) as i32) } > 0,
            "native reply deadline"
        );
        let received = fd::receive(
            client.as_fd(),
            &mut bytes[used..],
            rights - descriptors.len(),
        )
        .unwrap();
        assert!(received.bytes > 0);
        used += received.bytes;
        descriptors.extend(received.descriptors);
        if used == 4 && bytes.len() == 4 {
            let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert!((32..=128).contains(&length));
            bytes.resize(length, 0);
        }
    }
    assert_eq!(descriptors.len(), rights);
    (bytes, descriptors.into_iter().next())
}
fn setup() -> (NativeService, UnixStream, OwnedFd, mpsc::Receiver<()>) {
    let _clock = PausedClockScope::enter();
    let (host, client) = fd::socket_pair().unwrap();
    let mut client = UnixStream::from(client);
    client.set_nonblocking(true).unwrap();
    let now = Instant::now();
    let mut service =
        NativeService::new(host.into(), Geometry::new(1, 1, 8, 16).unwrap(), now).unwrap();
    let (tx, rx) = mpsc::channel();
    service
        .set_wake(Arc::new(move || {
            let _ = tx.send(());
        }))
        .unwrap();
    let offer = service
        .prepare_offer(NonZeroU32::new(1).unwrap(), Instant::now())
        .unwrap();
    service.authorize_written_offer(offer.receipt);
    let mut hello = literals::HELLO.to_vec();
    hello[24..32].copy_from_slice(&offer.epoch.get().to_le_bytes());
    hello[32..36].copy_from_slice(&offer.capability_id.get().to_le_bytes());
    hello[40..56].copy_from_slice(&offer.value);
    client.write_all(&hello).unwrap();
    let (reply, descriptor) = receive(&client, 1);
    assert_eq!(u16::from_le_bytes(reply[4..6].try_into().unwrap()), 9);
    (service, client, descriptor.unwrap(), rx)
}
fn activate() -> (NativeService, UnixStream, OwnedFd) {
    let (mut service, mut client, descriptor, wake) = setup();
    let producer = Producer::new(descriptor.try_clone().unwrap());
    producer.publish(1, &frame_literal());
    advance_time(&service, FRAME_TURN);
    client.write_all(&control(literals::ACTIVATE, 2)).unwrap();
    wake.recv_timeout(Duration::from_secs(2)).unwrap();
    service.poll(Instant::now()).unwrap();
    let (active, _) = receive(&client, 0);
    assert_eq!(&active[4..6], &4u16.to_le_bytes());
    assert_eq!(service.scene().unwrap().frame().context.sequence.get(), 1);
    (service, client, descriptor)
}
fn advance_time(service: &NativeService, duration: Duration) {
    service.advance_clock(duration);
}
fn resume_time(service: &NativeService) {
    // Resume at this real instant. Protocol time advances by elapsed wall time,
    // and fixture_response restores the unchanged real COMMAND_BOUND.
    service
        .test_clock
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .resumed_at = Some(Instant::now());
}
fn stop(service: &mut NativeService) {
    service.close();
    service.exited.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn control_and_ring_progress_continue_while_a_paint_snapshot_is_held() {
    let (mut service, mut client, descriptor) = activate();
    let painted = Arc::clone(service.scene().unwrap());
    let (progress, consumed) = mpsc::channel();
    service
        .request(move |w| {
            w.progress = Some(progress);
            Ok(())
        })
        .unwrap();
    let (finished, barrier) = mpsc::channel();
    let (release, painted_done) = mpsc::channel();
    let producer = thread::spawn(move || {
        let producer = Producer::new(descriptor);
        let start = Instant::now();
        let mut frame = 2;
        loop {
            let mut literal = frame_literal();
            literal[128] = b'Q';
            producer.publish(frame, &literal);
            let deadline = Instant::now() + Duration::from_secs(2);
            while consumed
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
                != frame
            {}
            assert_eq!(producer.counter(128).load(Ordering::Acquire), frame);
            client
                .write_all(&control(literals::PING_IDLE, frame + 1))
                .unwrap();
            let (pong, _) = receive(&client, 0);
            assert_eq!(&pong[4..6], &15u16.to_le_bytes());
            assert_eq!(&pong[16..24], &(frame + 1).to_le_bytes());
            if frame >= 16 && start.elapsed() >= Duration::from_millis(200) {
                break;
            }
            frame += 1;
        }
        let elapsed = start.elapsed();
        finished.send((elapsed, frame)).unwrap();
        painted_done.recv().unwrap();
        elapsed
    });
    // The UI paint continuation stays behind this barrier. No service.poll runs.
    let (elapsed, newest) = barrier.recv().unwrap();
    assert!(elapsed >= Duration::from_millis(200));
    assert_eq!(painted.frame().context.sequence.get(), 1);
    assert_eq!(service.scene().unwrap().frame().context.sequence.get(), 1);
    service.poll(Instant::now()).unwrap();
    assert_eq!(
        service.scene().unwrap().frame().context.sequence.get(),
        newest
    );
    assert_eq!(
        service.scene().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('Q')
    );
    release.send(()).unwrap();
    producer.join().unwrap();
    stop(&mut service);
}

#[test]
fn revoke_between_snapshot_and_ack_never_releases_active() {
    let (mut service, mut client, descriptor, wake) = setup();
    let producer = Producer::new(descriptor);
    producer.publish(1, &frame_literal());
    advance_time(&service, FRAME_TURN);
    client.write_all(&control(literals::ACTIVATE, 2)).unwrap();
    wake.recv_timeout(Duration::from_secs(2)).unwrap();
    service
        .request(|w| {
            let now = w.now_clock();
            w.native.poll_for_service(now, FrameWork::InstallPending)
        })
        .unwrap();
    let token = service.cached.installation.unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    service
        .request(|w| {
            let now = w.now_clock();
            w.native.pty_bytes(now)
        })
        .unwrap();
    assert!(!service
        .request(move |w| {
            let now = w.now_clock();
            w.native.acknowledge_installation(token, now)
        })
        .unwrap());
    let (ended, _) = receive(&client, 0);
    assert_eq!(&ended[4..6], &14u16.to_le_bytes());
    assert!(service.scene().is_none());
    stop(&mut service);
}

#[test]
fn worker_side_revocation_still_marks_cached_native_damage_before_pty_parse() {
    let (mut service, _client, _descriptor) = activate();
    assert!(service.cached.scene.is_some());
    let (changed, observed) = mpsc::channel();
    service
        .sender
        .send(Message::Call(Box::new(move |w| {
            let now = w.now_clock();
            w.native.pty_bytes(now).unwrap();
            w.publish(true);
            changed.send(()).unwrap();
        })))
        .unwrap();
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(service.cached.scene.is_some());
    assert!(service.scene().is_none());
    assert!(service.pty_bytes(Instant::now()).unwrap());
    assert!(service.cached.scene.is_none());
    stop(&mut service);
}

#[test]
fn a_stalled_worker_fails_closed_without_holding_the_ui_command() {
    let (mut service, _client, _descriptor) = activate();
    let (entered, waiting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    service
        .sender
        .send(Message::Call(Box::new(move |_w| {
            entered.send(()).unwrap();
            // Longer than the real watchdog, shorter than a fixture hang guard.
            gate.recv_timeout(Duration::from_millis(500)).unwrap();
        })))
        .unwrap();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    resume_time(&service);
    assert!(!service.take_revocation());
    assert!(service.pty_bytes(Instant::now()).is_err());
    assert!(!service.available());
    assert!(service.scene().is_none());
    release.send(()).unwrap();
    service.exited.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn a_worker_panic_closes_transport_and_invalidates_the_cached_scene() {
    let (mut service, client, _descriptor) = activate();
    service
        .sender
        .send(Message::Call(Box::new(|_| {
            panic!("injected worker failure")
        })))
        .unwrap();
    service.exited.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!service.available());
    assert!(service.scene().is_none());
    assert!(service.take_revocation());
    assert!(service.cached.scene.is_none());
    let mut client = &client;
    assert_eq!(client.read(&mut [0]).unwrap(), 0);
    service.close();
}

#[test]
fn invalid_ping_identity_closes_the_worker_without_renewing_native_ownership() {
    for (offset, value) in [(8, 2u64), (16, 4), (24, 2)] {
        let (mut service, mut client, _descriptor) = activate();
        let mut invalid = control(literals::PING_IDLE, 3);
        invalid[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        client.write_all(&invalid).unwrap();
        service.exited.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!service.available());
        assert!(service.scene().is_none());
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        service.close();
    }
}

struct BlockedMaximumSlot {
    service: NativeService,
    client: UnixStream,
    producer: Producer,
    release: mpsc::Sender<()>,
    epoch: PtyEpoch,
}

fn blocked_maximum_slot() -> BlockedMaximumSlot {
    let (mut service, mut client, _old_descriptor, _wake) = setup();
    // Maximum cell and text extents fill the entire 1 MiB slot.
    service
        .resize(Geometry::new(256, 128, 8, 16).unwrap(), Instant::now())
        .unwrap();
    // Resize revoked setup; negotiate a new generation with its own literal context.
    let offer = service
        .prepare_offer(NonZeroU32::new(2).unwrap(), Instant::now())
        .unwrap();
    service.authorize_written_offer(offer.receipt);
    let mut hello = literals::HELLO.to_vec();
    hello[24..32].copy_from_slice(&offer.epoch.get().to_le_bytes());
    hello[32..36].copy_from_slice(&offer.capability_id.get().to_le_bytes());
    hello[40..56].copy_from_slice(&offer.value);
    client.write_all(&hello).unwrap();
    let (_reply, descriptor) = receive(&client, 1);
    let producer = Producer::new(descriptor.unwrap());
    let mut bytes = frame_literal();
    let cell = bytes[128..144].to_vec();
    bytes.resize(1_048_576, 0);
    for (offset, value) in [
        (0, 1_048_576u32),
        (24, 256),
        (28, 128),
        (32, 32_768),
        (40, 16_376),
        (44, 524_416),
        (48, 262_144),
        (52, 786_432),
        (60, 1_048_576),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[16..24].copy_from_slice(&2u64.to_le_bytes());
    bytes[96..104].copy_from_slice(&2u64.to_le_bytes());
    for (index, output) in bytes[128..524_416].chunks_exact_mut(16).enumerate() {
        output.copy_from_slice(&cell);
        output[..4].copy_from_slice(&(0x8000_0000u32 + (index % 16_376) as u32).to_le_bytes());
    }
    let text = "X\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{1abf}";
    assert_eq!(text.len(), 16);
    let mut used = 0;
    for index in 0..16_376 {
        let value = if index < 64 {
            format!("{text}\u{301}")
        } else {
            text.to_string()
        };
        let entry = 524_416 + index * 16;
        bytes[entry..entry + 4].copy_from_slice(&(used as u32).to_le_bytes());
        bytes[entry + 4..entry + 8].copy_from_slice(&(value.len() as u32).to_le_bytes());
        bytes[entry + 8] = 1;
        bytes[786_432 + used..786_432 + used + value.len()].copy_from_slice(value.as_bytes());
        used += value.len();
    }
    assert_eq!(used, 262_144);
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    service
        .request(move |w| {
            w.before_frame = Some(Box::new(move || {
                entered.send(()).unwrap();
                // A fixture hang guard must not substitute for the real watchdog.
                gate.recv_timeout(Duration::from_millis(500)).unwrap();
            }));
            Ok(())
        })
        .unwrap();
    producer.publish(1, &bytes);
    let mut activate = control(literals::ACTIVATE, 2);
    activate[8..16].copy_from_slice(&2u64.to_le_bytes());
    activate[24..32].copy_from_slice(&2u64.to_le_bytes());
    advance_time(&service, FRAME_TURN);
    client.write_all(&activate).unwrap();
    entry.recv_timeout(Duration::from_secs(2)).unwrap();
    BlockedMaximumSlot {
        service,
        client,
        producer,
        release,
        epoch: offer.epoch,
    }
}

#[test]
fn a_maximum_slot_turn_keeps_the_public_revoke_bounded_or_fails_closed() {
    let BlockedMaximumSlot {
        mut service,
        client: _client,
        producer,
        release,
        epoch,
    } = blocked_maximum_slot();
    service.before_request = Some(Box::new(move |_| {
        release.send(()).unwrap();
    }));
    resume_time(&service);
    let started = Instant::now();
    let result = service.pty_bytes(started);
    let elapsed = started.elapsed();
    assert!(service.scene().is_none());
    match result {
        Ok(_) => {
            assert!(service.epoch().get() > epoch.get());
            assert_eq!(producer.counter(128).load(Ordering::Acquire), 1);
        }
        Err(AuthorityError::Io(error)) if error.kind() == io::ErrorKind::TimedOut => {
            assert!(!service.available());
            assert!(service.cached.scene.is_none());
        }
        Err(error) => panic!("unexpected public revoke result: {error:?}"),
    }
    let watchdog_closed = !service.available();
    stop(&mut service);
    eprintln!(
        "maximum slot public revoke: {}us, max worker turn {}us, closed {}",
        elapsed.as_micros(),
        service.max_turn_us(),
        watchdog_closed
    );
}

#[test]
fn a_blocked_maximum_slot_cannot_extend_the_public_command_watchdog() {
    let BlockedMaximumSlot {
        mut service,
        client: _client,
        producer,
        release,
        ..
    } = blocked_maximum_slot();
    resume_time(&service);
    let error = service.pty_bytes(Instant::now()).unwrap_err();
    assert!(matches!(error, AuthorityError::Io(error) if error.kind() == io::ErrorKind::TimedOut));
    assert!(!service.available());
    assert!(service.scene().is_none());
    assert!(service.cached.scene.is_none());
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 0);
    // Only the completed watchdog call releases the worker's frame-copy gate.
    release.send(()).unwrap();
    stop(&mut service);
}

#[test]
fn a_revoke_between_cache_check_and_pty_command_still_marks_damage() {
    let (mut service, _client, _descriptor) = activate();
    service.before_request = Some(Box::new(|commands| {
        let (revoked, observed) = mpsc::sync_channel(1);
        commands
            .send(Message::Call(Box::new(move |w| {
                let now = w.now_clock();
                assert!(w.native.pty_bytes(now).unwrap());
                w.publish(true);
                revoked.send(()).unwrap();
            })))
            .unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
    }));
    assert!(service.pty_bytes(Instant::now()).unwrap());
    assert!(service.cached.scene.is_none());
    stop(&mut service);
}

#[test]
fn a_full_ring_drains_to_the_newest_pending_scene_between_control_requests() {
    let (mut service, mut client, descriptor) = activate();
    let producer = Producer::new(descriptor);
    let (progress, consumed) = mpsc::channel();
    let (entered, entry) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    service
        .request(move |w| {
            w.progress = Some(progress);
            w.before_frame = Some(Box::new(move || {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(2)).unwrap();
            }));
            Ok(())
        })
        .unwrap();
    let mut literal = frame_literal();
    literal[128] = b'C';
    producer.publish(2, &literal);
    entry.recv_timeout(Duration::from_secs(2)).unwrap();
    for frame in 3..=5 {
        literal[128] = b'A' + frame as u8;
        producer.publish(frame, &literal);
    }
    client.write_all(&control(literals::PING_IDLE, 3)).unwrap();
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while consumed
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
        != 5
    {}
    // The worker handles and flushes the queued PING between these ring turns.
    let (pong, _) = receive(&client, 0);
    assert_eq!(&pong[4..6], &15u16.to_le_bytes());
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 5);
    assert_eq!(service.scene().unwrap().frame().context.sequence.get(), 1);
    service.poll(Instant::now()).unwrap();
    assert_eq!(service.scene().unwrap().frame().context.sequence.get(), 5);
    assert_eq!(
        service.scene().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('F')
    );
    stop(&mut service);
}
