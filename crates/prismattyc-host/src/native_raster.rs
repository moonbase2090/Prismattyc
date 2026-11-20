// SPDX-License-Identifier: MIT
//! Native ink keeps complete graphemes and the producer's declared cell span.

use super::*;
use crate::native::{
    composition::{source_over, SamplingAxis},
    frame::Cell as NativeCell,
    grid::{NativeGrid, Text},
    scene::NativeScene,
};

pub(crate) struct Paint<'a> {
    pub scene: &'a NativeScene,
    pub theme: &'a Theme,
    pub font: &'a FontMetrics,
    pub selection: Option<CellRange>,
    pub cursor: bool,
    pub blink_on: bool,
    pub background: ScreenPaint,
}

pub(crate) struct Target<'a> {
    pub buffer: &'a mut [u32],
    pub stride: usize,
    /// Guest rectangle in framebuffer pixels.
    pub rect: [usize; 4],
}

impl Target<'_> {
    fn over(&mut self, x: usize, y: usize, source: [u8; 4], opacity: u8) {
        let index = y * self.stride + x;
        let destination = self.buffer[index];
        let [r, g, b] = unpack_rgb(destination);
        let output = source_over(source, [r, g, b, alpha_of(destination)], opacity);
        self.buffer[index] = pack_argb(output[3], [output[0], output[1], output[2]]);
    }
}

fn color(theme: &Theme, encoded: u32, foreground: bool) -> [u8; 3] {
    match encoded >> 30 {
        0 => {
            if foreground {
                theme.default_fg
            } else {
                theme.default_bg
            }
        }
        1 => palette_rgb_with_theme(theme, encoded as u8),
        2 => unpack_rgb(encoded),
        _ => unreachable!("native color validated before scene installation"),
    }
}

fn colors(paint: &Paint<'_>, cell: &NativeCell, selected: bool) -> ([u8; 3], [u8; 3]) {
    let mut fg = color(paint.theme, cell.foreground, true);
    let mut bg = color(paint.theme, cell.background, false);
    if cell.meta & (1 << 7) != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.meta & (1 << 1) != 0 {
        fg = fg.map(|channel| channel / 2);
    }
    if selected {
        if let (Some(selection_fg), Some(selection_bg)) =
            (paint.theme.selection_fg, paint.theme.selection_bg)
        {
            (fg, bg) = (selection_fg, selection_bg);
        } else {
            std::mem::swap(&mut fg, &mut bg);
        }
    }
    (fg, bg)
}

fn selection_span(
    grid: NativeGrid<'_>,
    selection: Option<CellRange>,
    row: usize,
) -> Option<(usize, usize)> {
    let selection = selection?.normalized();
    if row < selection.start_row || row > selection.end_row {
        return None;
    }
    let start = if row == selection.start_row {
        selection.start_col
    } else {
        0
    };
    let end = if row == selection.end_row {
        selection.end_col
    } else {
        grid.columns() - 1
    }
    .min(grid.columns() - 1);
    if start > end {
        return None;
    }
    if selection.start_row == selection.end_row {
        return Some((start, end));
    }
    let last = (start..=end)
        .rev()
        .find_map(|column| {
            let cell = grid.cell(column, row).unwrap();
            match cell.text {
                Text::Continuation | Text::Scalar(' ') => None,
                _ => Some((column + usize::from(cell.cell.width()) - 1).min(end)),
            }
        })
        .unwrap_or(end);
    Some((start, last))
}

fn images(paint: &Paint<'_>, target: &mut Target<'_>, negative: bool) {
    let [ox, oy, width, height] = target.rect;
    for item in paint
        .scene
        .images()
        .iter()
        .filter(|item| (item.placement.depth < 0) == negative)
    {
        let placement = item.placement;
        let [sx, sy, sw, sh] = placement.crop;
        let [dx, dy] = placement.destination;
        let [dw, dh] = placement.size;
        let x_axis = SamplingAxis::new(sx, sw, item.pixels.width(), dx, dw).unwrap();
        let y_axis = SamplingAxis::new(sy, sh, item.pixels.height(), dy, dh).unwrap();
        let x0 = i64::from(dx).max(0) as usize;
        let y0 = i64::from(dy).max(0) as usize;
        let x1 = (i64::from(dx) + i64::from(dw)).max(0).min(width as i64) as usize;
        let y1 = (i64::from(dy) + i64::from(dh)).max(0).min(height as i64) as usize;
        for y in y0..y1 {
            let source_y = y_axis.at(y as i32).unwrap() as usize;
            for x in x0..x1 {
                let source_x = x_axis.at(x as i32).unwrap() as usize;
                let offset = (source_y * item.pixels.width() as usize + source_x) * 4;
                target.over(
                    ox + x,
                    oy + y,
                    item.pixels.pixels()[offset..offset + 4].try_into().unwrap(),
                    placement.opacity,
                );
            }
        }
    }
}

fn ink(
    paint: &Paint<'_>,
    target: &mut Target<'_>,
    text: &str,
    span: usize,
    at: [usize; 2],
    fg: [u8; 3],
    meta: u16,
) {
    let [ox, oy, width, height] = target.rect;
    let [x, y] = at;
    let glyph = paint.font.native_ink(text, span);
    let glyph_width = paint.font.cell_w * span;
    let glyph_height = paint.font.cell_h;
    for sy in 0..glyph_height.min(height.saturating_sub(y)) {
        let slant = if meta & 4 != 0 {
            (glyph_height as isize / 2 - sy as isize) / 4
        } else {
            0
        };
        for sx in 0..glyph_width {
            let position = sy * glyph_width + sx;
            let source = match glyph.as_ref() {
                Ink::Mask(mask) => [fg[0], fg[1], fg[2], mask[position]],
                Ink::Color(pixels) => {
                    let mut rgba: [u8; 4] =
                        pixels[position * 4..position * 4 + 4].try_into().unwrap();
                    if meta & 2 != 0 {
                        for channel in &mut rgba[..3] {
                            *channel /= 2;
                        }
                    }
                    rgba
                }
            };
            if source[3] == 0 {
                continue;
            }
            for bold_offset in 0..=usize::from(meta & 1 != 0) {
                let shifted = sx as isize + slant + bold_offset as isize;
                if shifted >= 0
                    && (shifted as usize) < glyph_width
                    && x + (shifted as usize) < width
                {
                    target.over(ox + x + shifted as usize, oy + y + sy, source, 255);
                }
            }
        }
    }
}

/// All passes borrow one installed scene. No ANSI cells or placements participate.
pub(crate) fn rasterize(paint: Paint<'_>, mut target: Target<'_>) {
    if target.stride == 0 {
        return;
    }
    let [ox, oy, requested_width, requested_height] = target.rect;
    let width = requested_width.min(target.stride.saturating_sub(ox));
    let height = requested_height.min((target.buffer.len() / target.stride).saturating_sub(oy));
    target.rect = [ox, oy, width, height];
    if width == 0 || height == 0 {
        return;
    }
    let grid = paint.scene.grid();
    let rows = grid.rows().min(height.div_ceil(paint.font.cell_h));
    let columns = grid.columns().min(width.div_ceil(paint.font.cell_w));
    for row in 0..rows {
        let selected = selection_span(grid, paint.selection, row);
        for column in 0..columns {
            let raw = grid.cell(column, row).unwrap().cell;
            let cell = grid.style_source(column, row).unwrap();
            let lead_column = if raw.width() == 0 { column - 1 } else { column };
            let selected =
                selected.is_some_and(|(first, last)| (first..=last).contains(&lead_column));
            let (_, bg) = colors(&paint, cell, selected);
            let default_bg = cell.background == 0 && cell.meta & 0x80 == 0 && !selected;
            let (x, y) = (column * paint.font.cell_w, row * paint.font.cell_h);
            paint_cell_background(
                target.buffer,
                target.stride,
                ox + x,
                oy + y,
                paint.font.cell_w.min(width - x),
                paint.font.cell_h.min(height - y),
                bg,
                if default_bg {
                    paint.background.default_bg_alpha
                } else {
                    255
                },
                default_bg && paint.background.skip_default_bg,
                selected,
                paint.background.bg_weight,
            );
        }
    }
    images(&paint, &mut target, true);
    let mut text = String::new();
    for row in 0..rows {
        let selected = selection_span(grid, paint.selection, row);
        for column in 0..columns {
            let view = grid.cell(column, row).unwrap();
            let cell = grid.style_source(column, row).unwrap();
            if cell.meta & 0x100 != 0 || cell.meta & 0x40 != 0 && !paint.blink_on {
                continue;
            }
            let lead_column = if view.cell.width() == 0 {
                column - 1
            } else {
                column
            };
            let selected =
                selected.is_some_and(|(first, last)| (first..=last).contains(&lead_column));
            let (fg, _) = colors(&paint, cell, selected);
            let (x, y) = (column * paint.font.cell_w, row * paint.font.cell_h);
            text.clear();
            view.text.append_to(&mut text);
            if view.cell.width() != 0 {
                ink(
                    &paint,
                    &mut target,
                    &text,
                    usize::from(cell.width()),
                    [x, y],
                    fg,
                    cell.meta,
                );
            }
            let underline = match (cell.meta >> 3) & 7 {
                0 => UnderlineStyle::None,
                1 => UnderlineStyle::Single,
                2 => UnderlineStyle::Double,
                3 => UnderlineStyle::Curly,
                4 => UnderlineStyle::Dotted,
                5 => UnderlineStyle::Dashed,
                _ => unreachable!("validated underline"),
            };
            let cell_width = paint.font.cell_w.min(width - x);
            draw_underline(
                target.buffer,
                target.stride,
                ox + x,
                oy + y,
                cell_width,
                paint.font.cell_h,
                oy + height,
                underline,
                fg,
            );
            for (bit, line) in [(9, y + paint.font.cell_h / 2), (10, y)] {
                if cell.meta & (1 << bit) != 0 && line < height {
                    fill_rect(
                        target.buffer,
                        target.stride,
                        ox + x,
                        oy + line,
                        cell_width,
                        1,
                        fg,
                    );
                }
            }
        }
    }
    if let Some([column, row]) = grid.cursor().filter(|_| paint.cursor) {
        let (x, y) = (column * paint.font.cell_w, row * paint.font.cell_h);
        if x < width && y < height {
            fill_rect(
                target.buffer,
                target.stride,
                ox + x,
                oy + y,
                paint.font.cell_w.min(width - x),
                paint.font.cell_h.min(height - y),
                paint.theme.cursor_bg.unwrap_or(paint.theme.default_fg),
            );
            let view = grid.cell(column, row).unwrap();
            let style = grid.style_source(column, row).unwrap();
            text.clear();
            view.text.append_to(&mut text);
            if view.cell.width() != 0
                && style.meta & 0x100 == 0
                && (style.meta & 0x40 == 0 || paint.blink_on)
            {
                target.rect[2] = (x + paint.font.cell_w).min(width);
                ink(
                    &paint,
                    &mut target,
                    &text,
                    usize::from(view.cell.width()),
                    [x, y],
                    paint.theme.cursor_fg.unwrap_or(paint.theme.default_bg),
                    style.meta,
                );
                target.rect[2] = width;
            }
        }
    }
    images(&paint, &mut target, false);
}

pub(super) enum Ink {
    Mask(Vec<u8>),
    Color(Vec<u8>),
}

struct ShapedMask {
    complete: bool,
    pixels: Vec<u8>,
}

fn choose_shape(
    shaped: Option<ShapedMask>,
    covers_base: bool,
    partial: &mut Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let shaped = shaped?;
    if shaped.complete {
        return Some(shaped.pixels);
    }
    if covers_base && partial.is_none() {
        *partial = Some(shaped.pixels);
    }
    None
}

fn shaped_mask(
    font: &Font,
    bytes: &[u8],
    index: u32,
    metrics: &FontMetrics,
    text: &str,
    width: usize,
) -> Option<ShapedMask> {
    let face = rustybuzz::Face::from_slice(bytes, index)?;
    let scale = metrics.px / face.units_per_em() as f32;
    let mut input = rustybuzz::UnicodeBuffer::new();
    input.push_str(text);
    input.guess_segment_properties();
    let shaped = rustybuzz::shape(&face, &[], input);
    let complete = !shaped.glyph_infos().iter().any(|glyph| glyph.glyph_id == 0);
    let height = metrics.cell_h;
    let mut mask = vec![0u8; width * height];
    let (mut pen_x, mut pen_y) = (0f32, 0f32);
    for (glyph, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        let glyph_x = pen_x + position.x_offset as f32 * scale;
        let glyph_y = pen_y + position.y_offset as f32 * scale;
        pen_x += position.x_advance as f32 * scale;
        pen_y += position.y_advance as f32 * scale;
        if glyph.glyph_id == 0 {
            continue;
        }
        let (bounds, bitmap) =
            font.rasterize_indexed(u16::try_from(glyph.glyph_id).ok()?, metrics.px);
        let x = glyph_x.round() as i32 + bounds.xmin;
        let y = (metrics.baseline - glyph_y).round() as i32 - bounds.ymin - bounds.height as i32;
        for by in 0..bounds.height {
            for bx in 0..bounds.width {
                let (dx, dy) = (x + bx as i32, y + by as i32);
                if dx >= 0 && dy >= 0 && dx < width as i32 && dy < height as i32 {
                    let dest = &mut mask[dy as usize * width + dx as usize];
                    *dest = raise_alpha(*dest, bitmap[by * bounds.width + bx]);
                }
            }
        }
    }
    Some(ShapedMask {
        complete,
        pixels: mask,
    })
}

impl FontMetrics {
    fn native_ink(&self, text: &str, span: usize) -> Arc<Ink> {
        let cache = &self.native_ink[span - 1];
        if let Some(ink) = cache.borrow_mut().get(text) {
            return ink;
        }
        self.painted_pending.set(false);
        let ink = Arc::new(self.paint_native_ink(text, span));
        if !self.painted_pending.get() {
            cache.borrow_mut().insert(text.into(), Arc::clone(&ink));
        }
        ink
    }

    fn paint_native_ink(&self, text: &str, span: usize) -> Ink {
        let width = self.cell_w * span;
        let height = self.cell_h;
        let mut characters = text.chars();
        let first = characters.next().unwrap();
        if characters.next().is_none() {
            if let Some(sprite) = sprite_coverage(first, width, height) {
                return Ink::Mask(sprite);
            }
        }
        let emoji = self.cached_emoji_cluster(text).or_else(|| {
            (text.chars().count() == 1)
                .then(|| self.rasterize_color_emoji(first))
                .flatten()
                .map(Arc::new)
        });
        if let Some(emoji) = emoji {
            let (w, h, pixels) =
                scale_rgba_nearest(emoji.width, emoji.height, &emoji.rgba, width, height);
            let mut canvas = vec![0; width * height * 4];
            let (x, y) = ((width - w) / 2, (height - h) / 2);
            for row in 0..h {
                canvas[((y + row) * width + x) * 4..((y + row) * width + x + w) * 4]
                    .copy_from_slice(&pixels[row * w * 4..(row + 1) * w * 4]);
            }
            return Ink::Color(canvas);
        }
        let mut partial = None;
        if let Some(mask) = choose_shape(
            shaped_mask(
                &self.fonts[0],
                self.primary_bytes.as_ref(),
                self.primary_index,
                self,
                text,
                width,
            ),
            self.fonts[0].lookup_glyph_index(first) != 0,
            &mut partial,
        ) {
            return Ink::Mask(mask);
        }
        for lazy in &self.lazy_fonts {
            let Some(font) = lazy.for_char(first) else {
                continue;
            };
            let bytes = lazy.bytes.get().unwrap().as_ref().unwrap();
            if let Some(mask) = choose_shape(
                shaped_mask(font, bytes.as_ref(), 0, self, text, width),
                font.lookup_glyph_index(first) != 0,
                &mut partial,
            ) {
                return Ink::Mask(mask);
            }
        }
        let missing = text
            .chars()
            .find(|&character| self.fonts[0].lookup_glyph_index(character) == 0)
            .unwrap_or(first);
        if let Some(face) = self.fallback_face(missing) {
            if let Some(mask) = choose_shape(
                shaped_mask(
                    &face.font,
                    face.bytes.as_ref(),
                    face.index,
                    self,
                    text,
                    width,
                ),
                face.font.lookup_glyph_index(first) != 0,
                &mut partial,
            ) {
                return Ink::Mask(mask);
            }
        }
        // Prefer a complete face, then preserve mapped base ink while leaving
        // unsupported glyphs blank. Never rasterize a .notdef placeholder.
        Ink::Mask(partial.unwrap_or_else(|| vec![0; width * height]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_font_coverage_preserves_supported_ink_without_drawing_notdef() {
        use unicode_segmentation::UnicodeSegmentation;
        let font = FontMetrics::load_baked(20.0).unwrap();
        let text = "e\u{1abf}";
        assert_eq!(text.graphemes(true).count(), 1);
        assert_eq!(font.fonts[0].lookup_glyph_index('\u{1abf}'), 0);
        let shaped = shaped_mask(&font.fonts[0], BAKED_MONO, 0, &font, text, font.cell_w * 2);
        let shaped = shaped.expect("missing mark erased the supported base");
        assert!(!shaped.complete);
        assert!(shaped.pixels.iter().any(|&coverage| coverage != 0));
        let base = shaped_mask(&font.fonts[0], BAKED_MONO, 0, &font, "e", font.cell_w * 2).unwrap();
        assert!(base.complete);
        assert_eq!(
            shaped.pixels, base.pixels,
            "missing glyph drew a .notdef placeholder"
        );
        let mut partial = None;
        assert!(choose_shape(Some(shaped), true, &mut partial).is_none());
        assert_eq!(partial.as_deref(), Some(base.pixels.as_slice()));
        assert!(choose_shape(Some(base), true, &mut partial).is_some());
    }

    #[test]
    fn native_shaping_keeps_marks_after_the_first_scalar_and_declared_width() {
        let font = FontMetrics::load_baked(20.0).unwrap();
        let Ink::Mask(base) = font.paint_native_ink("e", 1) else {
            panic!("outline fixture")
        };
        let Ink::Mask(cluster) = font.paint_native_ink("e\u{323}", 1) else {
            panic!("outline fixture")
        };
        assert!(base.iter().any(|&coverage| coverage != 0));
        assert!(cluster.iter().zip(&base).any(|(&mark, &base)| mark > base));
        let Ink::Mask(wide) = font.paint_native_ink("\u{2588}", 2) else {
            panic!("sprite fixture")
        };
        assert_eq!(wide.len(), 2 * font.cell_w * font.cell_h);
        assert!(wide.iter().all(|&coverage| coverage == 255));
    }
}
