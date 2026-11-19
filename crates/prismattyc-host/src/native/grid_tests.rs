// SPDX-License-Identifier: MIT
use super::super::{
    frame::{decode_slot, FrameContext},
    wire12::{FrameSequence, MetricsEpoch, PtyEpoch, SLOT_BYTES},
};
use super::*;

fn frame() -> FrameSnapshot {
    fn u32_at(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    let text = format!("e{}", "\u{301}".repeat(20));
    let used = (272 + text.len() + 7) & !7;
    let mut bytes = vec![0; SLOT_BYTES as usize].into_boxed_slice();
    bytes[4..8].copy_from_slice(&[128, 0, 1, 0]);
    bytes[8] = 1;
    bytes[16] = 1;
    bytes[96] = 1;
    for (offset, value) in [
        (0, used as u32),
        (24, 4),
        (28, 2),
        (32, 8),
        (36, 128),
        (40, 1),
        (44, 256),
        (48, text.len() as u32),
        (52, 272),
        (60, used as u32),
        (64, 2),
        (68, 1),
        (72, 1),
    ] {
        u32_at(&mut bytes, offset, value);
    }
    for index in 0..8 {
        u32_at(&mut bytes, 128 + index * 16, u32::from(' '));
        u32_at(&mut bytes, 140 + index * 16, 0x1000);
    }
    u32_at(&mut bytes, 128, u32::from('A'));
    u32_at(&mut bytes, 140, 0x2000);
    u32_at(&mut bytes, 144, 0x00110000);
    u32_at(&mut bytes, 148, 0x800000ff);
    u32_at(&mut bytes, 152, 0x80ff0000);
    u32_at(&mut bytes, 156, 0x46f);
    u32_at(&mut bytes, 160, 0x80000000);
    u32_at(&mut bytes, 164, 0x400000e7);
    u32_at(&mut bytes, 172, 0x1502);
    u32_at(&mut bytes, 192, u32::from('B'));
    u32_at(&mut bytes, 260, text.len() as u32);
    bytes[264] = 1;
    bytes[272..272 + text.len()].copy_from_slice(text.as_bytes());
    decode_slot(
        bytes,
        FrameContext {
            sequence: FrameSequence::new(1).unwrap(),
            epoch: PtyEpoch::new(1).unwrap(),
            metrics: MetricsEpoch::new(1).unwrap(),
            columns: 4,
            rows: 2,
            activating: true,
        },
    )
    .unwrap()
}

#[test]
fn borrowed_cells_keep_declared_width_continuation_style_and_complete_cluster() {
    let frame = frame();
    let grid = NativeGrid::new(&frame);
    let lead = grid.cell(0, 0).unwrap();
    assert_eq!(lead.cell.width(), 2);
    assert_eq!(lead.text, Text::Scalar('A'));
    let continuation = grid.cell(1, 0).unwrap();
    assert_eq!(continuation.text, Text::Continuation);
    assert_eq!(
        (
            continuation.cell.foreground,
            continuation.cell.background,
            continuation.cell.meta
        ),
        (0x800000ff, 0x80ff0000, 0x46f)
    );
    let cluster = grid.cell(2, 0).unwrap();
    assert_eq!(cluster.text,Text::Cluster("e\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}"));
    assert_eq!(
        (cluster.cell.foreground, cluster.cell.meta),
        (0x400000e7, 0x1502)
    );
    let Text::Cluster(text) = cluster.text else {
        panic!("expected cluster")
    };
    assert!(std::ptr::eq(text.as_ptr(), frame.clusters[0].as_ptr()));
    assert!(std::ptr::eq(lead.cell, &frame.cells[0]));
}

#[test]
fn selected_text_uses_the_snapshot_order_without_continuations_or_trailing_spaces() {
    let frame = frame();
    let grid = NativeGrid::new(&frame);
    let expected = "Ae\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\nB";
    assert_eq!(grid.extract_text([0, 0], [3, 1]), expected);
    assert_eq!(grid.extract_text([3, 1], [0, 0]), expected);
    assert_eq!(grid.extract_text([0, 1], [usize::MAX, usize::MAX]), "B");
    assert_eq!(grid.extract_text([1, 0], [1, 0]), "");
    assert_eq!(grid.extract_text([0, 3], [3, 4]), "");
}

#[test]
fn geometry_and_cursor_come_from_the_same_snapshot_and_bounds_are_explicit() {
    let frame = frame();
    let grid = NativeGrid::new(&frame);
    assert_eq!(
        (grid.columns(), grid.rows(), grid.cursor()),
        (4, 2, Some([2, 1]))
    );
    assert_eq!(grid.cell(4, 0), None);
    assert_eq!(grid.cell(0, 2), None);
    assert_eq!(grid.cell(usize::MAX, usize::MAX), None);
}
