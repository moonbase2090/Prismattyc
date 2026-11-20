// SPDX-License-Identifier: MIT
use super::super::{
    frame::{literals as frame_literals, Glyph},
    scene::tests::Producer,
    transport::tests::send_rights,
    wire12::literals,
};
use super::*;
use prismattyc_process::fd;
use std::{
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
};

fn pair() -> (UnixStream, UnixStream) {
    let (left, right) = fd::socket_pair().unwrap();
    (left.into(), right.into())
}
fn owner(now: Instant) -> (DesktopNative, UnixStream) {
    let (host, client) = pair();
    let mut native = DesktopNative::new(host, Geometry::new(1, 1, 8, 16).unwrap(), now).unwrap();
    native.pool = ImagePool::isolated();
    (native, client)
}
fn hello(offer: &PreparedOffer) -> Vec<u8> {
    let mut bytes = literals::HELLO.to_vec();
    bytes[24..32].copy_from_slice(&offer.epoch.get().to_le_bytes());
    bytes[32..36].copy_from_slice(&offer.capability_id.get().to_le_bytes());
    bytes[40..56].copy_from_slice(&offer.value);
    bytes
}
fn control(literal: &[u8], session: u64, epoch: u64, sequence: u64) -> Vec<u8> {
    let mut bytes = literal.to_vec();
    bytes[8..16].copy_from_slice(&session.to_le_bytes());
    bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
    bytes[24..32].copy_from_slice(&epoch.to_le_bytes());
    bytes
}
fn response(client: &UnixStream, count: usize) -> (Vec<u8>, Option<OwnedFd>) {
    let mut prefix = [0; 4];
    let received = fd::receive(client.as_fd(), &mut prefix, count).unwrap();
    assert_eq!(received.bytes, 4);
    assert_eq!(received.descriptors.len(), count);
    let length = u32::from_le_bytes(prefix) as usize;
    assert!((32..=128).contains(&length));
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(&prefix);
    assert_eq!(
        fd::receive(client.as_fd(), &mut bytes[4..], 0)
            .unwrap()
            .bytes,
        length - 4
    );
    (bytes, received.descriptors.into_iter().next())
}
fn opcode(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[4..6].try_into().unwrap())
}
fn value(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn setup(native: &mut DesktopNative, client: &mut UnixStream, now: Instant) -> Producer {
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    client.write_all(&hello(&offer)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (reply, descriptor) = response(client, 1);
    assert_eq!(opcode(&reply), 9);
    assert_eq!(value(&reply, 8), 1);
    assert_eq!(value(&reply, 24), 1);
    Producer::new(descriptor.unwrap())
}
fn publish(producer: &Producer, literal: &[u8], sequence: u64, epoch: u64, metrics: u64) {
    let mut bytes = literal.to_vec();
    bytes[16..24].copy_from_slice(&epoch.to_le_bytes());
    bytes[96..104].copy_from_slice(&metrics.to_le_bytes());
    producer.publish(sequence, &bytes);
}
fn active(native: &mut DesktopNative, client: &mut UnixStream, now: Instant) -> Producer {
    let producer = setup(native, client, now);
    publish(&producer, frame_literals::FRAME_INITIAL, 1, 1, 1);
    client
        .write_all(&control(literals::ACTIVATE, 1, 1, 2))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (reply, descriptor) = response(client, 0);
    assert!(descriptor.is_none());
    assert_eq!(opcode(&reply), 4);
    assert_eq!(reply[6..8], [2, 0]);
    assert_eq!(native.scene().unwrap().frame().context.sequence.get(), 1);
    producer
}
fn eof(mut client: UnixStream) {
    client.set_nonblocking(true).unwrap();
    assert_eq!(client.read(&mut [0]).unwrap(), 0);
}

#[test]
fn hello_requires_the_exact_offer_to_have_a_successful_pty_write_receipt() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    client.write_all(&hello(&offer)).unwrap();
    assert!(!native.poll(now).unwrap());
    assert!(native.session.is_none());
    assert!(matches!(
        native.poll(now + Duration::from_millis(200)),
        Err(AuthorityError::Protocol("native offer write deadline"))
    ));
    assert!(!native.available());
    eof(client);
    let (mut native, mut client) = owner(now);
    let old = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    let current = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(old.receipt);
    client.write_all(&hello(&current)).unwrap();
    assert!(!native.poll(now).unwrap());
    assert!(native.session.is_none());
    assert!(native.poll(now + Duration::from_millis(200)).is_err());
    eof(client);
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    let mut wrong = hello(&offer);
    wrong[40] ^= 1;
    client.write_all(&wrong).unwrap();
    assert!(native.poll(now).is_err());
    eof(client);
}

#[test]
fn an_early_matching_hello_waits_for_the_writer_receipt_without_allocating_or_replying() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    client.write_all(&hello(&offer)).unwrap();
    assert!(!native.poll(now).unwrap());
    assert!(native.session.is_none());
    assert!(native.scene().is_none());
    client.set_nonblocking(true).unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    native.authorize_written_offer(offer.receipt);
    let completed = now + Duration::from_millis(20);
    native.poll(completed).unwrap();
    native.poll(completed).unwrap();
    let (reply, descriptor) = response(&client, 1);
    assert_eq!(opcode(&reply), 9);
    assert_eq!(value(&reply, 8), 1);
    assert!(native.scene().is_none());
    drop(descriptor);
}

#[test]
fn active_is_written_only_after_the_named_full_snapshot_is_actually_installed() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let producer = setup(&mut native, &mut client, now);
    client
        .write_all(&control(literals::ACTIVATE, 1, 1, 2))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    assert!(native.scene().is_none());
    client.set_nonblocking(true).unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    publish(&producer, frame_literals::FRAME_INITIAL, 1, 1, 1);
    assert!(native.poll(now).unwrap());
    assert_eq!(
        native.scene().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('?')
    );
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    native.poll(now).unwrap();
    let (reply, _) = response(&client, 0);
    assert_eq!(opcode(&reply), 4);
    assert_eq!(value(&reply, 32), 1);
}

#[test]
fn only_valid_current_control_renews_the_500ms_lease_even_with_a_retained_socket() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let producer = active(&mut native, &mut client, now);
    let retained_shell = client.try_clone().unwrap();
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    let ping = now + Duration::from_millis(400);
    native.poll(ping).unwrap();
    native.poll(ping).unwrap();
    assert_eq!(opcode(&response(&client, 0).0), 15);
    publish(&producer, frame_literals::FRAME_OMISSION, 2, 1, 1);
    native.poll(now + Duration::from_millis(899)).unwrap();
    assert_eq!(native.scene().unwrap().frame().context.sequence.get(), 2);
    assert!(native.poll(now + Duration::from_millis(900)).unwrap());
    assert!(native.scene().is_none());
    assert_eq!(native.epoch().get(), 2);
    assert!(native.available());
    retained_shell.set_nonblocking(true).unwrap();
    assert_eq!(
        (&retained_shell).read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn timer_expiry_releases_a_hidden_scene_without_receiving_its_queued_ping() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    let scene = Arc::downgrade(native.scene().unwrap());
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    assert!(!native.expire(now + Duration::from_millis(499)).unwrap());
    assert!(scene.upgrade().is_some());
    assert!(native.expire(now + Duration::from_millis(500)).unwrap());
    assert!(scene.upgrade().is_none());
    assert!(native.scene().is_none());
    assert_eq!(native.epoch().get(), 2);
    assert!(native.available());
    client.set_nonblocking(true).unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    native.poll(now + Duration::from_millis(500)).unwrap();
    native.poll(now + Duration::from_millis(500)).unwrap();
    let (ended, descriptors) = response(&client, 0);
    assert!(descriptors.is_none());
    assert_eq!(opcode(&ended), 14);
    assert_eq!(value(&ended, 24), 1);
    assert_eq!(value(&ended, 32), 2);
}

#[test]
fn a_resumed_old_session_after_long_idle_gets_ended_and_can_start_a_fresh_session() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _old_producer = active(&mut native, &mut client, now);
    assert!(native.expire(now + Duration::from_millis(500)).unwrap());
    assert!(native.available());
    assert_eq!(native.next_wakeup(), None);
    let resumed = now + Duration::from_secs(30);
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    native.poll(resumed).unwrap();
    native.poll(resumed).unwrap();
    let (ended, descriptor) = response(&client, 0);
    assert!(descriptor.is_none());
    assert_eq!(
        (
            opcode(&ended),
            value(&ended, 8),
            value(&ended, 16),
            value(&ended, 24),
            value(&ended, 32)
        ),
        (14, 1, 3, 1, 2)
    );
    assert_eq!(&ended[40..48], &[3, 0, 0, 0, 0, 0, 0, 0]);

    let fresh = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), resumed)
        .unwrap();
    native.authorize_written_offer(fresh.receipt.clone());
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 4))
        .unwrap();
    native.poll(resumed).unwrap();
    native.poll(resumed).unwrap();
    assert_eq!(opcode(&response(&client, 0).0), 14);
    client.write_all(&hello(&fresh)).unwrap();
    native.poll(resumed).unwrap();
    native.poll(resumed).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(
        (
            opcode(&setup),
            value(&setup, 8),
            value(&setup, 24),
            value(&setup, 64)
        ),
        (9, 2, 2, 2)
    );
    let producer = Producer::new(descriptor.unwrap());
    publish(&producer, frame_literals::FRAME_INITIAL, 1, 2, 2);
    client
        .write_all(&control(literals::ACTIVATE, 2, 2, 2))
        .unwrap();
    native.poll(resumed).unwrap();
    native.poll(resumed).unwrap();
    native.poll(resumed).unwrap();
    assert_eq!(opcode(&response(&client, 0).0), 4);
    assert_eq!(native.scene().unwrap().frame().context.epoch.get(), 2);

    let late = resumed + Duration::from_millis(250);
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 5))
        .unwrap();
    native.poll(late).unwrap();
    native.poll(late).unwrap();
    assert_eq!(opcode(&response(&client, 0).0), 14);
    assert!(native.scene().is_some());
    assert!(
        native.expire(resumed + Duration::from_millis(500)).unwrap(),
        "old traffic must not renew the current lease"
    );
}

#[test]
fn a_resumed_stale_drain_keeps_one_absolute_deadline() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    native.pty_bytes(now).unwrap();
    let resumed = now + Duration::from_secs(30);
    for elapsed in [0, 199] {
        let at = resumed + Duration::from_millis(elapsed);
        client
            .write_all(&control(literals::PING_IDLE, 1, 1, 3))
            .unwrap();
        native.poll(at).unwrap();
        native.poll(at).unwrap();
        assert_eq!(opcode(&response(&client, 0).0), 14);
    }
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    assert!(matches!(
        native.poll(resumed + Duration::from_millis(200)),
        Err(AuthorityError::Protocol("native stale drain limit"))
    ));
    assert!(!native.available());
    eof(client);
}

#[test]
fn a_late_first_stale_record_does_not_extend_the_fresh_discovery_deadline() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    native.pty_bytes(now).unwrap();
    let fresh = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(fresh.receipt);
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    let late = now + Duration::from_millis(199);
    native.poll(late).unwrap();
    native.poll(late).unwrap();
    assert_eq!(opcode(&response(&client, 0).0), 14);
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    assert!(matches!(
        native.poll(now + Duration::from_millis(200)),
        Err(AuthorityError::Protocol("native stale drain limit"))
    ));
    assert!(!native.available());
    eof(client);
}

#[test]
fn control_only_pty_output_revokes_before_rearm_and_old_records_cannot_authorize_the_new_session() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    assert!(native.pty_bytes(now).unwrap());
    assert!(native.scene().is_none());
    assert_eq!(native.epoch().get(), 2);
    let fresh = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(fresh.receipt.clone());
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (ended, _) = response(&client, 0);
    assert_eq!(opcode(&ended), 14);
    assert_eq!(
        (
            value(&ended, 8),
            value(&ended, 16),
            value(&ended, 24),
            value(&ended, 32)
        ),
        (1, 3, 1, 2)
    );
    assert_eq!(ended[40..44], [2, 0, 0, 0]);
    assert!(native.scene().is_none());
    client.write_all(&hello(&fresh)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(opcode(&setup), 9);
    assert_eq!(
        (
            value(&setup, 8),
            value(&setup, 16),
            value(&setup, 24),
            value(&setup, 64)
        ),
        (2, 1, 2, 2)
    );
    drop(descriptor);
    assert!(native.scene().is_none());
}

#[test]
fn stale_drain_is_bounded_to_four_records_without_advancing_current_authority() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    native.pty_bytes(now).unwrap();
    for _ in 0..4 {
        client
            .write_all(&control(literals::PING_IDLE, 1, 1, 3))
            .unwrap();
        native.poll(now).unwrap();
        native.poll(now).unwrap();
        assert_eq!(opcode(&response(&client, 0).0), 14);
    }
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    assert!(matches!(
        native.poll(now),
        Err(AuthorityError::Protocol("native stale drain limit"))
    ));
    assert_eq!(native.epoch().get(), 2);
    assert!(!native.available());
    eof(client);
}

#[test]
fn resize_removes_the_scene_before_new_geometry_is_offered() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    assert!(native
        .resize(Geometry::new(2, 1, 9, 17).unwrap(), now)
        .unwrap());
    assert!(native.scene().is_none());
    let fresh = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(fresh.receipt.clone());
    client.write_all(&hello(&fresh)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(
        setup[72..88],
        [9, 0, 0, 0, 17, 0, 0, 0, 18, 0, 0, 0, 17, 0, 0, 0]
    );
    assert_eq!(
        (value(&setup, 8), value(&setup, 24), value(&setup, 64)),
        (2, 2, 2)
    );
    drop(descriptor);
}

#[test]
fn unexpected_rights_on_active_or_stale_ping_close_the_endpoint_and_remove_native_content() {
    let now = Instant::now();
    for stale in [false, true] {
        let (mut native, mut client) = owner(now);
        let _producer = active(&mut native, &mut client, now);
        if stale {
            native.pty_bytes(now).unwrap();
        }
        let (source, source_peer) = pair();
        send_rights(
            &client,
            &control(literals::PING_IDLE, 1, 1, 3),
            &[source.as_fd()],
        );
        assert!(matches!(native.poll(now), Err(AuthorityError::Io(_))));
        assert!(native.scene().is_none());
        assert!(!native.available());
        drop(source);
        eof(source_peer);
        eof(client);
    }
}

#[test]
fn real_fd_exhaustion_cannot_leave_an_installed_scene_or_half_installed_rights() {
    const CHILD: &str = "PRISMATTYC_NATIVE_PROOF_EXHAUST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        let module = module_path!().split_once("::").unwrap().1;
        let test = format!(
            "{module}::real_fd_exhaustion_cannot_leave_an_installed_scene_or_half_installed_rights"
        );
        command
            .args([test.as_str(), "--exact"])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = prismattyc_process::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
            "the isolated descriptor-exhaustion child must execute its test"
        );
        return;
    }
    struct Limit(libc::rlimit);
    impl Drop for Limit {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
        }
    }
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    let (source, source_peer) = pair();
    let installed_scene = Arc::downgrade(native.scene().unwrap());
    send_rights(
        &client,
        &control(literals::PING_IDLE, 1, 1, 3),
        &[source.as_fd(); 8],
    );
    let mut saved: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut saved) },
        0
    );
    let restore = Limit(saved);
    let reduced = libc::rlimit {
        rlim_cur: 64,
        rlim_max: saved.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &reduced) }, 0);
    let mut fillers = Vec::new();
    loop {
        match std::fs::File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    let failure = native.poll(now);
    drop(restore);
    drop(fillers);
    let Err(AuthorityError::Io(error)) = failure else {
        panic!("exhaustion did not fail at the receive boundary");
    };
    #[cfg(target_os = "macos")]
    assert!(
        matches!(error.raw_os_error(), Some(libc::EMFILE | libc::EMSGSIZE)),
        "unexpected exhaustion: {error:?}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        error.raw_os_error() == Some(libc::EMFILE)
            || matches!(
                error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<fd::AncillaryFailure>()),
                Some(fd::AncillaryFailure::Truncated)
            )
    );
    assert!(native.scene().is_none());
    assert!(!native.available());
    assert!(installed_scene.upgrade().is_none());
    drop(source);
    eof(source_peer);
    #[cfg(target_os = "macos")]
    eof(client);
    #[cfg(target_os = "linux")]
    {
        // Closing with the unconsumed request remainder resets the Linux
        // control peer. The descriptor-source peer above must still see EOF.
        client.set_nonblocking(true).unwrap();
        match client.read(&mut [0]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("control endpoint remained usable: {other:?}"),
        }
    }
}

#[test]
fn wrong_session_sequence_future_epoch_and_unselected_attributes_never_renew_authority() {
    let now = Instant::now();
    for (session, epoch, sequence) in [(2, 1, 3), (1, 1, 4), (1, 2, 3)] {
        let (mut native, mut client) = owner(now);
        let _producer = active(&mut native, &mut client, now);
        let scene = Arc::downgrade(native.scene().unwrap());
        client
            .write_all(&control(literals::PING_IDLE, session, epoch, sequence))
            .unwrap();
        assert!(native.poll(now).is_err());
        assert!(!native.available());
        assert!(scene.upgrade().is_none());
        eof(client);
    }
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    let mut bytes = hello(&offer);
    bytes[6] = 1;
    client.write_all(&bytes).unwrap();
    assert!(native.poll(now).is_err());
    eof(client);
    let (mut native, mut client) = owner(now);
    let _producer = setup(&mut native, &mut client, now);
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 2))
        .unwrap();
    assert!(native.poll(now).is_err());
    eof(client);
}

#[test]
fn release_removes_the_overlay_before_released_and_fresh_hello_allocates_another_session() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    let scene = Arc::downgrade(native.scene().unwrap());
    let mut release = control(literals::PING_IDLE, 1, 1, 3);
    release[4..6].copy_from_slice(&5u16.to_le_bytes());
    client.write_all(&release).unwrap();
    assert!(native.poll(now).unwrap());
    assert!(native.scene().is_none());
    assert!(scene.upgrade().is_none());
    assert_eq!(native.epoch().get(), 2);
    client.set_nonblocking(true).unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    native.poll(now).unwrap();
    let (reply, _) = response(&client, 0);
    assert_eq!(opcode(&reply), 6);
    assert_eq!(reply.len(), 32);
    let offer = native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    client.write_all(&hello(&offer)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(
        (value(&setup, 8), value(&setup, 16), value(&setup, 24)),
        (2, 1, 2)
    );
    drop(descriptor);
}

#[test]
fn real_image_fd_upload_becomes_ready_and_only_retires_after_its_installed_omission() {
    use super::super::backing::tests as backing;
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let producer = active(&mut native, &mut client, now);
    let (source, physical) = backing::source();
    let mut put = control(literals::PUT_IMAGE, 1, 1, 3);
    put[64..72].copy_from_slice(&physical.to_le_bytes());
    #[cfg(target_os = "linux")]
    put[46..48].copy_from_slice(&1u16.to_le_bytes());
    send_rights(&client, &put, &[source.as_fd()]);
    native.poll(now).unwrap();
    drop(source);
    native.poll(now).unwrap();
    let (admitted, _) = response(&client, 0);
    assert_eq!(opcode(&admitted), 11);
    assert_eq!(value(&admitted, 32), 1);
    assert_eq!(admitted[40..48], [0; 8]);
    native
        .session
        .as_mut()
        .unwrap()
        .frames
        .finish_upload_for_test()
        .unwrap();
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 4))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (pong, _) = response(&client, 0);
    assert_eq!(opcode(&pong), 15);
    assert_eq!(value(&pong, 32), 1);
    assert_eq!(pong[40..48], [2, 0, 0, 0, 0, 0, 0, 0]);
    publish(&producer, frame_literals::FRAME_PLACEMENT, 2, 1, 1);
    assert!(native.poll(now).unwrap());
    assert_eq!(
        native.scene().unwrap().images()[0].pixels.pixels(),
        [255, 0, 0, 128]
    );
    client
        .write_all(&control(literals::RETIRE_IMAGE, 1, 1, 5))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (busy, _) = response(&client, 0);
    assert_eq!(opcode(&busy), 13);
    assert_eq!(busy[40..48], [1, 0, 0, 0, 0, 0, 0, 0]);
    publish(&producer, frame_literals::FRAME_OMISSION, 3, 1, 1);
    assert!(native.poll(now).unwrap());
    assert!(native.scene().unwrap().images().is_empty());
    client
        .write_all(&control(literals::RETIRE_IMAGE, 1, 1, 6))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (retired, _) = response(&client, 0);
    assert_eq!(opcode(&retired), 13);
    assert_eq!(value(&retired, 32), 1);
    assert_eq!(retired[40..48], [0; 8]);
}

#[test]
fn advertised_tokens_match_actual_setup_activation_and_ping_dialects_on_this_platform() {
    let now = Instant::now();
    let expected = if cfg!(target_os = "linux") {
        "native.grid.v1,native.images.v1"
    } else {
        "native.images.v1"
    };
    for minor in [0u16, 2] {
        let (mut native, mut client) = owner(now);
        let offer = native
            .prepare_offer(NonZeroU32::new(17).unwrap(), now)
            .unwrap();
        let advertised =
            String::from_utf8(super::super::profile::encode_reply(&offer).unwrap()).unwrap();
        assert!(advertised.starts_with(&format!(
            "\x1b_Prismattyc;cap;r;id=17;v=0.4;features={expected};native.offer="
        )));
        assert!(advertised.ends_with(";native.generation=1\x1b\\"));
        native.authorize_written_offer(offer.receipt.clone());
        let mut bytes = hello(&offer);
        bytes[38..40].copy_from_slice(&minor.to_le_bytes());
        client.write_all(&bytes).unwrap();
        if minor == 0 && !cfg!(target_os = "linux") {
            assert!(matches!(
                native.poll(now),
                Err(AuthorityError::Protocol("native unsupported dialect"))
            ));
            eof(client);
            continue;
        }
        native.poll(now).unwrap();
        native.poll(now).unwrap();
        let (setup, descriptor) = response(&client, 1);
        assert_eq!(opcode(&setup), if minor == 0 { 2 } else { 9 });
        assert_eq!(setup.len(), if minor == 0 { 48 } else { 128 });
        let producer = Producer::new(descriptor.unwrap());
        assert_eq!(producer.minor(), minor);
        publish(
            &producer,
            frame_literals::FRAME_INITIAL,
            1,
            1,
            if minor == 0 { 0 } else { 1 },
        );
        client
            .write_all(&control(literals::ACTIVATE, 1, 1, 2))
            .unwrap();
        native.poll(now).unwrap();
        native.poll(now).unwrap();
        native.poll(now).unwrap();
        let (active, _) = response(&client, 0);
        assert_eq!(opcode(&active), 4);
        assert_eq!(active[6..8], if minor == 0 { [0, 0] } else { [2, 0] });
        assert_eq!(
            native.scene().unwrap().frame().cells[0].glyph,
            Glyph::Scalar('?')
        );
        client
            .write_all(&control(literals::PING_IDLE, 1, 1, 3))
            .unwrap();
        native.poll(now).unwrap();
        native.poll(now).unwrap();
        let (pong, _) = response(&client, 0);
        assert_eq!(opcode(&pong), if minor == 0 { 8 } else { 15 });
        assert_eq!(pong.len(), if minor == 0 { 32 } else { 48 });
    }
}

#[cfg(target_os = "linux")]
fn legacy_active(now: Instant) -> (DesktopNative, UnixStream, Producer) {
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    let mut bytes = hello(&offer);
    bytes[38..40].fill(0);
    client.write_all(&bytes).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(opcode(&setup), 2);
    let producer = Producer::new(descriptor.unwrap());
    assert_eq!(producer.minor(), 0);
    publish(&producer, frame_literals::FRAME_INITIAL, 1, 1, 0);
    client
        .write_all(&control(literals::ACTIVATE, 1, 1, 2))
        .unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (active, _) = response(&client, 0);
    assert_eq!(active[4..8], [4, 0, 0, 0]);
    (native, client, producer)
}

#[test]
#[cfg(target_os = "linux")]
fn legacy_release_flushes_its_unchanged_reply_then_closes_the_generation() {
    let now = Instant::now();
    let (mut native, mut client, _producer) = legacy_active(now);
    let scene = Arc::downgrade(native.scene().unwrap());
    let mut release = control(literals::PING_IDLE, 1, 1, 3);
    release[4..6].copy_from_slice(&5u16.to_le_bytes());
    client.write_all(&release).unwrap();
    assert!(native.poll(now).unwrap());
    assert!(scene.upgrade().is_none());
    native.poll(now).unwrap();
    let (reply, _) = response(&client, 0);
    assert_eq!(opcode(&reply), 6);
    assert_eq!(reply.len(), 32);
    assert!(!native.available());
    eof(client);
    assert!(native
        .prepare_offer(NonZeroU32::new(2).unwrap(), now)
        .is_err());
}

#[test]
#[cfg(target_os = "linux")]
fn legacy_resize_pty_output_and_lease_expiry_end_the_original_generation() {
    let now = Instant::now();
    for reason in 0..3 {
        let (mut native, client, _producer) = legacy_active(now);
        let scene = Arc::downgrade(native.scene().unwrap());
        let removed = match reason {
            0 => native.pty_bytes(now),
            1 => native.resize(Geometry::new(2, 1, 8, 16).unwrap(), now),
            _ => native.poll(now + Duration::from_millis(500)),
        }
        .unwrap();
        assert!(removed);
        assert!(!native.available());
        assert!(scene.upgrade().is_none());
        eof(client);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn legacy_rejects_image_uploads_and_closes_the_received_rights() {
    let now = Instant::now();
    let (mut native, client, _producer) = legacy_active(now);
    let scene = Arc::downgrade(native.scene().unwrap());
    let (source, source_peer) = pair();
    let mut put = control(literals::PUT_IMAGE, 1, 1, 3);
    put[46..48].copy_from_slice(&1u16.to_le_bytes());
    put[64..72].copy_from_slice(&4u64.to_le_bytes());
    send_rights(&client, &put, &[source.as_fd()]);
    assert!(matches!(
        native.poll(now),
        Err(AuthorityError::Protocol("legacy native active opcode"))
    ));
    assert!(scene.upgrade().is_none());
    drop(source);
    eof(source_peer);
    eof(client);
}

#[test]
fn an_unused_offer_expires_once_without_periodic_idle_wakeups() {
    let now = Instant::now();
    let (mut native, _client) = owner(now);
    assert_eq!(native.next_wakeup(), None);
    native.poll(now).unwrap();
    assert_eq!(native.next_wakeup(), None);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(200)));
    native.poll(now + Duration::from_millis(200)).unwrap();
    assert_eq!(native.next_wakeup(), None);
    assert!(native.offer.is_none());
    native.authorize_written_offer(offer.receipt);
    assert_eq!(native.next_wakeup(), None);
    assert!(native.available());
}

#[test]
fn a_partial_record_keeps_its_first_byte_deadline_without_a_session_or_offer() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    client.write_all(&[56]).unwrap();
    native.poll(now + Duration::from_millis(25)).unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(225)));
    assert!(
        matches!(native.poll(now+Duration::from_millis(225)),Err(AuthorityError::Io(error)) if error.kind()==io::ErrorKind::TimedOut)
    );
    assert_eq!(native.next_wakeup(), None);
    eof(client);
}

#[test]
fn frame_polling_starts_only_when_a_frame_can_be_installed_and_stops_on_release() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let producer = setup(&mut native, &mut client, now);
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(200)));
    client
        .write_all(&control(literals::ACTIVATE, 1, 1, 2))
        .unwrap();
    native.poll(now).unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(20)));
    publish(&producer, frame_literals::FRAME_INITIAL, 1, 1, 1);
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (active, _) = response(&client, 0);
    assert_eq!(opcode(&active), 4);
    native.poll(now + Duration::from_millis(20)).unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(40)));
    let mut release = control(literals::PING_IDLE, 1, 1, 3);
    release[4..6].copy_from_slice(&5u16.to_le_bytes());
    client.write_all(&release).unwrap();
    native.poll(now + Duration::from_millis(30)).unwrap();
    native.poll(now + Duration::from_millis(30)).unwrap();
    let (released, _) = response(&client, 0);
    assert_eq!(opcode(&released), 6);
    assert_eq!(native.next_wakeup(), None);
    assert!(native.available());
}

#[test]
fn a_writer_receipt_schedules_an_already_received_hello_immediately() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    client.write_all(&hello(&offer)).unwrap();
    native.poll(now + Duration::from_millis(10)).unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(200)));
    native.authorize_written_offer(offer.receipt);
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(10)));
    native.poll(now + Duration::from_millis(15)).unwrap();
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(35)));
    native.poll(now + Duration::from_millis(15)).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!(opcode(&setup), 9);
    drop(descriptor);
    assert_eq!(native.next_wakeup(), Some(now + Duration::from_millis(210)));
}

#[test]
fn a_control_socket_hello_wakes_its_owner_without_any_pty_output() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    native
        .start_watcher(Arc::new(move || {
            let _ = wake_tx.send(());
        }))
        .unwrap();
    let offer = native
        .prepare_offer(NonZeroU32::new(1).unwrap(), now)
        .unwrap();
    native.authorize_written_offer(offer.receipt.clone());
    client.write_all(&hello(&offer)).unwrap();
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (setup, descriptor) = response(&client, 1);
    assert_eq!((opcode(&setup), value(&setup, 8)), (9, 1));
    assert!(descriptor.is_some());
    assert!(native.scene().is_none());
    native.close();
    eof(client);
}

#[test]
fn revocation_rearms_a_paused_reader_for_stale_control_without_a_timer() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    native
        .start_watcher(Arc::new(move || {
            let _ = wake_tx.send(());
        }))
        .unwrap();
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    native.poll(now).unwrap();
    assert!(!native.readiness.as_ref().unwrap().observed_socket_watched());
    assert_eq!(
        wake_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    );
    assert!(native.pty_bytes(now).unwrap());
    assert_eq!(native.next_wakeup(), None);
    assert!(native.readiness.as_ref().unwrap().observed_socket_watched());
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 4))
        .unwrap();
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    native.poll(now).unwrap();
    native.poll(now).unwrap();
    let (ended, descriptor) = response(&client, 0);
    assert!(descriptor.is_none());
    assert_eq!(
        (opcode(&ended), value(&ended, 16), value(&ended, 32)),
        (14, 4, 2)
    );
    native.close();
    eof(client);
}

#[test]
fn closing_the_owner_shuts_down_a_socket_retained_by_a_blocked_watcher() {
    let now = Instant::now();
    let (mut native, mut client) = owner(now);
    let _producer = active(&mut native, &mut client, now);
    let scene = Arc::downgrade(native.scene().unwrap());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let finish = std::sync::Mutex::new(finish_rx);
    native
        .start_watcher(Arc::new(move || {
            entered_tx.send(()).unwrap();
            finish
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }))
        .unwrap();
    client
        .write_all(&control(literals::PING_IDLE, 1, 1, 3))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    native.close();
    assert!(scene.upgrade().is_none());
    assert!(!native.available());
    assert_eq!(native.next_wakeup(), None);
    client.set_nonblocking(true).unwrap();
    match client.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        result => panic!("endpoint stayed open while the watcher retained it: {result:?}"),
    }
    finish_tx.send(()).unwrap();
}
