// SPDX-License-Identifier: MIT
use lunatui_core::{ClusterTable, Color, Grid, Rect, Style};
use lunatui_image::{Fit, ImageId, ImageRequest, ImageSource};
use lunatui_prismattyc::FrameEncoder;
use native_client_probe::{frame, ring::Producer};
use native_component_proof::{
    frame::{decode_slot, FrameContext, Glyph},
    ring::{RingContext, SharedRing},
    wire12::{FrameSequence, MetricsEpoch, PtyEpoch, SessionId},
};

fn grid() -> (Grid, ClusterTable) {
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
    (grid, clusters)
}

#[test]
fn actual_lunatui_encoder_publishes_wide_clusters_into_host_owned_ring() {
    let context = RingContext {
        session: SessionId::new(5).unwrap(),
        epoch: PtyEpoch::new(7).unwrap(),
        metrics: MetricsEpoch::new(9).unwrap(),
        columns: 3,
        rows: 2,
    };
    let mut host = SharedRing::create(context).unwrap();
    // SharedRing itself created and validated this fixed platform backing.
    let mut producer =
        unsafe { Producer::from_validated(host.descriptor().try_clone_to_owned().unwrap(), 5) }
            .unwrap();
    let (grid, clusters) = grid();
    let mut encoder = FrameEncoder::new();
    let bytes = frame::encode(&mut encoder, &grid, &clusters, 7, 9, 1, &[]).unwrap();
    assert_eq!(producer.publish(&bytes).unwrap(), 1);
    let copied = host.copy_next().unwrap().unwrap();
    assert_eq!(copied.snapshot.cells[0].glyph, Glyph::Scalar('界'));
    assert_eq!(copied.snapshot.cells[0].width(), 2);
    assert_eq!(copied.snapshot.cells[1].glyph, Glyph::Continuation);
    assert_eq!(copied.snapshot.cells[2].glyph, Glyph::Cluster(0));
    assert_eq!(copied.snapshot.clusters, ["e\u{301}"]);
    assert_eq!(host.acknowledge(copied.receipt).unwrap().get(), 1);
    assert!(host.copy_next().unwrap().is_none());
    for sequence in 2..=5 {
        let bytes = frame::encode(&mut encoder, &grid, &clusters, 7, 9, sequence, &[]).unwrap();
        producer.publish(&bytes).unwrap();
    }
    let sixth = frame::encode(&mut encoder, &grid, &clusters, 7, 9, 6, &[]).unwrap();
    assert_eq!(
        producer.publish(&sixth).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let copied = host.copy_next().unwrap().unwrap();
    assert_eq!(copied.snapshot.context.sequence.get(), 2);
    host.acknowledge(copied.receipt).unwrap();
    assert_eq!(producer.publish(&sixth).unwrap(), 6);
}

#[test]
fn real_image_geometry_encodes_exact_cover_crop_depth_opacity_and_metrics() {
    let source = ImageSource::rgba8(12, 4, vec![255; 12 * 4 * 4].into_boxed_slice()).unwrap();
    let request = ImageRequest::new(
        ImageId::new(23).unwrap(),
        4,
        &source,
        Rect::new(1, 1, 1, 1),
        Fit::Cover,
        -2,
        128,
    );
    let placement = frame::Placement::from_request(request, 17, [8, 16], 0).unwrap();
    let (grid, clusters) = grid();
    let bytes = frame::encode(
        &mut FrameEncoder::new(),
        &grid,
        &clusters,
        7,
        9,
        2,
        &[placement],
    )
    .unwrap();
    let mut slot = vec![0; 1_048_576];
    slot[..bytes.len()].copy_from_slice(&bytes);
    let decoded = decode_slot(
        slot.into_boxed_slice(),
        FrameContext {
            sequence: FrameSequence::new(2).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns: 3,
            rows: 2,
            activating: false,
        },
    )
    .unwrap();
    assert_eq!(decoded.placements.len(), 1);
    let p = decoded.placements[0];
    assert_eq!(p.id, 23);
    assert_eq!(p.depth, -2);
    assert_eq!(p.handle.get(), 17);
    assert_eq!(p.crop, [5, 0, 2, 4]);
    assert_eq!(p.destination, [8, 16]);
    assert_eq!(p.size, [8, 16]);
    assert_eq!(p.opacity, 128);
    assert_eq!(p.ordinal, 0);
}
