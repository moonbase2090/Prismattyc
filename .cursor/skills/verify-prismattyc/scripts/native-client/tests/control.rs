// SPDX-License-Identifier: MIT
use lunatui_core::{ClusterTable, Grid};
use lunatui_prismattyc::FrameEncoder;
use native_client_probe::{control::Control, frame, ring::Producer};
use native_component_proof::{authority::Geometry, service::NativeService};
mod support;
use std::{
    io::{Read, Write},
    num::NonZeroU32,
    os::unix::net::UnixStream,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

fn request(opcode: u16, length: usize, session: u64, epoch: u64, sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[4..6].copy_from_slice(&opcode.to_le_bytes());
    for (offset, value) in [(8, session), (16, sequence), (24, epoch)] {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[test]
fn client_and_actual_host_activate_expire_release_and_rearm_at_a_new_epoch() {
    let mut now = Instant::now();
    let (left, right) = prismattyc_process::fd::socket_pair().unwrap();
    let mut host =
        NativeService::new(left.into(), Geometry::new(3, 2, 8, 16).unwrap(), now).unwrap();
    let (wake_tx, wake_rx) = mpsc::channel();
    host.set_wake(Arc::new(move || {
        let _ = wake_tx.send(());
    }))
    .unwrap();
    let mut client = Control::new(right.into()).unwrap();
    for epoch in [1u64, 2] {
        let offer = host
            .prepare_offer(NonZeroU32::new(77).unwrap(), now)
            .unwrap();
        assert_eq!(offer.epoch.get(), epoch);
        host.authorize_written_offer(offer.receipt);
        let mut hello = request(1, 56, 0, epoch, 1);
        hello[32..36].copy_from_slice(&77u32.to_le_bytes());
        hello[36..38].copy_from_slice(&1u16.to_le_bytes());
        hello[38..40].copy_from_slice(&2u16.to_le_bytes());
        hello[40..56].copy_from_slice(&offer.value);
        client.begin(hello, None, Instant::now()).unwrap();
        let setup = support::finish(&mut client);
        assert_eq!(&setup.bytes[4..8], &[9, 0, 0, 0]);
        let session = u64::from_le_bytes(setup.bytes[8..16].try_into().unwrap());
        let metrics = u64::from_le_bytes(setup.bytes[64..72].try_into().unwrap());
        let mut producer = Producer::from_setup(setup.descriptor.unwrap(), &setup.bytes).unwrap();
        let bytes = frame::encode(
            &mut FrameEncoder::new(),
            &Grid::new(3, 2),
            &ClusterTable::new(),
            epoch,
            metrics,
            1,
            &[],
        )
        .unwrap();
        producer.publish(&bytes).unwrap();
        let mut activate = request(3, 40, session, epoch, 2);
        activate[32..40].copy_from_slice(&1u64.to_le_bytes());
        client.begin(activate, None, Instant::now()).unwrap();
        assert!(client.step(Instant::now()).unwrap().is_none());
        support::install(&mut host, &wake_rx, now);
        let active = support::finish(&mut client);
        assert_eq!(&active.bytes[4..8], &[4, 0, 2, 0]);
        assert_eq!(host.scene().unwrap().frame().context.sequence.get(), 1);
        client
            .begin(request(7, 32, session, epoch, 3), None, Instant::now())
            .unwrap();
        let pong = support::finish(&mut client);
        assert_eq!(&pong.bytes[32..48], &[0; 16]);
        if epoch == 1 {
            while host.scene().is_some() {
                wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                now = Instant::now();
                host.poll(now).unwrap();
            }
            assert!(host.scene().is_none());
            client
                .begin(request(7, 32, session, epoch, 4), None, Instant::now())
                .unwrap();
            let ended = support::finish(&mut client);
            assert_eq!(&ended.bytes[4..8], &[14, 0, 0, 0]);
            assert_eq!(&ended.bytes[32..40], &2u64.to_le_bytes());
            assert_eq!(&ended.bytes[40..48], &[3, 0, 0, 0, 0, 0, 0, 0]);
            continue;
        }
        client
            .begin(request(5, 32, session, epoch, 4), None, Instant::now())
            .unwrap();
        let released = support::finish(&mut client);
        assert_eq!(&released.bytes[4..8], &[6, 0, 0, 0]);
        assert!(host.scene().is_none());
        assert_eq!(host.epoch().get(), epoch + 1);
    }
}

#[test]
fn partial_response_never_renews_the_absolute_operation_deadline() {
    let now = Instant::now();
    let (left, right) = prismattyc_process::fd::socket_pair().unwrap();
    let mut peer = UnixStream::from(left);
    let mut client = Control::new(right.into()).unwrap();
    client.begin(request(7, 32, 1, 1, 3), None, now).unwrap();
    assert!(client.step(now).unwrap().is_none());
    peer.write_all(&48u32.to_le_bytes()).unwrap();
    assert!(client
        .step(now + Duration::from_millis(199))
        .unwrap()
        .is_none());
    let error = client.step(now + Duration::from_millis(200)).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let mut request_bytes = [0; 32];
    peer.read_exact(&mut request_bytes).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert!(client.begin(request(7, 32, 1, 1, 4), None, now).is_err());
}

#[test]
fn a_reply_stolen_or_replaced_by_another_client_fails_identity_without_recovery_scan() {
    let now = Instant::now();
    let (left, right) = prismattyc_process::fd::socket_pair().unwrap();
    let mut peer = UnixStream::from(left);
    let mut client = Control::new(right.into()).unwrap();
    client.begin(request(7, 32, 3, 7, 11), None, now).unwrap();
    assert!(client.step(now).unwrap().is_none());
    peer.write_all(&request(15, 48, 4, 7, 11)).unwrap();
    assert!(client.step(now).is_err());
    let mut request_bytes = [0; 32];
    peer.read_exact(&mut request_bytes).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}
