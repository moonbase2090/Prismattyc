// SPDX-License-Identifier: MIT
//! ABI 1.2 snapshots decoded from a complete host-owned slot copy.

use super::wire12::{FrameSequence, MetricsEpoch, PtyEpoch, ResourceHandle, SLOT_BYTES};
use std::{collections::HashSet, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameContext {
    pub sequence: FrameSequence,
    pub epoch: PtyEpoch,
    pub metrics: MetricsEpoch,
    pub columns: u32,
    pub rows: u32,
    pub activating: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    Length,
    Header,
    Geometry,
    Sections,
    Padding,
    Cursor,
    Cluster,
    Cell,
    Damage,
    Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Glyph {
    Scalar(char),
    Continuation,
    Cluster(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub glyph: Glyph,
    pub foreground: u32,
    pub background: u32,
    pub meta: u16,
}

impl Cell {
    pub fn width(self) -> u16 {
        (self.meta >> 12) & 3
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Damage {
    pub row: u32,
    pub column: u32,
    pub cells: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    pub id: u32,
    pub depth: i32,
    pub handle: ResourceHandle,
    pub crop: [u32; 4],
    pub destination: [i32; 2],
    pub size: [u32; 2],
    pub opacity: u8,
    pub ordinal: u32,
}

impl Placement {
    /// Resource ownership and READY status are resolved by the session registry.
    pub fn crop_fits(self, width: u32, height: u32) -> bool {
        u64::from(self.crop[0]) + u64::from(self.crop[2]) <= u64::from(width)
            && u64::from(self.crop[1]) + u64::from(self.crop[3]) <= u64::from(height)
    }
}

/// Structurally validated data; resource handles still require session resolution.
#[derive(Debug, Eq, PartialEq)]
pub struct FrameSnapshot {
    pub context: FrameContext,
    pub full: bool,
    pub cursor: Option<[u32; 2]>,
    pub cells: Vec<Cell>,
    pub clusters: Vec<String>,
    pub damage: Vec<Damage>,
    pub placements: Vec<Placement>,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

struct Sections<'a> {
    bytes: &'a [u8],
    end: usize,
}

impl Sections<'_> {
    fn take(
        &mut self,
        offset: u32,
        count: usize,
        stride: usize,
    ) -> Result<Range<usize>, FrameError> {
        let start = self.end.checked_add(7).ok_or(FrameError::Sections)? & !7;
        let end = count
            .checked_mul(stride)
            .and_then(|size| start.checked_add(size))
            .filter(|end| *end <= self.bytes.len())
            .ok_or(FrameError::Sections)?;
        if offset as usize != start {
            return Err(FrameError::Sections);
        }
        if self.bytes[self.end..start].iter().any(|byte| *byte != 0) {
            return Err(FrameError::Padding);
        }
        self.end = end;
        Ok(start..end)
    }
}

/// Consumes an owned copy, never a slice into a peer-mutable shared mapping.
pub fn decode_slot(slot: Box<[u8]>, context: FrameContext) -> Result<FrameSnapshot, FrameError> {
    decode_profile(slot, context, false)
}

/// Legacy header validation stays with its frozen codec; cells remain host-owned.
pub fn decode_legacy_slot(
    slot: Box<[u8]>,
    context: FrameContext,
) -> Result<FrameSnapshot, FrameError> {
    if slot.len() != SLOT_BYTES as usize {
        return Err(FrameError::Length);
    }
    prismattyc_protocol::native_grid::FrameHeader::decode(&slot[..128])
        .map_err(|_| FrameError::Header)?;
    decode_profile(slot, context, true)
}

fn decode_profile(
    slot: Box<[u8]>,
    context: FrameContext,
    legacy: bool,
) -> Result<FrameSnapshot, FrameError> {
    if slot.len() != SLOT_BYTES as usize {
        return Err(FrameError::Length);
    }
    let used = u32_at(&slot, 0) as usize;
    if !(128..=slot.len()).contains(&used) || !legacy && used & 7 != 0 {
        return Err(FrameError::Length);
    }
    let flags = u16_at(&slot, 6);
    if u16_at(&slot, 4) != 128
        || flags & !1 != 0
        || u64_at(&slot, 8) != context.sequence.get()
        || u64_at(&slot, 16) != context.epoch.get()
        || !legacy && u64_at(&slot, 96) != context.metrics.get()
        || slot[92..96] != [0; 4]
        || slot[104..128] != [0; 24]
        || context.activating && flags != 1
        // Cell alpha is not negotiated by the first host implementation.
        || u32_at(&slot, 76) != 0
        || u32_at(&slot, 80) != 0
    {
        return Err(FrameError::Header);
    }
    let columns = u32_at(&slot, 24);
    let rows = u32_at(&slot, 28);
    let cell_count = u32_at(&slot, 32);
    if !(1..=4096).contains(&columns)
        || !(1..=4096).contains(&rows)
        || columns != context.columns
        || rows != context.rows
        || columns.checked_mul(rows) != Some(cell_count)
        || cell_count > 32_768
    {
        return Err(FrameError::Geometry);
    }
    let cluster_count = u32_at(&slot, 40);
    let blob_count = u32_at(&slot, 48);
    let damage_count = u32_at(&slot, 56);
    let placement_count = u32_at(&slot, 84);
    if cluster_count > cell_count
        || blob_count > 262_144
        || damage_count > rows * 2
        || damage_count > 8192
        || flags == 1 && damage_count != 0
        || placement_count > 256
        || context.activating && placement_count != 0
    {
        return Err(FrameError::Sections);
    }
    let cursor = match u32_at(&slot, 72) {
        0 if slot[64..72] == [0; 8] => None,
        1 if u32_at(&slot, 64) < columns && u32_at(&slot, 68) < rows => {
            Some([u32_at(&slot, 64), u32_at(&slot, 68)])
        }
        _ => return Err(FrameError::Cursor),
    };
    let mut sections = Sections {
        bytes: &slot[..used],
        end: 128,
    };
    let cell_range = sections.take(u32_at(&slot, 36), cell_count as usize, 16)?;
    let cluster_range = sections.take(u32_at(&slot, 44), cluster_count as usize, 16)?;
    let blob_range = sections.take(u32_at(&slot, 52), blob_count as usize, 1)?;
    let damage_range = sections.take(u32_at(&slot, 60), damage_count as usize, 16)?;
    let placement_range = if placement_count == 0 {
        if u32_at(&slot, 88) != 0 {
            return Err(FrameError::Sections);
        }
        sections.end..sections.end
    } else {
        sections.take(u32_at(&slot, 88), placement_count as usize, 64)?
    };
    let final_end = sections.end.checked_add(7).ok_or(FrameError::Sections)? & !7;
    if final_end > used || !legacy && final_end != used {
        return Err(FrameError::Sections);
    }
    if slot[sections.end..].iter().any(|byte| *byte != 0) {
        return Err(FrameError::Padding);
    }

    let blob = &slot[blob_range];
    let mut cluster_text = Vec::with_capacity(cluster_count as usize);
    let mut packed_end = 0;
    for entry in slot[cluster_range].chunks_exact(16) {
        let start = u32_at(entry, 0) as usize;
        let length = u32_at(entry, 4) as usize;
        let end = start.checked_add(length).ok_or(FrameError::Cluster)?;
        if start != packed_end
            || !(1..=1024).contains(&length)
            || !matches!(entry[8], 1 | 2)
            || entry[9..] != [0; 7]
        {
            return Err(FrameError::Cluster);
        }
        let text = blob
            .get(start..end)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or(FrameError::Cluster)?;
        if text.chars().any(char::is_control) || text.graphemes(true).count() != 1 {
            return Err(FrameError::Cluster);
        }
        cluster_text.push((text, entry[8]));
        packed_end = end;
    }
    if packed_end != blob.len() {
        return Err(FrameError::Cluster);
    }
    let mut cells = Vec::with_capacity(cell_count as usize);
    let mut clusters = Vec::new();
    let mut remapping = vec![None; cluster_text.len()];
    for (index, entry) in slot[cell_range].chunks_exact(16).enumerate() {
        let foreground = u32_at(entry, 4);
        let background = u32_at(entry, 8);
        let meta = u32_at(entry, 12);
        let width = (meta >> 12) & 3;
        if !valid_color(foreground)
            || !valid_color(background)
            || meta & 0xffff_c800 != 0
            || (meta >> 3) & 7 > 5
            || width == 3
        {
            return Err(FrameError::Cell);
        }
        let raw = u32_at(entry, 0);
        let glyph = if raw & 0x8000_0000 != 0 {
            let id = (raw & 0x7fff_ffff) as usize;
            let &(text, cluster_width) = cluster_text.get(id).ok_or(FrameError::Cell)?;
            if u32::from(cluster_width) != width {
                return Err(FrameError::Cell);
            }
            let mapped = *remapping[id].get_or_insert_with(|| {
                let mapped = clusters.len() as u32;
                clusters.push(text.to_owned());
                mapped
            });
            Glyph::Cluster(mapped)
        } else if raw == 0x0011_0000 {
            if width != 0
                || index % columns as usize == 0
                || cells.last().is_none_or(|cell: &Cell| cell.width() != 2)
            {
                return Err(FrameError::Cell);
            }
            Glyph::Continuation
        } else {
            let scalar = char::from_u32(raw)
                .filter(|scalar| !scalar.is_control())
                .ok_or(FrameError::Cell)?;
            if !matches!(width, 1 | 2) {
                return Err(FrameError::Cell);
            }
            Glyph::Scalar(scalar)
        };
        if index % columns as usize != 0
            && cells.last().is_some_and(|cell: &Cell| cell.width() == 2)
            && glyph != Glyph::Continuation
        {
            return Err(FrameError::Cell);
        }
        if width == 2 && index % columns as usize + 1 == columns as usize {
            return Err(FrameError::Cell);
        }
        cells.push(Cell {
            glyph,
            foreground,
            background,
            meta: meta as u16,
        });
    }
    let mut damage = Vec::with_capacity(damage_count as usize);
    let mut previous: Option<Damage> = None;
    for entry in slot[damage_range].chunks_exact(16) {
        let item = Damage {
            row: u32_at(entry, 0),
            column: u32_at(entry, 4),
            cells: u32_at(entry, 8),
        };
        let end = item
            .column
            .checked_add(item.cells)
            .ok_or(FrameError::Damage)?;
        if item.row >= rows
            || item.cells == 0
            || end > columns
            || u32_at(entry, 12) != 0
            || previous.is_some_and(|prev| {
                item.row < prev.row
                    || item.row == prev.row && item.column < prev.column + prev.cells
            })
            || cells[(item.row * columns + item.column) as usize].width() == 0
            || cells[(item.row * columns + end - 1) as usize].width() == 2
        {
            return Err(FrameError::Damage);
        }
        previous = Some(item);
        damage.push(item);
    }
    let mut placements = Vec::with_capacity(placement_count as usize);
    let mut ids = HashSet::with_capacity(placement_count as usize);
    for (ordinal, entry) in slot[placement_range].chunks_exact(64).enumerate() {
        let id = u32_at(entry, 0);
        let crop = [
            u32_at(entry, 16),
            u32_at(entry, 20),
            u32_at(entry, 24),
            u32_at(entry, 28),
        ];
        let destination = [u32_at(entry, 32) as i32, u32_at(entry, 36) as i32];
        let size = [u32_at(entry, 40), u32_at(entry, 44)];
        if id == 0
            || !ids.insert(id)
            || crop[2] == 0
            || crop[3] == 0
            || entry[49..52] != [0; 3]
            || entry[56..64] != [0; 8]
            || u32_at(entry, 52) != ordinal as u32
            || destination.into_iter().zip(size).any(|(origin, size)| {
                let origin = i64::from(origin);
                size == 0
                    || size > 1_073_741_824
                    || !(-1_073_741_824..=1_073_741_824).contains(&origin)
                    || !(-1_073_741_824..=1_073_741_824).contains(&(origin + i64::from(size)))
            })
        {
            return Err(FrameError::Placement);
        }
        placements.push(Placement {
            id,
            depth: u32_at(entry, 4) as i32,
            handle: ResourceHandle::new(u64_at(entry, 8)).ok_or(FrameError::Placement)?,
            crop,
            destination,
            size,
            opacity: entry[48],
            ordinal: ordinal as u32,
        });
    }
    Ok(FrameSnapshot {
        context,
        full: flags == 1,
        cursor,
        cells,
        clusters,
        damage,
        placements,
    })
}

fn valid_color(color: u32) -> bool {
    match color >> 30 {
        0 => color == 0,
        1 => color & 0x3fff_ff00 == 0,
        2 => color & 0x3f00_0000 == 0,
        _ => false,
    }
}

#[cfg(test)]
#[path = "frame_literals.rs"]
pub(crate) mod literals;

#[cfg(test)]
mod tests {
    use super::*;

    fn context(sequence: u64, columns: u32) -> FrameContext {
        FrameContext {
            sequence: FrameSequence::new(sequence).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns,
            rows: 1,
            activating: sequence == 1,
        }
    }
    fn slot(literal: &[u8]) -> Box<[u8]> {
        let mut bytes = vec![0; SLOT_BYTES as usize];
        bytes[..literal.len()].copy_from_slice(literal);
        bytes.into_boxed_slice()
    }
    fn word(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn two_cells() -> Box<[u8]> {
        let mut bytes = slot(literals::FRAME_INITIAL);
        word(&mut bytes, 0, 160);
        word(&mut bytes, 24, 2);
        word(&mut bytes, 32, 2);
        for offset in [44, 52, 60] {
            word(&mut bytes, offset, 160);
        }
        bytes.copy_within(128..144, 144);
        bytes
    }
    fn cluster_frame(text: &[u8], width: u8) -> Box<[u8]> {
        let mut bytes = slot(literals::FRAME_INITIAL);
        word(&mut bytes, 40, 1);
        word(&mut bytes, 48, text.len() as u32);
        word(&mut bytes, 52, 160);
        let end = (160 + text.len() + 7) & !7;
        word(&mut bytes, 0, end as u32);
        word(&mut bytes, 60, end as u32);
        word(&mut bytes, 128, 0x8000_0000);
        word(&mut bytes, 148, text.len() as u32);
        bytes[152] = width;
        bytes[160..160 + text.len()].copy_from_slice(text);
        bytes
    }

    #[test]
    fn approved_frame_literals_decode_into_owned_snapshots() {
        let initial = decode_slot(slot(literals::FRAME_INITIAL), context(1, 1)).unwrap();
        assert_eq!(
            initial.cells,
            vec![Cell {
                glyph: Glyph::Scalar('?'),
                foreground: 0x80ff_ffff,
                background: 0x8000_0000,
                meta: 0x1000,
            }]
        );
        assert!(initial.full);
        assert_eq!(initial.cursor, None);
        assert_eq!(initial.clusters, Vec::<String>::new());
        assert_eq!(initial.damage, Vec::<Damage>::new());
        assert_eq!(initial.placements, Vec::<Placement>::new());
        let placed = decode_slot(slot(literals::FRAME_PLACEMENT), context(2, 1)).unwrap();
        assert_eq!(placed.cells[0].glyph, Glyph::Scalar(' '));
        assert_eq!(
            placed.placements,
            vec![Placement {
                id: 1,
                depth: 0,
                handle: ResourceHandle::new(1).unwrap(),
                crop: [0, 0, 1, 1],
                destination: [0, 0],
                size: [8, 16],
                opacity: 255,
                ordinal: 0,
            }]
        );
        let omitted = decode_slot(slot(literals::FRAME_OMISSION), context(3, 1)).unwrap();
        assert_eq!(omitted.cells[0].glyph, Glyph::Scalar('?'));
        assert_eq!(omitted.placements, Vec::<Placement>::new());
    }

    #[test]
    fn canonical_offsets_padding_and_the_entire_slot_are_checked() {
        for (offset, value, error) in [
            (104, 1, FrameError::Header),
            (88, 152, FrameError::Sections),
            (96, 8, FrameError::Header),
            (168, 0, FrameError::Placement),
            (208, 1, FrameError::Padding),
            (SLOT_BYTES as usize - 4, 1, FrameError::Padding),
            (44, 136, FrameError::Sections),
            (0, 216, FrameError::Sections),
        ] {
            let mut bytes = slot(literals::FRAME_PLACEMENT);
            word(&mut bytes, offset, value);
            assert_eq!(
                decode_slot(bytes, context(2, 1)),
                Err(error),
                "offset {offset}"
            );
        }
        let mut bytes = cluster_frame("e\u{301}".as_bytes(), 1);
        bytes[163] = 1;
        assert_eq!(decode_slot(bytes, context(1, 1)), Err(FrameError::Padding));
        assert_eq!(
            decode_slot(vec![0; 144].into_boxed_slice(), context(1, 1)),
            Err(FrameError::Length)
        );
    }

    #[test]
    fn frame_identity_geometry_and_activation_require_the_negotiated_context() {
        for (offset, value, error) in [
            (8, 2, FrameError::Header),
            (16, 8, FrameError::Header),
            (24, 2, FrameError::Geometry),
            (28, 0, FrameError::Geometry),
            (32, 32_769, FrameError::Geometry),
            (76, 1, FrameError::Header),
            (80, 144, FrameError::Header),
            (72, 2, FrameError::Cursor),
            (64, 1, FrameError::Cursor),
        ] {
            let mut bytes = slot(literals::FRAME_INITIAL);
            word(&mut bytes, offset, value);
            assert_eq!(
                decode_slot(bytes, context(1, 1)),
                Err(error),
                "offset {offset}"
            );
        }
        let mut bytes = slot(literals::FRAME_PLACEMENT);
        word(&mut bytes, 8, 1);
        assert_eq!(decode_slot(bytes, context(1, 1)), Err(FrameError::Sections));
        let mut bytes = slot(literals::FRAME_INITIAL);
        bytes[6] = 0;
        assert_eq!(decode_slot(bytes, context(1, 1)), Err(FrameError::Header));
        let mut bytes = slot(literals::FRAME_INITIAL);
        word(&mut bytes, 72, 1);
        assert_eq!(
            decode_slot(bytes, context(1, 1)).unwrap().cursor,
            Some([0, 0])
        );
    }

    #[test]
    fn cell_colors_styles_and_wide_pairs_follow_the_wire_schema() {
        for (offset, value) in [
            (128, 0x0000_000a),
            (128, 0x0011_0000),
            (128, 0x8000_0000),
            (128, 0x0000_d800),
            (132, 1),
            (132, 0x4100_0001),
            (132, 0x8100_0000),
            (136, 0xc000_0000),
            (140, 0x1800),
            (140, 0x1030),
            (140, 0x3000),
            (140, 0x5000),
            (140, 0x0001_1000),
            (140, 0x2000),
        ] {
            let mut bytes = slot(literals::FRAME_INITIAL);
            word(&mut bytes, offset, value);
            assert_eq!(decode_slot(bytes, context(1, 1)), Err(FrameError::Cell));
        }
        let mut bytes = two_cells();
        word(&mut bytes, 128, 0x4e2d);
        word(&mut bytes, 140, 0x2000);
        word(&mut bytes, 144, 0x0011_0000);
        word(&mut bytes, 156, 0);
        let frame = decode_slot(bytes.clone(), context(1, 2)).unwrap();
        assert_eq!(frame.cells[0].glyph, Glyph::Scalar('中'));
        assert_eq!(frame.cells[1].glyph, Glyph::Continuation);
        word(&mut bytes, 144, 0x20);
        word(&mut bytes, 156, 0x1000);
        assert_eq!(decode_slot(bytes, context(1, 2)), Err(FrameError::Cell));
        for color in [0, 0x4000_00ff, 0x80ab_cdef] {
            let mut bytes = slot(literals::FRAME_INITIAL);
            word(&mut bytes, 132, color);
            assert_eq!(
                decode_slot(bytes, context(1, 1)).unwrap().cells[0].foreground,
                color
            );
        }
    }

    #[test]
    fn clusters_are_packed_single_graphemes_and_owned_by_the_snapshot() {
        let frame = decode_slot(cluster_frame("e\u{301}".as_bytes(), 1), context(1, 1)).unwrap();
        assert_eq!(frame.clusters, vec!["e\u{301}"]);
        assert_eq!(frame.cells[0].glyph, Glyph::Cluster(0));
        for text in [&b"ab"[..], &b"a\n"[..], &b"\xff"[..]] {
            assert_eq!(
                decode_slot(cluster_frame(text, 1), context(1, 1)),
                Err(FrameError::Cluster)
            );
        }
        for (offset, value, error) in [
            (144, 1, FrameError::Cluster),
            (148, 1025, FrameError::Cluster),
            (152, 3, FrameError::Cluster),
            (156, 1, FrameError::Cluster),
            (128, 0x8000_0001, FrameError::Cell),
            (140, 0x2000, FrameError::Cell),
        ] {
            let mut bytes = cluster_frame("e\u{301}".as_bytes(), 1);
            word(&mut bytes, offset, value);
            assert_eq!(decode_slot(bytes, context(1, 1)), Err(error));
        }
    }

    #[test]
    fn damage_is_ordered_in_bounds_and_contains_complete_wide_cells() {
        let mut bytes = two_cells();
        bytes[6] = 0;
        word(&mut bytes, 8, 2);
        word(&mut bytes, 0, 176);
        word(&mut bytes, 56, 1);
        word(&mut bytes, 128, 0x4e2d);
        word(&mut bytes, 140, 0x2000);
        word(&mut bytes, 144, 0x0011_0000);
        word(&mut bytes, 156, 0);
        word(&mut bytes, 168, 2);
        assert_eq!(
            decode_slot(bytes.clone(), context(2, 2)).unwrap().damage,
            vec![Damage {
                row: 0,
                column: 0,
                cells: 2
            }]
        );
        for (offset, value) in [(160, 1), (164, 1), (168, 0), (168, 1), (168, 3), (172, 1)] {
            let mut invalid = bytes.clone();
            word(&mut invalid, offset, value);
            assert_eq!(decode_slot(invalid, context(2, 2)), Err(FrameError::Damage));
        }
        let mut continuation_only = bytes.clone();
        word(&mut continuation_only, 164, 1);
        word(&mut continuation_only, 168, 1);
        assert_eq!(
            decode_slot(continuation_only, context(2, 2)),
            Err(FrameError::Damage)
        );
        word(&mut bytes, 0, 192);
        word(&mut bytes, 56, 2);
        word(&mut bytes, 184, 2);
        assert_eq!(decode_slot(bytes, context(2, 2)), Err(FrameError::Damage));
    }

    #[test]
    fn placements_bound_rectangles_ids_order_and_resource_crops() {
        for (offset, value) in [
            (144, 0),
            (152, 0),
            (168, 0),
            (172, 0),
            (184, 0),
            (188, 1_073_741_825),
            (176, 1_073_741_824),
            (180, (-1_073_741_825i32) as u32),
            (196, 1),
            (200, 1),
        ] {
            let mut bytes = slot(literals::FRAME_PLACEMENT);
            word(&mut bytes, offset, value);
            assert_eq!(
                decode_slot(bytes, context(2, 1)),
                Err(FrameError::Placement)
            );
        }
        let mut bytes = slot(literals::FRAME_PLACEMENT);
        bytes[193] = 1;
        assert_eq!(
            decode_slot(bytes, context(2, 1)),
            Err(FrameError::Placement)
        );
        let mut bytes = slot(literals::FRAME_PLACEMENT);
        word(&mut bytes, 0, 272);
        word(&mut bytes, 84, 2);
        bytes.copy_within(144..208, 208);
        word(&mut bytes, 260, 1);
        assert_eq!(
            decode_slot(bytes, context(2, 1)),
            Err(FrameError::Placement)
        );
        let frame = decode_slot(slot(literals::FRAME_PLACEMENT), context(2, 1)).unwrap();
        let placement = frame.placements[0];
        assert!(placement.crop_fits(1, 1));
        assert!(!placement.crop_fits(0, 1));
        assert!(!Placement {
            crop: [u32::MAX, 0, 1, 1],
            ..placement
        }
        .crop_fits(u32::MAX, 1));
    }
}
