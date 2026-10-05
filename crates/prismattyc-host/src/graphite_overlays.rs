//! Graphite treatment for host-owned dialogs and small overlays.
//!
//! Layout and hit-testing for palette rows stay in `raster`; this module owns
//! Graphite's typography, surfaces, and dialog geometry. Classic paths do not
//! call these painters.

use crate::graphite::{self, Face, Rect};
use crate::mux::ChromeGeom;
use crate::raster::{
    self, FontMetrics, OverlaySurface, PaletteFrame, PaletteLayout, PaletteLine, ThemePickerRow,
};
use crate::theme::{Theme, ThemeVariant};

const PALETTE_RADIUS: f32 = 10.0;
const THEME_DIALOG_W: f32 = 760.0;
const THEME_DIALOG_H: f32 = 460.0;
const THEME_HEADER_H: f32 = 56.0;
const THEME_FOOTER_H: f32 = 52.0;
const THEME_PAD: f32 = 16.0;
const THEME_ROW_H: f32 = 38.0;

fn panel_color(variant: ThemeVariant) -> [u8; 3] {
    match variant {
        ThemeVariant::Dark => [0x18, 0x1b, 0x21],
        ThemeVariant::Light => [0xff, 0xff, 0xff],
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    buffer: &mut [u32],
    stride: usize,
    rect: Rect,
    face: Face,
    px: f32,
    text: &str,
    color: [u8; 3],
    clip_x0: usize,
    clip_x1: usize,
) {
    graphite::draw_text(
        buffer,
        stride,
        rect.x as f32,
        rect.y as f32 + rect.h as f32 / 2.0,
        face,
        px,
        text,
        color,
        clip_x0,
        clip_x1,
    );
}

fn draw_panel(
    buffer: &mut [u32],
    stride: usize,
    panel: Rect,
    variant: ThemeVariant,
    surface: OverlaySurface,
) {
    let tok = graphite::tokens(variant);
    let shadow = Rect::new(
        panel.x.saturating_add(4),
        panel.y.saturating_add(6),
        panel.w,
        panel.h,
    );
    raster::paint_overlay_surface_rounded(
        buffer,
        stride,
        shadow.x,
        shadow.y,
        shadow.w,
        shadow.h,
        PALETTE_RADIUS,
        [0, 0, 0],
        OverlaySurface {
            opacity: surface.opacity * 0.22,
            blur_radius: 0,
        },
    );
    graphite::stroke_round_rect(
        buffer,
        stride,
        panel.x as f32,
        panel.y as f32,
        panel.right() as f32,
        panel.y.saturating_add(panel.h) as f32,
        PALETTE_RADIUS,
        1.0,
        tok.hairline,
    );
    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.w.saturating_sub(2),
        panel.h.saturating_sub(2),
    );
    raster::paint_overlay_surface_rounded(
        buffer,
        stride,
        inner.x,
        inner.y,
        inner.w,
        inner.h,
        PALETTE_RADIUS - 1.0,
        panel_color(variant),
        surface,
    );
}

/// Paint the shared fixed-height palette geometry with Graphite tokens.
#[allow(clippy::too_many_arguments)]
pub(crate) fn palette(
    font: &FontMetrics,
    variant: ThemeVariant,
    chrome: ChromeGeom,
    focus_rgb: [u8; 3],
    frame: &PaletteFrame<'_>,
    surface: OverlaySurface,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    hovered_filter: Option<usize>,
) -> Option<PaletteLayout> {
    let layout = raster::palette_layout(font, frame, width, height)?;
    let tok = graphite::tokens(variant);
    let accent = graphite::accent(tok, focus_rgb);
    let x = layout.panel_x;
    let y = layout.panel_y;
    let right = x.saturating_add(layout.panel_w);
    let bottom = y.saturating_add(layout.panel_h);
    let cell_w = font.cell_w.max(1);
    let cell_h = font.cell_h.max(1);
    let inset = cell_w.saturating_mul(layout.geom.inset_cells);
    let text_x = x.saturating_add(inset);
    let text_right = right.saturating_sub(inset);
    let px = chrome.px(13.0).max(10) as f32;
    let small_px = chrome.px(11.0).max(9) as f32;

    draw_panel(
        buffer,
        width,
        Rect::new(x, y, layout.panel_w, layout.panel_h),
        variant,
        surface,
    );

    let blank = if layout.geom.compact { 0 } else { cell_h };
    let mut row_y = y.saturating_add(cell_h / 2);
    if let Some(query) = frame.query {
        let query_rect = Rect::new(
            text_x,
            row_y,
            text_right.saturating_sub(text_x),
            layout.geom.query_h_px,
        );
        graphite::fill_round_rect(buffer, width, query_rect, 6.0, tok.field, 255);
        let field_line = Rect::new(query_rect.x, query_rect.y, query_rect.w, 1);
        graphite::fill_round_rect(buffer, width, field_line, 0.0, tok.field_line, 255);
        let value = format!("Search  {query}|");
        draw_text(
            buffer,
            width,
            Rect::new(
                query_rect.x.saturating_add(chrome.px(10.0)),
                query_rect.y,
                query_rect.w.saturating_sub(chrome.px(20.0)),
                query_rect.h,
            ),
            Face::Regular,
            px,
            &value,
            tok.text_strong,
            query_rect.x,
            query_rect.right(),
        );
        row_y = row_y
            .saturating_add(layout.geom.query_h_px)
            .saturating_add(blank);
    }

    if let Some((labels, selected_chip)) = frame.chips {
        let hint = "C-←/→ filter";
        let hint_w = graphite::text_width(Face::Mono, small_px, hint).ceil() as usize;
        let hint_x = text_right.saturating_sub(hint_w);
        for chip in &layout.filter_chips {
            let Some(label) = labels.get(chip.index) else {
                continue;
            };
            let selected = chip.index == selected_chip;
            let hovered = hovered_filter == Some(chip.index);
            let fill = if selected {
                tok.tab_active
            } else if hovered {
                tok.tab_hover
            } else {
                tok.bar
            };
            if selected || hovered {
                graphite::fill_round_rect(
                    buffer,
                    width,
                    Rect::new(chip.x, chip.y, chip.width, chip.height),
                    6.0,
                    fill,
                    255,
                );
            }
            let label_x = chip.x.saturating_add(cell_w);
            draw_text(
                buffer,
                width,
                Rect::new(
                    label_x,
                    chip.y,
                    chip.width.saturating_sub(cell_w * 2),
                    chip.height,
                ),
                Face::Regular,
                small_px,
                label,
                if selected { tok.text_strong } else { tok.muted },
                chip.x,
                chip.x.saturating_add(chip.width),
            );
        }
        draw_text(
            buffer,
            width,
            Rect::new(hint_x, row_y, hint_w, cell_h),
            Face::Mono,
            small_px,
            hint,
            tok.muted,
            hint_x,
            text_right,
        );
        row_y = row_y.saturating_add(cell_h).saturating_add(blank);
    }

    let list_bottom = layout
        .list_viewport
        .y
        .saturating_add(layout.list_viewport.height);
    let (lines, _) = raster::palette_collect_lines(frame);
    let mut global = lines
        .iter()
        .take(layout.start)
        .filter(|line| matches!(line, PaletteLine::Row(..)))
        .count();
    for (visible, line) in lines
        .iter()
        .skip(layout.start)
        .take(layout.shown)
        .enumerate()
    {
        if !layout.geom.compact && visible > 0 && matches!(line, PaletteLine::Header(_)) {
            row_y = row_y.saturating_add(cell_h);
        }
        if row_y.saturating_add(layout.geom.row_pitch_px) > list_bottom {
            break;
        }
        let row_rect = Rect::new(
            x.saturating_add(chrome.px(6.0)),
            row_y,
            layout.panel_w.saturating_sub(chrome.px(12.0)),
            layout.geom.row_pitch_px,
        );
        let text_y = row_y.saturating_add(layout.geom.row_pitch_px.saturating_sub(cell_h) / 2);
        match *line {
            PaletteLine::Header(section_index) => {
                let section = &frame.sections[section_index];
                let header = if section.subtitle.is_empty() {
                    section.header.to_owned()
                } else {
                    format!("{}  {}", section.header, section.subtitle)
                };
                draw_text(
                    buffer,
                    width,
                    Rect::new(text_x, text_y, text_right.saturating_sub(text_x), cell_h),
                    Face::SemiBold,
                    small_px,
                    &header,
                    tok.muted,
                    text_x,
                    text_right,
                );
                let header_w =
                    graphite::text_width(Face::SemiBold, small_px, &header).ceil() as usize;
                let rule_x = text_x.saturating_add(header_w + chrome.px(10.0));
                if rule_x < text_right {
                    graphite::fill_round_rect(
                        buffer,
                        width,
                        Rect::new(
                            rule_x,
                            text_y.saturating_add(cell_h / 2),
                            text_right - rule_x,
                            1,
                        ),
                        0.0,
                        tok.hairline,
                        255,
                    );
                }
            }
            PaletteLine::Row(section_index, row_index) => {
                let row = &frame.sections[section_index].rows[row_index];
                let selected = global == frame.selected;
                global += 1;
                if selected {
                    graphite::fill_round_rect(buffer, width, row_rect, 6.0, tok.tab_hover, 255);
                    graphite::fill_round_rect(
                        buffer,
                        width,
                        Rect::new(
                            row_rect.x,
                            row_rect.y + 4,
                            chrome.px(2.0).max(2),
                            row_rect.h.saturating_sub(8),
                        ),
                        1.0,
                        accent,
                        255,
                    );
                }
                let chord_px =
                    graphite::text_width(Face::Mono, small_px, &row.chord_label).ceil() as usize;
                let chord_x = text_right.saturating_sub(chord_px);
                let gap = chrome.px(14.0);
                let name_w = chrome.px(190.0).min(chord_x.saturating_sub(text_x + gap));
                let name = graphite::ellipsize(Face::Regular, px, &row.name, name_w as f32);
                let description_x = text_x.saturating_add(name_w).saturating_add(gap);
                let description_right = chord_x.saturating_sub(gap);
                let description = if row.full_width {
                    String::new()
                } else {
                    graphite::ellipsize(
                        Face::Regular,
                        small_px,
                        &row.describe,
                        description_right.saturating_sub(description_x) as f32,
                    )
                };
                draw_text(
                    buffer,
                    width,
                    Rect::new(text_x, text_y, name_w, cell_h),
                    Face::Regular,
                    px,
                    &name,
                    if selected { tok.text_strong } else { tok.text },
                    text_x,
                    text_x.saturating_add(name_w),
                );
                draw_text(
                    buffer,
                    width,
                    Rect::new(
                        description_x,
                        text_y,
                        description_right.saturating_sub(description_x),
                        cell_h,
                    ),
                    Face::Regular,
                    small_px,
                    &description,
                    if selected { tok.text } else { tok.muted },
                    description_x,
                    description_right,
                );
                draw_text(
                    buffer,
                    width,
                    Rect::new(chord_x, text_y, chord_px, cell_h),
                    Face::Mono,
                    small_px,
                    &row.chord_label,
                    tok.key_text,
                    chord_x,
                    text_right,
                );
            }
        }
        row_y = row_y.saturating_add(layout.geom.row_pitch_px);
    }

    if layout.more_line && row_y.saturating_add(cell_h) <= list_bottom {
        let below = lines.len().saturating_sub(layout.start + layout.shown);
        let label = if below > 0 {
            format!("↓ {below} more")
        } else {
            format!("↑ {} more", layout.start)
        };
        draw_text(
            buffer,
            width,
            Rect::new(text_x, row_y, text_right.saturating_sub(text_x), cell_h),
            Face::Regular,
            small_px,
            &label,
            tok.muted,
            text_x,
            text_right,
        );
        row_y = row_y.saturating_add(layout.geom.row_pitch_px);
    }

    let detail_y = layout
        .detail_y
        .or_else(|| frame.detail.map(|_| row_y.saturating_add(cell_h)));
    if let (Some(detail_y), Some(detail)) = (detail_y, frame.detail) {
        let detail_rect = Rect::new(
            x.saturating_add(inset / 2),
            detail_y,
            layout.panel_w.saturating_sub(inset),
            cell_h.saturating_mul(3),
        );
        graphite::fill_round_rect(buffer, width, detail_rect, 6.0, tok.field, 255);
        graphite::fill_round_rect(
            buffer,
            width,
            Rect::new(detail_rect.x, detail_rect.y, detail_rect.w, 1),
            0.0,
            tok.field_line,
            255,
        );
        let body = format!("{} — {}", detail.name, detail.text);
        let body = graphite::ellipsize(
            Face::Regular,
            small_px,
            &body,
            detail_rect.w.saturating_sub(chrome.px(20.0)) as f32,
        );
        draw_text(
            buffer,
            width,
            Rect::new(
                detail_rect.x + chrome.px(10.0),
                detail_rect.y + 2,
                detail_rect.w,
                cell_h,
            ),
            Face::Regular,
            small_px,
            &body,
            tok.text,
            detail_rect.x,
            detail_rect.right(),
        );
        let meta = format!("{} · {}", detail.chords, detail.config_key);
        draw_text(
            buffer,
            width,
            Rect::new(
                detail_rect.x + chrome.px(10.0),
                detail_rect.y + cell_h + 2,
                detail_rect.w,
                cell_h,
            ),
            Face::Mono,
            small_px,
            &meta,
            tok.muted,
            detail_rect.x,
            detail_rect.right(),
        );
    }

    let footer_y = bottom.saturating_sub(cell_h.saturating_mul(3) / 2);
    draw_text(
        buffer,
        width,
        Rect::new(text_x, footer_y, text_right.saturating_sub(text_x), cell_h),
        Face::Regular,
        small_px,
        frame.footer,
        tok.muted,
        text_x,
        text_right,
    );
    Some(layout)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemePickerHit {
    Close,
    Row(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThemePickerLayout {
    pub panel: Rect,
    pub close: Rect,
    pub list: Rect,
    pub footer_y: usize,
    pub row_h: usize,
    pub visible_rows: usize,
    pub scroll: usize,
}

pub(crate) fn theme_picker_layout(
    chrome: ChromeGeom,
    row_count: usize,
    scroll: usize,
    window_w: usize,
    window_h: usize,
) -> Option<ThemePickerLayout> {
    if window_w < chrome.px(320.0) || window_h < chrome.px(220.0) {
        return None;
    }
    let margin = chrome.px(THEME_PAD);
    let panel_w = chrome
        .px(THEME_DIALOG_W)
        .min(window_w.saturating_sub(margin * 2));
    let panel_h = chrome
        .px(THEME_DIALOG_H)
        .min(window_h.saturating_sub(margin * 2));
    let panel = Rect::new(
        window_w.saturating_sub(panel_w) / 2,
        window_h.saturating_sub(panel_h) / 2,
        panel_w,
        panel_h,
    );
    let pad = chrome.px(THEME_PAD);
    let header_h = chrome.px(THEME_HEADER_H);
    let footer_h = chrome.px(THEME_FOOTER_H);
    let list = Rect::new(
        panel.x.saturating_add(pad),
        panel.y.saturating_add(header_h),
        panel.w.saturating_sub(pad.saturating_mul(2)),
        panel.h.saturating_sub(header_h.saturating_add(footer_h)),
    );
    let row_h = chrome.px(THEME_ROW_H).max(1);
    let visible_rows = list.h / row_h;
    if visible_rows == 0 {
        return None;
    }
    let close_size = chrome.px(24.0).max(16);
    let close = Rect::new(
        panel.right().saturating_sub(pad).saturating_sub(close_size),
        panel
            .y
            .saturating_add((header_h.saturating_sub(close_size)) / 2),
        close_size,
        close_size,
    );
    let max_scroll = row_count.saturating_sub(visible_rows);
    Some(ThemePickerLayout {
        panel,
        close,
        list,
        footer_y: panel.y.saturating_add(panel.h).saturating_sub(footer_h),
        row_h,
        visible_rows,
        scroll: scroll.min(max_scroll),
    })
}

pub(crate) fn theme_picker_visible_rows(
    chrome: ChromeGeom,
    theme_count: usize,
    window_w: usize,
    window_h: usize,
) -> usize {
    if theme_count == 0 {
        return 0;
    }
    theme_picker_layout(chrome, theme_count, 0, window_w, window_h)
        .map_or(0, |layout| layout.visible_rows.min(theme_count).max(1))
}

pub(crate) fn theme_picker_hit(
    chrome: ChromeGeom,
    row_count: usize,
    scroll: usize,
    window_w: usize,
    window_h: usize,
    x: usize,
    y: usize,
) -> Option<ThemePickerHit> {
    let layout = theme_picker_layout(chrome, row_count, scroll, window_w, window_h)?;
    if layout.close.contains(x, y) {
        return Some(ThemePickerHit::Close);
    }
    if !layout.list.contains(x, y) {
        return None;
    }
    let index = layout
        .scroll
        .saturating_add(y.saturating_sub(layout.list.y) / layout.row_h);
    (index < row_count).then_some(ThemePickerHit::Row(index))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn theme_picker(
    chrome: ChromeGeom,
    variant: ThemeVariant,
    rows: &[ThemePickerRow<'_>],
    selected: Option<usize>,
    scroll: usize,
    preview: &Theme,
    hint: &str,
    surface: OverlaySurface,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    focus_rgb: [u8; 3],
    hovered_row: Option<usize>,
    hovered_close: bool,
) -> Option<ThemePickerLayout> {
    let layout = theme_picker_layout(chrome, rows.len(), scroll, width, height)?;
    let tok = graphite::tokens(variant);
    let accent = graphite::accent(tok, focus_rgb);
    let panel = layout.panel;
    draw_panel(buffer, width, panel, variant, surface);
    let pad = chrome.px(THEME_PAD);
    let text_x = panel.x.saturating_add(pad);
    let text_right = panel.right().saturating_sub(pad);
    let title = format!("Theme settings · {}", preview.name);
    draw_text(
        buffer,
        width,
        Rect::new(
            text_x,
            panel.y.saturating_add(chrome.px(7.0)),
            text_right - text_x,
            chrome.px(20.0),
        ),
        Face::SemiBold,
        chrome.px(16.0).max(12) as f32,
        &title,
        tok.text_strong,
        text_x,
        layout.close.x,
    );
    draw_text(
        buffer,
        width,
        Rect::new(
            text_x,
            panel.y.saturating_add(chrome.px(30.0)),
            text_right - text_x,
            chrome.px(18.0),
        ),
        Face::Regular,
        chrome.px(12.0).max(10) as f32,
        hint,
        tok.muted,
        text_x,
        text_right,
    );
    let close_fill = if hovered_close {
        tok.tab_hover
    } else {
        tok.field
    };
    graphite::fill_round_rect(buffer, width, layout.close, 6.0, close_fill, 255);
    draw_text(
        buffer,
        width,
        layout.close,
        Face::Regular,
        chrome.px(16.0).max(12) as f32,
        "×",
        tok.text,
        layout.close.x,
        layout.close.right(),
    );

    for index in (layout.scroll..rows.len()).take(layout.visible_rows) {
        let y = layout.list.y + (index - layout.scroll) * layout.row_h;
        let bounds = Rect::new(layout.list.x, y, layout.list.w, layout.row_h);
        let is_selected = selected == Some(index);
        let hovered = hovered_row == Some(index);
        if is_selected || hovered {
            graphite::fill_round_rect(
                buffer,
                width,
                Rect::new(bounds.x, bounds.y + 2, bounds.w, bounds.h.saturating_sub(4)),
                6.0,
                if is_selected {
                    tok.title_focus
                } else {
                    tok.tab_hover
                },
                255,
            );
        }
        if is_selected {
            graphite::fill_round_rect(
                buffer,
                width,
                Rect::new(
                    bounds.x,
                    bounds.y + 8,
                    chrome.px(2.0).max(2),
                    bounds.h.saturating_sub(16),
                ),
                1.0,
                accent,
                255,
            );
        }
        let row = &rows[index];
        let marker = if is_selected { "›" } else { " " };
        let branch = if row.branch { "  ›" } else { "" };
        let label = format!("{marker} {}{branch}", row.label);
        draw_text(
            buffer,
            width,
            Rect::new(bounds.x + chrome.px(12.0), bounds.y, bounds.w, bounds.h),
            Face::Regular,
            chrome.px(13.0).max(10) as f32,
            &label,
            if is_selected {
                tok.text_strong
            } else {
                tok.text
            },
            bounds.x,
            bounds.right(),
        );
    }
    if rows.len() > layout.visible_rows {
        let track = Rect::new(
            layout.list.right().saturating_sub(chrome.px(6.0)),
            layout.list.y,
            chrome.px(4.0).max(3),
            layout.list.h,
        );
        let thumb_h = (track.h.saturating_mul(layout.visible_rows) / rows.len())
            .clamp(chrome.px(22.0).max(12), track.h);
        let travel = track.h.saturating_sub(thumb_h);
        let max_scroll = rows.len().saturating_sub(layout.visible_rows);
        let thumb_y = track.y + layout.scroll.saturating_mul(travel) / max_scroll.max(1);
        graphite::fill_round_rect(
            buffer,
            width,
            Rect::new(track.x, thumb_y, track.w, thumb_h),
            2.0,
            tok.muted,
            255,
        );
    }

    let footer_line = Rect::new(
        panel.x.saturating_add(pad),
        layout.footer_y,
        panel.w.saturating_sub(pad.saturating_mul(2)),
        1,
    );
    graphite::fill_round_rect(buffer, width, footer_line, 0.0, tok.hairline, 255);
    draw_text(
        buffer,
        width,
        Rect::new(
            text_x,
            layout.footer_y.saturating_add(chrome.px(4.0)),
            chrome.px(80.0),
            chrome.px(16.0),
        ),
        Face::Regular,
        chrome.px(10.5).max(9) as f32,
        "ANSI 0–15",
        tok.muted,
        text_x,
        text_right,
    );
    let swatch_x = text_x.saturating_add(chrome.px(78.0));
    let swatch_y = layout.footer_y.saturating_add(chrome.px(6.0));
    let swatch_w = text_right.saturating_sub(swatch_x) / 16;
    let swatch_h = chrome.px(10.0).max(6);
    for (index, color) in preview.ansi.iter().enumerate() {
        let x = swatch_x.saturating_add(index.saturating_mul(swatch_w));
        if x >= text_right {
            break;
        }
        graphite::fill_round_rect(
            buffer,
            width,
            Rect::new(
                x,
                swatch_y,
                swatch_w.saturating_sub(chrome.px(1.0)).max(1),
                swatch_h,
            ),
            2.0,
            *color,
            255,
        );
    }
    Some(layout)
}

/// Paint small host feedback chips with the opaque Graphite text treatment.
pub(crate) fn toast(
    chrome: ChromeGeom,
    variant: ThemeVariant,
    label: &str,
    rect: Rect,
    buffer: &mut [u32],
    stride: usize,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let tok = graphite::tokens(variant);
    graphite::fill_round_rect(buffer, stride, rect, 6.0, tok.tab_active, 255);
    graphite::fill_round_rect(
        buffer,
        stride,
        Rect::new(rect.x, rect.y, rect.w, 1),
        0.0,
        tok.hairline,
        255,
    );
    let text = graphite::ellipsize(
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        label.trim(),
        rect.w.saturating_sub(chrome.px(14.0)) as f32,
    );
    draw_text(
        buffer,
        stride,
        Rect::new(
            rect.x.saturating_add(chrome.px(7.0)),
            rect.y,
            rect.w,
            rect.h,
        ),
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        &text,
        tok.text,
        rect.x,
        rect.right(),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn find_prompt(
    chrome: ChromeGeom,
    variant: ThemeVariant,
    label: &str,
    rect: Rect,
    buffer: &mut [u32],
    stride: usize,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let tok = graphite::tokens(variant);
    graphite::fill_round_rect(buffer, stride, rect, 5.0, tok.field, 255);
    graphite::fill_round_rect(
        buffer,
        stride,
        Rect::new(rect.x, rect.y, rect.w, 1),
        0.0,
        tok.field_line,
        255,
    );
    let text = graphite::ellipsize(
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        label.trim(),
        rect.w.saturating_sub(chrome.px(16.0)) as f32,
    );
    draw_text(
        buffer,
        stride,
        Rect::new(
            rect.x.saturating_add(chrome.px(8.0)),
            rect.y,
            rect.w,
            rect.h,
        ),
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        &text,
        tok.text,
        rect.x,
        rect.right(),
    );
}

/// Paint the one-row Graphite shortcut legend above the spaces rail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn legend(
    chrome: ChromeGeom,
    variant: ThemeVariant,
    text: &str,
    buffer: &mut [u32],
    stride: usize,
    bottom: usize,
    rows: usize,
    alpha: u8,
) {
    if stride == 0 || bottom == 0 || rows == 0 {
        return;
    }
    let tok = graphite::tokens(variant);
    let row_h = chrome.px(22.0).max(16);
    let h = row_h.saturating_mul(rows).min(bottom);
    let y = bottom.saturating_sub(h);
    raster::fill_rect_argb(buffer, stride, 0, y, stride, h, tok.status_bar, alpha);
    graphite::fill_round_rect(
        buffer,
        stride,
        Rect::new(0, y, stride, 1),
        0.0,
        tok.status_line,
        255,
    );
    let label = graphite::ellipsize(
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        text,
        stride.saturating_sub(chrome.px(24.0)) as f32,
    );
    draw_text(
        buffer,
        stride,
        Rect::new(chrome.px(12.0), y, stride, h),
        Face::Regular,
        chrome.px(11.0).max(9) as f32,
        &label,
        tok.text,
        0,
        stride,
    );
}

fn splash_ink(ink: [u8; 3], art: bool, variant: ThemeVariant, accent: graphite::Rgb) -> [u8; 3] {
    let tok = graphite::tokens(variant);
    if art {
        return if ink == prismattyc_core::splash::INK {
            tok.text_strong
        } else {
            ink
        };
    }
    match ink {
        crate::splash::DIM => tok.muted,
        crate::splash::ACCENT | prismattyc_core::splash::LINK => accent,
        prismattyc_core::splash::INK => tok.text_strong,
        other => other,
    }
}

fn splash_text(text: &str) -> &str {
    if text == "⏎" {
        "Enter"
    } else {
        text
    }
}

fn splash_line_style(
    page: crate::splash::Page,
    line: usize,
    art_rows: usize,
    font: &FontMetrics,
    chrome: ChromeGeom,
) -> (Face, f32) {
    if line < art_rows {
        (Face::Mono, font.px)
    } else if (page == crate::splash::Page::Main && line == art_rows + 1)
        || (page != crate::splash::Page::Main && line == 0)
    {
        (Face::SemiBold, chrome.px(16.0).max(12) as f32)
    } else {
        (Face::Regular, chrome.px(13.0).max(10) as f32)
    }
}

/// Paint the launch splash with Graphite ground, copy and chrome typography.
/// The established monospace art painter keeps its flare placement and
/// animation, while Graphite tokens make the fill readable in both variants.
#[allow(clippy::too_many_arguments)]
pub(crate) fn splash(
    font: &FontMetrics,
    chrome: ChromeGeom,
    variant: ThemeVariant,
    page: crate::splash::Page,
    lines: &[Vec<crate::splash::Span>],
    animation_ms: Option<u64>,
    surface: OverlaySurface,
    buffer: &mut [u32],
    stride: usize,
    height: usize,
    focus_rgb: [u8; 3],
) {
    if stride == 0 || height == 0 || font.cell_h == 0 {
        return;
    }

    let tok = graphite::tokens(variant);
    let accent = graphite::accent(tok, focus_rgb);
    raster::paint_overlay_surface_rounded(
        buffer, stride, 0, 0, stride, height, 0.0, tok.ground, surface,
    );

    let art_rows = if page == crate::splash::Page::Main {
        prismattyc_core::splash::ART.len()
    } else {
        0
    };
    let line_widths: Vec<usize> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let (face, px) = splash_line_style(page, index, art_rows, font, chrome);
            line.iter()
                .map(|(text, _)| graphite::text_width(face, px, splash_text(text)))
                .sum::<f32>()
                .ceil() as usize
        })
        .collect();
    let block_w = line_widths.iter().copied().max().unwrap_or(0).min(stride);
    let line_h = font.cell_h.max(chrome.px(20.0));
    let block_h = line_h.saturating_mul(lines.len()).min(height);
    let x0 = stride.saturating_sub(block_w) / 2;
    let y0 = height.saturating_sub(block_h) / 2;

    for (index, line) in lines.iter().enumerate() {
        if index < art_rows {
            continue;
        }
        let row_y = y0.saturating_add(index.saturating_mul(line_h));
        if row_y >= height {
            break;
        }
        let (face, px) = splash_line_style(page, index, art_rows, font, chrome);
        let center_y = row_y as f32 + line_h as f32 / 2.0;
        let mut pen = x0 as f32;
        for (text, ink) in line {
            let text = splash_text(text);
            pen = graphite::draw_text(
                buffer,
                stride,
                pen,
                center_y,
                face,
                px,
                text,
                splash_ink(*ink, index < art_rows, variant, accent),
                x0,
                stride,
            );
        }
    }
    if art_rows > 0 {
        let art_lines: Vec<Vec<crate::splash::Span>> = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                line.iter()
                    .map(|(text, ink)| {
                        (
                            text.clone(),
                            splash_ink(*ink, index < art_rows, variant, accent),
                        )
                    })
                    .collect()
            })
            .collect();
        raster::rasterize_splash_art(font, &art_lines, buffer, stride, height, animation_ms);
    }
}

/// Paint the walkthrough caption using the same rounded Graphite surface and
/// tokens as the dialogs while retaining the classic caption's hit geometry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walkthrough_caption(
    font: &FontMetrics,
    chrome: ChromeGeom,
    variant: ThemeVariant,
    view: &crate::walkthrough::CaptionView,
    band: crate::walkthrough::CaptionBand,
    surface: OverlaySurface,
    buffer: &mut [u32],
    stride: usize,
    focus_rgb: [u8; 3],
    hovered: Option<crate::walkthrough::CaptionHit>,
) {
    if band.w == 0 || band.h == 0 || font.cell_w == 0 || font.cell_h == 0 {
        return;
    }

    let caption_opacity = f32::from(crate::walkthrough::CAPTION_ALPHA) / 255.0;
    draw_panel(
        buffer,
        stride,
        Rect::new(band.x, band.y, band.w, band.h),
        variant,
        OverlaySurface {
            opacity: surface.opacity * caption_opacity,
            blur_radius: surface.blur_radius,
        },
    );

    let tok = graphite::tokens(variant);
    let accent = graphite::accent(tok, focus_rgb);
    let px = chrome.px(12.0).max(10) as f32;
    let small_px = chrome.px(10.5).max(9) as f32;
    let pad_x = font.cell_w;
    let pad_y = font.cell_h / 4 + 1;
    let line1_y = band.y.saturating_add(pad_y);
    let line2_y = line1_y.saturating_add(font.cell_h);
    let line1_x = band.x.saturating_add(pad_x);
    let line1_right = band.dismiss.x.saturating_sub(chrome.px(8.0));
    if line1_right > line1_x {
        let caption = graphite::ellipsize(
            Face::SemiBold,
            px,
            &view.caption,
            line1_right.saturating_sub(line1_x) as f32,
        );
        draw_text(
            buffer,
            stride,
            Rect::new(line1_x, line1_y, line1_right - line1_x, font.cell_h),
            Face::SemiBold,
            px,
            &caption,
            tok.text_strong,
            line1_x,
            line1_right,
        );
    }

    paint_caption_button(
        buffer,
        stride,
        variant,
        band.dismiss,
        "×",
        crate::walkthrough::CaptionHit::Dismiss,
        hovered,
        accent,
        small_px,
    );

    let line2_right = band
        .show_me
        .or(band.skip)
        .map(|rect| rect.x.saturating_sub(font.cell_w / 2))
        .unwrap_or(band.x.saturating_add(band.w).saturating_sub(pad_x));
    if line2_right > line1_x {
        let line2 = graphite::ellipsize(
            Face::Regular,
            small_px,
            &view.line2,
            line2_right.saturating_sub(line1_x) as f32,
        );
        draw_text(
            buffer,
            stride,
            Rect::new(line1_x, line2_y, line2_right - line1_x, font.cell_h),
            Face::Regular,
            small_px,
            &line2,
            tok.muted,
            line1_x,
            line2_right,
        );
    }
    if let Some(rect) = band.show_me {
        paint_caption_button(
            buffer,
            stride,
            variant,
            rect,
            crate::walkthrough::SHOW_ME_LABEL,
            crate::walkthrough::CaptionHit::ShowMe,
            hovered,
            accent,
            small_px,
        );
    }
    if let Some(rect) = band.skip {
        paint_caption_button(
            buffer,
            stride,
            variant,
            rect,
            crate::walkthrough::SKIP_LABEL,
            crate::walkthrough::CaptionHit::Skip,
            hovered,
            accent,
            small_px,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_caption_button(
    buffer: &mut [u32],
    stride: usize,
    variant: ThemeVariant,
    rect: crate::walkthrough::CaptionRect,
    label: &str,
    target: crate::walkthrough::CaptionHit,
    hovered: Option<crate::walkthrough::CaptionHit>,
    accent: graphite::Rgb,
    px: f32,
) {
    let tok = graphite::tokens(variant);
    let rect = Rect::new(rect.x, rect.y, rect.w, rect.h);
    if hovered == Some(target) {
        graphite::fill_round_rect(buffer, stride, rect, 4.0, tok.tab_hover, 255);
    }
    let color = if target == crate::walkthrough::CaptionHit::Dismiss {
        tok.muted
    } else {
        accent
    };
    let label = graphite::ellipsize(Face::Regular, px, label, rect.w as f32);
    draw_text(
        buffer,
        stride,
        rect,
        Face::Regular,
        px,
        &label,
        color,
        rect.x,
        rect.right(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::PaletteRow;
    use crate::raster::{pack_argb, PaletteLayoutMode, PalettePointerTarget};

    fn graphite_chrome() -> ChromeGeom {
        ChromeGeom {
            graphite: true,
            scale_milli: 1_000,
        }
    }

    #[test]
    fn palette_fixed_layout_does_not_follow_match_count_and_filter_hits_align() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let chrome = graphite_chrome();
        let small = vec![PaletteRow::plain(
            "Open space".into(),
            "Open a saved space".into(),
            "⌘O".into(),
        )];
        let many: Vec<_> = (0..80)
            .map(|index| {
                PaletteRow::plain(
                    format!("Command {index}"),
                    "A command description".into(),
                    "⌘K".into(),
                )
            })
            .collect();
        let labels = ["All", "Spaces", "Windows"];
        let small_sections = [crate::raster::PaletteSection {
            header: "MATCHES",
            subtitle: "open",
            rows: &small,
        }];
        let many_sections = [crate::raster::PaletteSection {
            header: "MATCHES",
            subtitle: "open",
            rows: &many,
        }];
        let small_frame = PaletteFrame {
            layout_mode: PaletteLayoutMode::FixedHeight,
            query: Some("open"),
            chips: Some((&labels, 0)),
            sections: &small_sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter run · Esc close",
        };
        let many_frame = PaletteFrame {
            sections: &many_sections,
            ..small_frame
        };
        let mut buffer = vec![pack_argb(255, [40, 40, 40]); 1280 * 800];
        let small_layout = palette(
            &font,
            ThemeVariant::Dark,
            chrome,
            [98, 168, 255],
            &small_frame,
            OverlaySurface::default(),
            &mut buffer,
            1280,
            800,
            Some(1),
        )
        .expect("small palette fits");
        let many_layout = palette(
            &font,
            ThemeVariant::Dark,
            chrome,
            [98, 168, 255],
            &many_frame,
            OverlaySurface::default(),
            &mut buffer,
            1280,
            800,
            Some(1),
        )
        .expect("large palette fits");
        assert_eq!(
            (
                small_layout.panel_x,
                small_layout.panel_y,
                small_layout.panel_w,
                small_layout.panel_h
            ),
            (
                many_layout.panel_x,
                many_layout.panel_y,
                many_layout.panel_w,
                many_layout.panel_h
            )
        );
        let chip = many_layout.filter_chips[1];
        assert_eq!(
            raster::palette_pointer_hit(
                &many_layout,
                chip.x + chip.width / 2,
                chip.y + chip.height / 2,
            ),
            Some(PalettePointerTarget::Filter(1))
        );
        let row = many_layout.rows[0];
        assert_eq!(
            raster::palette_pointer_hit(
                &many_layout,
                many_layout.panel_x + 8,
                row.y + many_layout.geom.row_pitch_px / 2,
            ),
            Some(PalettePointerTarget::Row(0))
        );
    }

    #[test]
    fn theme_picker_geometry_is_fixed_and_scrolled_hits_are_absolute() {
        let chrome = graphite_chrome();
        let short = theme_picker_layout(chrome, 3, 0, 1280, 800).expect("dialog fits");
        let long = theme_picker_layout(chrome, 80, 20, 1280, 800).expect("dialog fits");
        assert_eq!(short.panel, long.panel);
        assert_eq!(short.list, long.list);
        assert_eq!(short.visible_rows, long.visible_rows);
        assert!(long.scroll > 0);
        assert_eq!(
            theme_picker_hit(
                chrome,
                80,
                long.scroll,
                1280,
                800,
                long.list.x + long.list.w / 2,
                long.list.y + long.row_h / 2,
            ),
            Some(ThemePickerHit::Row(long.scroll))
        );
        assert_eq!(
            theme_picker_hit(
                chrome,
                80,
                long.scroll,
                1280,
                800,
                long.close.x + long.close.w / 2,
                long.close.y + long.close.h / 2,
            ),
            Some(ThemePickerHit::Close)
        );
    }

    #[test]
    fn translucent_graphite_panel_keeps_rounded_corners_clear() {
        let background = pack_argb(255, [210, 80, 40]);
        let mut buffer = vec![background; 24 * 24];
        raster::paint_overlay_surface_rounded(
            &mut buffer,
            24,
            2,
            2,
            20,
            20,
            8.0,
            [20, 24, 30],
            OverlaySurface {
                opacity: 0.5,
                blur_radius: 0,
            },
        );
        assert_eq!(buffer[2 * 24 + 2], background, "outer corner remains clear");
        assert_ne!(buffer[12 * 24 + 12], background, "panel center is blended");
    }

    #[test]
    fn palette_and_theme_picker_render_dark_light_and_translucent_surfaces() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let chrome = graphite_chrome();
        let labels: Vec<_> = (0..30).map(|index| format!("Theme {index}")).collect();
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| ThemePickerRow {
                label,
                branch: index % 7 == 0,
            })
            .collect();
        let palette_rows = vec![PaletteRow::plain(
            "Open settings".into(),
            "Change host preferences".into(),
            "⌘,".into(),
        )];
        let sections = [crate::raster::PaletteSection {
            header: "MATCHES",
            subtitle: "settings",
            rows: &palette_rows,
        }];
        let labels = ["All", "Actions"];
        let frame = PaletteFrame {
            layout_mode: PaletteLayoutMode::FixedHeight,
            query: Some("settings"),
            chips: Some((&labels, 0)),
            sections: &sections,
            selected: 0,
            scroll: 0,
            detail: None,
            footer: "Enter run · Esc close",
        };
        let mut themes = Vec::new();
        for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
            let preview = crate::theme::builtins()
                .iter()
                .find(|theme| theme.variant == variant)
                .expect("built-in theme variant")
                .clone();
            let mut buffer = vec![pack_argb(255, [220, 32, 32]); 1280 * 800];
            let palette_layout = palette(
                &font,
                variant,
                chrome,
                [98, 168, 255],
                &frame,
                OverlaySurface {
                    opacity: 0.68,
                    blur_radius: 0,
                },
                &mut buffer,
                1280,
                800,
                Some(1),
            )
            .expect("palette render path");
            assert!(palette_layout.panel_w > 0 && palette_layout.panel_h > 0);
            let picker_layout = theme_picker(
                chrome,
                variant,
                &rows,
                Some(4),
                0,
                &preview,
                "Up/Down · Enter apply · Esc cancel",
                OverlaySurface {
                    opacity: 0.68,
                    blur_radius: 0,
                },
                &mut buffer,
                1280,
                800,
                [98, 168, 255],
                Some(4),
                false,
            )
            .expect("theme picker render path");
            let center = (picker_layout.panel.y + picker_layout.panel.h / 2) * 1280
                + picker_layout.panel.x
                + picker_layout.panel.w / 2;
            assert_ne!(buffer[center], pack_argb(255, [220, 32, 32]));
            themes.push(buffer[center]);
        }
        assert_ne!(
            themes[0], themes[1],
            "dark and light panels use their tokens"
        );
    }

    #[test]
    fn splash_uses_graphite_tokens_and_blends_the_ground() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let chrome = graphite_chrome();
        let page = crate::splash::Page::Main;
        let animation_ms = prismattyc_core::splash::SETTLED_MS + 2_000;
        let lines = crate::splash::layout_with_resume(page, "0.3.0", 0, Some(animation_ms), false);
        let (width, height) = (1100, 640);
        let backdrop = [180, 42, 76];
        let mut dark = vec![pack_argb(255, backdrop); width * height];
        splash(
            &font,
            chrome,
            ThemeVariant::Dark,
            page,
            &lines,
            Some(animation_ms),
            OverlaySurface {
                opacity: 0.72,
                blur_radius: 0,
            },
            &mut dark,
            width,
            height,
            [98, 168, 255],
        );
        assert_ne!(dark[0], pack_argb(255, backdrop));

        let mut light = vec![pack_argb(255, backdrop); width * height];
        splash(
            &font,
            chrome,
            ThemeVariant::Light,
            page,
            &lines,
            None,
            OverlaySurface::default(),
            &mut light,
            width,
            height,
            [98, 168, 255],
        );
        assert_ne!(
            dark[0], light[0],
            "Graphite variants use different ground tokens"
        );
        assert_ne!(dark, light, "splash text and background follow the variant");

        let mut later = vec![pack_argb(255, backdrop); width * height];
        splash(
            &font,
            chrome,
            ThemeVariant::Dark,
            page,
            &lines,
            Some(animation_ms + 1_500),
            OverlaySurface {
                opacity: 0.72,
                blur_radius: 0,
            },
            &mut later,
            width,
            height,
            [98, 168, 255],
        );
        assert!(
            dark.iter().zip(&later).any(|(early, late)| early != late),
            "the Graphite flare responds to the splash clock"
        );
    }

    #[test]
    fn walkthrough_caption_uses_graphite_surface_and_hover_treatment() {
        let Ok(font) = FontMetrics::load(14.0) else {
            return;
        };
        let chrome = graphite_chrome();
        let view = crate::walkthrough::CaptionView {
            caption: "Open the command palette".into(),
            line2: "Ctrl+Shift+P".into(),
            show_me: true,
            skip: true,
        };
        let band = crate::walkthrough::caption_band(
            (100, 100, 900, 500),
            font.cell_w,
            font.cell_h,
            chrome.px(8.0),
            &view,
        )
        .expect("caption band fits");
        let (width, height) = (1200, 800);
        let backdrop = pack_argb(255, [180, 42, 76]);
        let mut idle = vec![backdrop; width * height];
        walkthrough_caption(
            &font,
            chrome,
            ThemeVariant::Dark,
            &view,
            band,
            OverlaySurface {
                opacity: 0.76,
                blur_radius: 0,
            },
            &mut idle,
            width,
            [98, 168, 255],
            None,
        );
        let panel_center = (band.y + band.h / 2) * width + band.x + band.w / 2;
        let surface_opacity = 0.76 * f32::from(crate::walkthrough::CAPTION_ALPHA) / 255.0;
        let shadow_rgb = raster::mix_rgb(
            [180, 42, 76],
            [0, 0, 0],
            raster::opacity_to_weight(surface_opacity * 0.22),
        );
        let expected_panel = raster::mix_rgb(
            shadow_rgb,
            panel_color(ThemeVariant::Dark),
            raster::opacity_to_weight(surface_opacity),
        );
        assert_eq!(
            idle[panel_center],
            pack_argb(255, expected_panel),
            "the translucent caption blends over its existing surface"
        );

        let mut hovered = vec![backdrop; width * height];
        walkthrough_caption(
            &font,
            chrome,
            ThemeVariant::Dark,
            &view,
            band,
            OverlaySurface {
                opacity: 0.76,
                blur_radius: 0,
            },
            &mut hovered,
            width,
            [98, 168, 255],
            Some(crate::walkthrough::CaptionHit::Skip),
        );
        assert_ne!(
            idle, hovered,
            "hovering a clickable caption control is visible"
        );

        let mut light = vec![backdrop; width * height];
        walkthrough_caption(
            &font,
            chrome,
            ThemeVariant::Light,
            &view,
            band,
            OverlaySurface::default(),
            &mut light,
            width,
            [98, 168, 255],
            None,
        );
        assert_ne!(
            idle, light,
            "caption panel and text follow dark/light tokens"
        );
    }
}
