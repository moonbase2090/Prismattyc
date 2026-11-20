// SPDX-License-Identifier: MIT
//! A real PTY writer and inherited control socket meet at the collector boundary.

use super::*;
use crate::{native::wire12::literals, rich::RichHost};
use std::{
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    os::unix::net::UnixStream,
};

const CHILD: &str = "PRISMATTYC_TEST_NATIVE_FLOW";

fn query() -> Vec<u8> {
    prismattyc_protocol::encode_capability_query(prismattyc_protocol::CapabilityQuery {
        request_id: prismattyc_protocol::RequestId::new(77).unwrap(),
        max_version: prismattyc_protocol::ProtocolVersion::new(0, 4),
    })
    .unwrap()
}

fn signal(
    receiver: &mpsc::Receiver<std::io::Result<Vec<u8>>>,
    observed: &mut Vec<u8>,
    marker: &[u8],
) {
    while !observed.windows(marker.len()).any(|bytes| bytes == marker) {
        let bytes = receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|error| {
                panic!(
                    "{error:?} waiting for {:?}, observed {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(observed)
                )
            })
            .unwrap();
        assert!(
            !bytes.is_empty(),
            "child exited before {:?}: {}",
            String::from_utf8_lossy(marker),
            String::from_utf8_lossy(observed)
        );
        observed.extend(bytes);
        assert!(observed.len() <= 4096, "fixture output exceeded its bound");
    }
    let end = observed
        .windows(marker.len())
        .position(|bytes| bytes == marker)
        .unwrap()
        + marker.len();
    observed.drain(..end);
}

#[test]
fn fixture_markers_preserve_following_markers_and_split_suffixes() {
    let (sender, receiver) = mpsc::channel();
    sender
        .send(Ok(b"prefix\nHELLO\nFRAME\nPAR".to_vec()))
        .unwrap();
    sender.send(Ok(b"TIAL\n".to_vec())).unwrap();
    let mut pending = Vec::new();
    signal(&receiver, &mut pending, b"HELLO\n");
    assert_eq!(pending, b"FRAME\nPAR");
    signal(&receiver, &mut pending, b"FRAME\n");
    assert_eq!(pending, b"PAR");
    signal(&receiver, &mut pending, b"PARTIAL\n");
    assert!(pending.is_empty());
}

fn field<'a>(body: &'a str, name: &str) -> &'a str {
    let mut values = body.split(';').filter_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then_some(value)
    });
    let value = values.next().unwrap();
    assert!(values.next().is_none(), "duplicate {name}");
    value
}

fn receive(socket: &UnixStream, count: usize) -> (Vec<u8>, Vec<OwnedFd>) {
    let mut bytes = vec![0; 4096];
    let mut descriptors = Vec::new();
    let mut used = 0;
    let mut length = 4;
    while used < length {
        readable(socket);
        let received = prismattyc_process::fd::receive(
            socket.as_fd(),
            &mut bytes[used..length],
            count - descriptors.len(),
        )
        .unwrap();
        assert!(received.bytes > 0);
        used += received.bytes;
        descriptors.extend(received.descriptors);
        if used == 4 && length == 4 {
            length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert!((32..=4096).contains(&length));
        }
    }
    bytes.truncate(length);
    assert_eq!(descriptors.len(), count);
    (bytes, descriptors)
}

fn readable(socket: &UnixStream) {
    let mut descriptor = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2_000) }, 1);
    assert_eq!(descriptor.revents & libc::POLLNVAL, 0);
}

fn child(trailing: bool) {
    struct Raw(libc::termios);
    impl Drop for Raw {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) }, 0);
        }
    }
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(unsafe { libc::tcgetattr(0, &mut original) }, 0);
    let _restore = Raw(original);
    let mut raw = original;
    unsafe { libc::cfmakeraw(&mut raw) };
    assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) }, 0);
    let inherited = std::env::var("PRISMATTYC_NATIVE_FD")
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(inherited, 3);
    let flags = unsafe { libc::fcntl(inherited, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(inherited, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
        0
    );
    let owned = unsafe { BorrowedFd::borrow_raw(inherited) }
        .try_clone_to_owned()
        .unwrap();
    let mut socket = UnixStream::from(owned);
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        assert_eq!(
            unsafe { libc::getpeereid(inherited, &mut uid, &mut gid) },
            0
        );
        assert_eq!(uid, unsafe { libc::geteuid() });
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials = unsafe { std::mem::zeroed::<libc::ucred>() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    inherited,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            },
            0
        );
        assert_eq!(credentials.uid, unsafe { libc::geteuid() });
    }
    println!("NATIVE_FLOW_READY");
    std::io::stdout().flush().unwrap();
    let mut begin = [0; 6];
    std::io::stdin().read_exact(&mut begin).unwrap();
    assert_eq!(&begin, b"begin\n");
    let mut bytes = query();
    if trailing {
        bytes.push(b'x');
    }
    std::io::stdout().write_all(&bytes).unwrap();
    std::io::stdout().flush().unwrap();
    let mut reply = Vec::new();
    while !reply.ends_with(b"\x1b\\") {
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        reply.push(byte[0]);
        assert!(reply.len() <= 4096);
    }
    let body = std::str::from_utf8(&reply)
        .unwrap()
        .strip_prefix("\x1b_Prismattyc;cap;r;")
        .unwrap()
        .strip_suffix("\x1b\\")
        .unwrap();
    assert_eq!(field(body, "id"), "77");
    assert_eq!(field(body, "v"), "0.4");
    let features: Vec<_> = field(body, "features").split(',').collect();
    assert!(features.contains(&"native.images.v1"));
    assert!(!features.contains(&"native.alpha.v1"));
    assert_eq!(
        features.contains(&"native.grid.v1"),
        cfg!(target_os = "linux")
    );
    let epoch = field(body, "native.generation").parse::<u64>().unwrap();
    assert!(epoch > 0);
    let offer = field(body, "native.offer");
    assert_eq!(offer.len(), 32);
    let mut hello = literals::HELLO.to_vec();
    hello[24..32].copy_from_slice(&epoch.to_le_bytes());
    hello[32..36].copy_from_slice(&77u32.to_le_bytes());
    for (index, pair) in offer.as_bytes().chunks_exact(2).enumerate() {
        hello[40 + index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    socket.write_all(&hello).unwrap();
    println!("NATIVE_HELLO_SENT");
    std::io::stdout().flush().unwrap();
    if !trailing {
        let (setup, descriptors) = receive(&socket, 1);
        assert_eq!(u16::from_le_bytes(setup[4..6].try_into().unwrap()), 9);
        assert_eq!(u64::from_le_bytes(setup[24..32].try_into().unwrap()), epoch);
        let session = u64::from_le_bytes(setup[8..16].try_into().unwrap());
        assert!(session > 0);
        let producer =
            crate::native::scene::tests::Producer::new(descriptors.into_iter().next().unwrap());
        let mut frame = crate::native::frame::literals::FRAME_INITIAL.to_vec();
        frame.resize(384, 0);
        for (offset, value) in [
            (0, 384u32),
            (24, 8),
            (28, 2),
            (32, 16),
            (44, 384),
            (52, 384),
            (60, 384),
            (64, 2),
            (68, 1),
            (72, 1),
        ] {
            frame[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        frame[16..24].copy_from_slice(&epoch.to_le_bytes());
        frame[96..104].copy_from_slice(&setup[64..72]);
        for index in 0..16 {
            let (glyph, meta) = match index {
                0 => (u32::from('N'), 0x2000u32),
                1 => (0x11_0000, 0),
                _ => (u32::from(' '), 0x1000),
            };
            let at = 128 + index * 16;
            for (offset, value) in [(0, glyph), (4, 0x80ff_ffff), (8, 0x8000_0000), (12, meta)] {
                frame[at + offset..at + offset + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        producer.publish(1, &frame);
        let mut activate = literals::ACTIVATE.to_vec();
        activate[8..16].copy_from_slice(&session.to_le_bytes());
        activate[24..32].copy_from_slice(&epoch.to_le_bytes());
        socket.write_all(&activate).unwrap();
        println!("NATIVE_FRAME_SENT");
        std::io::stdout().flush().unwrap();
        let (active, _) = receive(&socket, 0);
        assert_eq!(u16::from_le_bytes(active[4..6].try_into().unwrap()), 4);
        println!("NATIVE_ACTIVE_SEEN");
        std::io::stdout().flush().unwrap();
        let mut release_signal = [0; 8];
        std::io::stdin().read_exact(&mut release_signal).unwrap();
        assert_eq!(&release_signal, b"release\n");
        let mut release = literals::PING_IDLE.to_vec();
        release[4..6].copy_from_slice(&5u16.to_le_bytes());
        release[8..16].copy_from_slice(&session.to_le_bytes());
        release[16..24].copy_from_slice(&3u64.to_le_bytes());
        release[24..32].copy_from_slice(&epoch.to_le_bytes());
        socket.write_all(&release).unwrap();
        println!("NATIVE_RELEASE_SENT");
        std::io::stdout().flush().unwrap();
        let (released, _) = receive(&socket, 0);
        assert_eq!(u16::from_le_bytes(released[4..6].try_into().unwrap()), 6);
        println!("NATIVE_RELEASE_SEEN");
        std::io::stdout().flush().unwrap();
    }
    let mut finish = [0; 7];
    std::io::stdin().read_exact(&mut finish).unwrap();
    assert_eq!(&finish, b"finish\n");
}

struct WrittenFlow<'a> {
    flow: NativeFlow<'a>,
    written: &'a mpsc::Receiver<()>,
}

impl RichHost for WrittenFlow<'_> {
    fn before_bytes(&mut self, emulator: &Emulator) -> bool {
        self.flow.before_bytes(emulator)
    }

    fn capability_reply(
        &mut self,
        query: prismattyc_protocol::CapabilityQuery,
        emulator: &mut Emulator,
        rich: &mut RichSession,
        to_child: &mpsc::SyncSender<ChildWrite>,
    ) -> bool {
        let handled = self.flow.capability_reply(query, emulator, rich, to_child);
        if handled {
            self.written.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        handled
    }
}

#[test]
fn a_completed_native_reply_cannot_authorize_an_offer_revoked_by_trailing_pty_bytes() {
    if let Ok(mode) = std::env::var(CHILD) {
        child(mode == "trailing");
        return;
    }
    let module = module_path!().split_once("::").unwrap().1;
    let name = format!("{module}::a_completed_native_reply_cannot_authorize_an_offer_revoked_by_trailing_pty_bytes");
    for trailing in [false, true] {
        let _clock = crate::native::service::PausedClockScope::enter();
        let mode = if trailing { "trailing" } else { "clean" };
        let args = vec![
            "-c".into(),
            format!("export {CHILD}={mode}; exec \"$1\" --exact \"$2\" --nocapture"),
            "native-fixture".into(),
            std::env::current_exe().unwrap().to_str().unwrap().into(),
            name.clone(),
        ];
        let (written_tx, written_rx) = mpsc::channel();
        let (native_tx, native_rx) = mpsc::channel();
        let wake: Wake = Arc::new(move || {
            if thread::current()
                .name()
                .is_some_and(|name| name.ends_with("-write"))
            {
                let _ = written_tx.send(());
            } else {
                let _ = native_tx.send(());
            }
        });
        let mut runtime = MuxRuntime::spawn_with_geom(
            "/bin/sh",
            &args,
            8,
            2,
            HostGeom::tight(8, 16),
            true,
            false,
            Some(wake),
        )
        .unwrap();
        let pane = runtime.focused_mut();
        assert!(pane.native.is_some());
        let receiver = pane.from_pty_rx.take().unwrap();
        let mut signals = Vec::new();
        signal(&receiver, &mut signals, b"NATIVE_FLOW_READY\n");
        if !trailing {
            pane.rich.apply_grant(rich::RichGrant {
                features: prismattyc_protocol::Feature::V1.into_iter().collect(),
                region_limit: 8,
            });
            let (overlay_tx, _overlay_rx) = mpsc::sync_channel(8);
            let overlay = prismattyc_protocol::encode_attach_cell_rect(
                &prismattyc_protocol::AttachCellRect {
                    id: 1,
                    row: 0,
                    col: 0,
                    rows: 1,
                    cols: 4,
                    text: "STAT".into(),
                },
            )
            .unwrap();
            let hud =
                prismattyc_protocol::encode_attach_viewport(&prismattyc_protocol::AttachViewport {
                    id: 2,
                    row: 1,
                    col: 0,
                    rows: 1,
                    cols: 3,
                    text: "HUD".into(),
                    runs: Vec::new(),
                })
                .unwrap();
            for bytes in [overlay, hud] {
                rich::process_rich_chunk(
                    &mut pane.emulator,
                    &mut pane.rich,
                    &overlay_tx,
                    &bytes,
                    &mut (),
                );
            }
            assert!(!pane.emulator.screen().alt_active());
            let overlays = rich::visible_overlays(&pane.emulator, &pane.rich);
            assert_eq!(overlays.cell_rect.len(), 1);
            assert_eq!(overlays.cell_rect[0].text, "STAT");
            assert_eq!(overlays.viewport.len(), 1);
            assert_eq!(overlays.viewport[0].text, "HUD");
        }
        let mut native_hid_classic_overlays = true;
        pane.to_child_tx
            .send(ChildWrite::bytes(b"begin\n".to_vec()))
            .unwrap();
        let mut expected = query();
        if trailing {
            expected.push(b'x');
        }
        let mut bytes = Vec::new();
        while bytes.len() < expected.len() {
            let part = receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert!(!part.is_empty());
            bytes.extend(part);
        }
        assert_eq!(bytes, expected);
        let now = Instant::now();
        let offered_epoch = pane.native.as_ref().unwrap().authority.epoch();
        rich::process_rich_chunk(
            &mut pane.emulator,
            &mut pane.rich,
            &pane.to_child_tx,
            &bytes,
            &mut WrittenFlow {
                flow: NativeFlow {
                    owner: pane.native.as_mut().unwrap(),
                    session: pane.session.as_ref().unwrap(),
                    columns: pane.cols,
                    rows: &mut pane.rows,
                    outer_rows: pane.outer_rows,
                    cell_width: pane.cell_w,
                    cell_height: pane.cell_h,
                    now,
                },
                written: &written_rx,
            },
        );
        signal(&receiver, &mut signals, b"NATIVE_HELLO_SENT\n");
        pane.drain_grants();
        let _ = pane.emulator.take_damage();
        pane.poll_native_at(now);
        if trailing {
            if let Some(owner) = pane.native.as_mut() {
                assert!(owner.authority.epoch().get() > offered_epoch.get());
                if owner.authority.available() {
                    assert!(!owner.authority.has_offer().unwrap());
                }
            }
            assert!(
                pane.native_scene().is_none(),
                "trailing bytes authorized the stale offer"
            );
            assert!(!pane.content().is_native());
            assert_eq!(pane.emulator.screen().view_cell(0, 0, 0).character, 'x');
        } else {
            assert!(pane.native.as_ref().unwrap().authority.available());
            pane.poll_native_at(now);
            signal(&receiver, &mut signals, b"NATIVE_FRAME_SENT\n");
            pane.native
                .as_ref()
                .unwrap()
                .authority
                .advance_clock(Duration::from_millis(20));
            loop {
                pane.poll_native_at(Instant::now());
                if pane.content().is_native() {
                    break;
                }
                assert!(
                    pane.native.is_some(),
                    "native owner failed during activation"
                );
                native_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            signal(&receiver, &mut signals, b"NATIVE_ACTIVE_SEEN\n");
            assert!(pane.content().is_native());
            native_hid_classic_overlays =
                rich::visible_overlays(&pane.emulator, &pane.rich).is_empty();
            native_hid_classic_overlays &= pane.rich.attachments_len_for_test() == 0;
            assert_eq!(pane.content().row_text(0), "N");
            assert_eq!(
                pane.content().cursor(),
                Some(prismattyc_core::Cursor { row: 1, column: 2 })
            );
            assert_eq!(
                pane.emulator.take_damage().dirty_cell_count(),
                0,
                "native installation touched ANSI cells"
            );
            assert_eq!(pane.emulator.screen().view_cell(0, 0, 0).character, ' ');
            let pane_id = runtime.focused_id();
            let installed_damage = runtime.take_pane_damage();
            assert_eq!(installed_damage[&pane_id].dirty_cell_count(), 16);
            assert_eq!(runtime.take_pane_damage()[&pane_id].dirty_cell_count(), 0);
            let last_output = runtime.focused().last_output_at;
            runtime.focused_mut().invalidate_native_paint();
            assert_eq!(runtime.take_pane_damage()[&pane_id].dirty_cell_count(), 16);
            assert_eq!(runtime.focused().last_output_at, last_output);
            let pane = runtime.focused_mut();
            pane.to_child_tx
                .send(ChildWrite::bytes(b"release\n".to_vec()))
                .unwrap();
            signal(&receiver, &mut signals, b"NATIVE_RELEASE_SENT\n");
            pane.poll_native_at(now);
            pane.poll_native_at(now);
            signal(&receiver, &mut signals, b"NATIVE_RELEASE_SEEN\n");
            assert!(pane.native.as_ref().unwrap().authority.available());
            assert!(pane.native_scene().is_none());
            assert!(!pane.content().is_native());
            native_hid_classic_overlays &= rich::visible_overlays(&pane.emulator, &pane.rich)
                .is_empty()
                && pane.rich.attachments_len_for_test() == 0;
            assert_eq!(pane.content().row_text(0), "");
            assert_eq!(runtime.take_pane_damage()[&pane_id].dirty_cell_count(), 16);
            assert_eq!(runtime.take_pane_damage()[&pane_id].dirty_cell_count(), 0);
        }
        let pane = runtime.focused_mut();
        pane.to_child_tx
            .send(ChildWrite::bytes(b"finish\n".to_vec()))
            .unwrap();
        assert!(pane.session.as_mut().unwrap().wait().unwrap().success());
        assert!(
            native_hid_classic_overlays,
            "Classic primary overlays painted over the installed Native scene"
        );
    }
}

#[test]
fn a_closed_reply_queue_revokes_the_unused_offer_immediately() {
    let _clock = crate::native::service::PausedClockScope::enter();
    let mut runtime = MuxRuntime::spawn_with_geom(
        "/bin/sh",
        &["-c".into(), "read unused".into()],
        8,
        2,
        HostGeom::tight(8, 16),
        true,
        false,
        Some(Arc::new(|| {})),
    )
    .unwrap();
    let pane = runtime.focused_mut();
    let before = pane.native.as_ref().unwrap().authority.epoch().get();
    let (closed, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    let mut flow = NativeFlow {
        owner: pane.native.as_mut().unwrap(),
        session: pane.session.as_ref().unwrap(),
        columns: pane.cols,
        rows: &mut pane.rows,
        outer_rows: pane.outer_rows,
        cell_width: pane.cell_w,
        cell_height: pane.cell_h,
        now: Instant::now(),
    };
    assert!(flow.capability_reply(
        prismattyc_protocol::CapabilityQuery {
            request_id: prismattyc_protocol::RequestId::new(77).unwrap(),
            max_version: prismattyc_protocol::ProtocolVersion::new(0, 4),
        },
        &mut pane.emulator,
        &mut pane.rich,
        &closed,
    ));
    assert!(flow.owner.authority.available());
    assert_eq!(flow.owner.authority.epoch().get(), before + 1);
    assert_eq!(flow.owner.authority.next_wakeup(), None);
    assert!(flow.owner.authority.scene().is_none());
}

#[test]
fn a_native_reply_prefix_followed_by_a_real_socket_failure_cannot_issue_a_receipt() {
    struct PrefixSocket {
        socket: UnixStream,
        first: bool,
        peer_closed: mpsc::Receiver<()>,
    }
    impl Write for PrefixSocket {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let bytes = if self.first {
                &bytes[..bytes.len().min(3)]
            } else {
                bytes
            };
            let count = unsafe {
                libc::send(
                    self.socket.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if self.first {
                self.first = false;
                self.peer_closed
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            Ok(count as usize)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.socket.flush()
        }
    }
    let (host, client) = prismattyc_process::fd::socket_pair().unwrap();
    let now = Instant::now();
    let mut authority = crate::native::authority::DesktopNative::new(
        host.into(),
        crate::native::authority::Geometry::new(1, 1, 8, 16).unwrap(),
        now,
    )
    .unwrap();
    let offer = authority
        .prepare_offer(std::num::NonZeroU32::new(77).unwrap(), now)
        .unwrap();
    let (sink, peer) = prismattyc_process::fd::socket_pair().unwrap();
    let (closed_tx, closed_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let mut socket = UnixStream::from(peer);
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut prefix = [0; 3];
        socket.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"\x1b_P");
        drop(socket);
        closed_tx.send(()).unwrap();
    });
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(ChildWrite::capability(
        crate::native::profile::encode_reply(&offer).unwrap(),
        CapabilityGrant::Native(offer.receipt.clone()),
    ))
    .unwrap();
    drop(tx);
    let writer = crate::paste_job::PtyWriter::new(rx);
    let (grant_tx, grant_rx) = mpsc::channel();
    writer.run(
        &mut PrefixSocket {
            socket: sink.into(),
            first: true,
            peer_closed: closed_rx,
        },
        |grant| {
            grant_tx.send(grant).unwrap();
        },
    );
    drop(grant_tx);
    peer.join().unwrap();
    assert!(matches!(
        grant_rx.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    let mut hello = literals::HELLO.to_vec();
    hello[24..32].copy_from_slice(&offer.epoch.get().to_le_bytes());
    hello[32..36].copy_from_slice(&offer.capability_id.get().to_le_bytes());
    hello[40..56].copy_from_slice(&offer.value);
    let mut client = UnixStream::from(client);
    client.write_all(&hello).unwrap();
    assert!(!authority.poll(now).unwrap());
    let mut byte = [0];
    assert_eq!(
        prismattyc_process::fd::receive(client.as_fd(), &mut byte, 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(authority.expire(now + Duration::from_millis(200)).is_err());
    assert!(!authority.available());
    assert_eq!(
        prismattyc_process::fd::receive(client.as_fd(), &mut byte, 0)
            .unwrap()
            .bytes,
        0
    );
}
