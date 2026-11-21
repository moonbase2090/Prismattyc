// SPDX-License-Identifier: MIT
#[cfg(target_os = "linux")]
mod support;
use native_client_probe::{control::Control, session::record};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    time::Instant,
};

#[test]
fn legacy_control_accepts_frozen_pong_and_rejects_image_active_flags() {
    let now = Instant::now();
    let (left, right) = prismattyc_process::fd::socket_pair().unwrap();
    let mut peer = UnixStream::from(left);
    let mut client = Control::legacy(right.into()).unwrap();
    client.begin(record(7, 32, 5, 7, 3), None, now).unwrap();
    assert!(client.step(now).unwrap().is_none());
    let mut bytes = [0; 32];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes[4..8], &[7, 0, 0, 0]);
    peer.write_all(&record(8, 32, 5, 7, 3)).unwrap();
    assert_eq!(
        client.step(now).unwrap().unwrap().bytes,
        record(8, 32, 5, 7, 3)
    );
    let mut activate = record(3, 40, 5, 7, 4);
    activate[32..40].copy_from_slice(&1u64.to_le_bytes());
    client.begin(activate, None, now).unwrap();
    assert!(client.step(now).unwrap().is_none());
    let mut active = record(4, 40, 5, 7, 4);
    active[6..8].copy_from_slice(&2u16.to_le_bytes());
    active[32..40].copy_from_slice(&1u64.to_le_bytes());
    peer.write_all(&active).unwrap();
    assert!(client.step(now).is_err());
    let mut request = [0; 40];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn existing_lunatui_grid_codec_activates_real_host_minor_zero_and_releases() {
    use lunatui_core::{ClusterTable, Color, Grid, Rect, Style};
    use lunatui_prismattyc::FrameEncoder;
    use native_client_probe::ring::Producer;
    use native_component_proof::{authority::Geometry, frame::Glyph, service::NativeService};
    use std::num::NonZeroU32;
    let now = Instant::now();
    let (left, right) = prismattyc_process::fd::socket_pair().unwrap();
    let mut host =
        NativeService::new(left.into(), Geometry::new(3, 2, 8, 16).unwrap(), now).unwrap();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    host.set_wake(std::sync::Arc::new(move || {
        let _ = wake_tx.send(());
    }))
    .unwrap();
    let offer = host
        .prepare_offer(NonZeroU32::new(77).unwrap(), now)
        .unwrap();
    host.authorize_written_offer(offer.receipt);
    let mut client = Control::legacy(right.into()).unwrap();
    let mut encoder = FrameEncoder::new();
    assert_eq!(encoder.hello_ring_minor(), 0);
    let mut hello = record(1, 56, 0, 1, 1);
    hello[32..36].copy_from_slice(&77u32.to_le_bytes());
    hello[36..38].copy_from_slice(&1u16.to_le_bytes());
    hello[40..56].copy_from_slice(&offer.value);
    client.begin(hello, None, Instant::now()).unwrap();
    let setup = support::finish(&mut client);
    assert_eq!(&setup.bytes[4..8], &[2, 0, 0, 0]);
    assert_eq!(
        &setup.bytes[32..48],
        &[0, 16, 64, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 16, 0]
    );
    let session = u64::from_le_bytes(setup.bytes[8..16].try_into().unwrap());
    let mut producer =
        Producer::from_legacy_setup(setup.descriptor.unwrap(), &setup.bytes).unwrap();
    let mut grid = Grid::new(3, 2);
    let mut clusters = ClusterTable::new();
    grid.fill(
        Rect::new(0, 0, 3, 2),
        Style::new()
            .fg(Color::Rgb(255, 255, 255))
            .bg(Color::Rgb(0, 0, 0)),
        ' ',
        &mut clusters,
    );
    grid.put_str(
        0,
        0,
        Style::new()
            .fg(Color::Rgb(255, 255, 255))
            .bg(Color::Rgb(0, 0, 0)),
        "界e\u{301}",
        &mut clusters,
    );
    producer
        .publish(encoder.encode(&grid, &clusters, 1, 1, None).unwrap())
        .unwrap();
    let mut activate = record(3, 40, session, 1, 2);
    activate[32..40].copy_from_slice(&1u64.to_le_bytes());
    client.begin(activate, None, Instant::now()).unwrap();
    assert!(client.step(Instant::now()).unwrap().is_none());
    support::install(&mut host, &wake_rx, now);
    let active = support::finish(&mut client);
    assert_eq!(&active.bytes[4..8], &[4, 0, 0, 0]);
    let frame = host.scene().unwrap().frame();
    assert_eq!(frame.cells[0].glyph, Glyph::Scalar('界'));
    assert_eq!(frame.cells[0].width(), 2);
    assert_eq!(frame.cells[1].glyph, Glyph::Continuation);
    assert_eq!(frame.cells[2].glyph, Glyph::Cluster(0));
    assert_eq!(frame.clusters, ["e\u{301}"]);
    for (opcode, expected, sequence) in [(7, 8, 3), (5, 6, 4)] {
        client
            .begin(
                record(opcode, 32, session, 1, sequence),
                None,
                Instant::now(),
            )
            .unwrap();
        let reply = support::finish(&mut client);
        assert_eq!(reply.bytes, record(expected, 32, session, 1, sequence));
    }
    assert!(host.scene().is_none());
    assert_eq!(host.epoch().get(), 2);
}
