//! Kitty unicode-placeholder cells (`U+10EEEE` + diacritics). Ghostty
//! `graphics_unicode.zig` is the layout reference.

use prismattyc_core::Color;

/// Placeholder codepoint. Never paint this as a font glyph (macOS Last Resort).
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Kitty `rowcolumn-diacritics.txt` (297 entries). Index is the encoded value.
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

pub fn diacritic_index(ch: char) -> Option<u16> {
    DIACRITICS
        .binary_search(&(ch as u32))
        .ok()
        .and_then(|i| u16::try_from(i).ok())
}

/// Low 24 bits of the image id from cell fg (or placement id from underline).
pub fn color_to_id(color: Color) -> Option<u32> {
    match color {
        Color::Default => None,
        Color::Ansi(n) | Color::Indexed(n) => Some(u32::from(n)),
        Color::Rgb { r, g, b } => Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
    }
}

/// Combine fg id with optional third-diacritic MSB. Index > 255 is absent (0).
pub fn image_id(id_low: u32, msb_index: Option<u16>) -> u32 {
    let msb = msb_index.and_then(|i| u8::try_from(i).ok()).unwrap_or(0);
    id_low | (u32::from(msb) << 24)
}

/// Decoded row/column/MSB for one placeholder cell on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderPos {
    pub row: u32,
    pub col: u32,
    pub msb: u8,
}

/// Kitty continuation, left-to-right on one row (Ghostty `placeholderTarget`).
///
/// `prev` is the previous placeholder on this row with the same image and
/// placement ids. Reset it at each row start and on a non-placeholder.
pub fn decode_placeholder_pos(marks: &[char], prev: Option<PlaceholderPos>) -> PlaceholderPos {
    let row_d = marks.first().copied().and_then(diacritic_index);
    let col_d = marks.get(1).copied().and_then(diacritic_index);
    let msb_d = marks.get(2).copied().and_then(diacritic_index);
    let msb_or = |fallback: u8| msb_d.and_then(|i| u8::try_from(i).ok()).unwrap_or(fallback);
    match (row_d, col_d, prev) {
        (None, None, Some(p)) => PlaceholderPos {
            row: p.row,
            col: p.col.saturating_add(1),
            msb: p.msb,
        },
        (Some(r), None, Some(p)) if p.row == u32::from(r) => PlaceholderPos {
            row: u32::from(r),
            col: p.col.saturating_add(1),
            msb: p.msb,
        },
        (Some(r), Some(c), Some(p)) if p.col.saturating_add(1) == u32::from(c) => PlaceholderPos {
            row: u32::from(r),
            col: u32::from(c),
            msb: msb_or(p.msb),
        },
        (Some(r), Some(c), _) => PlaceholderPos {
            row: u32::from(r),
            col: u32::from(c),
            msb: msb_or(0),
        },
        (Some(r), None, _) => PlaceholderPos {
            row: u32::from(r),
            col: 0,
            msb: msb_or(0),
        },
        (None, Some(c), _) => PlaceholderPos {
            row: 0,
            col: u32::from(c),
            msb: msb_or(0),
        },
        (None, None, None) => PlaceholderPos {
            row: 0,
            col: 0,
            msb: msb_or(0),
        },
    }
}

/// One cell of a fitted virtual-placement grid, in image and cell pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellTile {
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub dst_ox: u32,
    pub dst_oy: u32,
    pub dst_w: u32,
    pub dst_h: u32,
}

/// Fit `img_w×img_h` into `cols×rows` cells (aspect preserved, letterboxed).
/// Then take the slice for cell (`col`, `row`). Ghostty `renderPlacement`.
#[allow(clippy::too_many_arguments)]
pub fn placeholder_tile(
    img_w: u32,
    img_h: u32,
    cols: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
    row: u32,
    col: u32,
) -> Option<CellTile> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    if col >= cols || row >= rows || img_w == 0 || img_h == 0 || cell_w == 0 || cell_h == 0 {
        return None;
    }
    let grid_w = cols.saturating_mul(cell_w);
    let grid_h = rows.saturating_mul(cell_h);
    let sx = grid_w as f64 / img_w as f64;
    let sy = grid_h as f64 / img_h as f64;
    let scale = sx.min(sy);
    if scale <= 0.0 {
        return None;
    }
    let fit_w = img_w as f64 * scale;
    let fit_h = img_h as f64 * scale;
    let ox = (grid_w as f64 - fit_w) / 2.0;
    let oy = (grid_h as f64 - fit_h) / 2.0;
    let cell_x0 = f64::from(col.saturating_mul(cell_w));
    let cell_y0 = f64::from(row.saturating_mul(cell_h));
    let cell_x1 = cell_x0 + f64::from(cell_w);
    let cell_y1 = cell_y0 + f64::from(cell_h);
    let ix0 = cell_x0.max(ox);
    let iy0 = cell_y0.max(oy);
    let ix1 = cell_x1.min(ox + fit_w);
    let iy1 = cell_y1.min(oy + fit_h);
    if ix1 <= ix0 || iy1 <= iy0 {
        return None;
    }
    let src_x = ((ix0 - ox) / scale).round().max(0.0) as u32;
    let src_y = ((iy0 - oy) / scale).round().max(0.0) as u32;
    let src_w = ((ix1 - ix0) / scale).round().max(1.0) as u32;
    let src_h = ((iy1 - iy0) / scale).round().max(1.0) as u32;
    Some(CellTile {
        src_x: src_x.min(img_w.saturating_sub(1)),
        src_y: src_y.min(img_h.saturating_sub(1)),
        src_w: src_w.min(img_w.saturating_sub(src_x).max(1)),
        src_h: src_h.min(img_h.saturating_sub(src_y).max(1)),
        dst_ox: (ix0 - cell_x0).round().max(0.0) as u32,
        dst_oy: (iy0 - cell_y0).round().max(0.0) as u32,
        dst_w: (ix1 - ix0).round().max(1.0) as u32,
        dst_h: (iy1 - iy0).round().max(1.0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diacritics_map_first_entries() {
        assert_eq!(diacritic_index('\u{0305}'), Some(0));
        assert_eq!(diacritic_index('\u{030D}'), Some(1));
        assert_eq!(diacritic_index('\u{030E}'), Some(2));
        assert!(diacritic_index('A').is_none());
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn color_to_id_rgb_ansi_indexed() {
        assert_eq!(color_to_id(Color::Default), None);
        assert_eq!(color_to_id(Color::Ansi(42)), Some(42));
        assert_eq!(color_to_id(Color::Indexed(7)), Some(7));
        assert_eq!(color_to_id(Color::Rgb { r: 1, g: 2, b: 3 }), Some(0x010203));
    }

    #[test]
    fn image_id_msb_overflow_is_absent() {
        assert_eq!(image_id(7, Some(256)), 7);
        assert_eq!(image_id(7, Some(1)), 7 | (1 << 24));
    }

    #[test]
    fn two_by_one_fit_maps_cells_without_stretch_loss() {
        // 2×1 px image in 2×1 cells of 1×1 px: 1:1.
        let a = placeholder_tile(2, 1, 2, 1, 1, 1, 0, 0).unwrap();
        let b = placeholder_tile(2, 1, 2, 1, 1, 1, 0, 1).unwrap();
        assert_eq!(a.src_x, 0);
        assert_eq!(b.src_x, 1);
        assert_eq!(a.src_w, 1);
        assert_eq!(b.src_w, 1);
    }

    #[test]
    fn continuation_resets_column_on_new_row_diacritic() {
        let start = decode_placeholder_pos(&['\u{0305}', '\u{0305}'], None);
        assert_eq!(
            start,
            PlaceholderPos {
                row: 0,
                col: 0,
                msb: 0
            }
        );
        let next = decode_placeholder_pos(&[], Some(start));
        assert_eq!(
            next,
            PlaceholderPos {
                row: 0,
                col: 1,
                msb: 0
            }
        );
        // New row, only a row diacritic, no previous cell on this row → col 0.
        let row1 = decode_placeholder_pos(&['\u{030D}'], None);
        assert_eq!(
            row1,
            PlaceholderPos {
                row: 1,
                col: 0,
                msb: 0
            }
        );
        let row1_next = decode_placeholder_pos(&[], Some(row1));
        assert_eq!(
            row1_next,
            PlaceholderPos {
                row: 1,
                col: 1,
                msb: 0
            }
        );
    }

    #[test]
    fn continuation_inherits_msb_on_bare_run() {
        let start = decode_placeholder_pos(&['\u{0305}', '\u{0305}', '\u{030D}'], None);
        assert_eq!(start.msb, 1);
        let next = decode_placeholder_pos(&[], Some(start));
        assert_eq!(next.msb, 1);
        assert_eq!(next.col, 1);
    }
}
