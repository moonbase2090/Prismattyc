// SPDX-License-Identifier: MIT
//! The Restart to update dialog drawn with lunatui widgets.
//!
//! The widgets render into a lunatui cell grid: a rounded `Block` for the
//! panel, rows written through `RenderCx`, and an `Accordion` section for the
//! session list behind Show and Hide. The app then paints that grid over its
//! window with its own cell font, so the dialog uses the app's colours.
use lunatui_core::{Attrs, ClusterTable, Color, DecodedGlyph, Grid, Rect, Style};
use lunatui_theme::Theme;
use lunatui_widgets::{Accordion, AccordionSection, Block, BorderType, RenderCx, Widget};

/// Colours for the dialog, taken from the app's theme.
#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub panel: [u8; 3],
    pub border: [u8; 3],
    pub text: [u8; 3],
    pub strong: [u8; 3],
    pub muted: [u8; 3],
    pub accent: [u8; 3],
    pub selected: [u8; 3],
}

/// What the dialog shows.
pub(super) struct View<'a> {
    pub title: &'a str,
    pub version: &'a str,
    pub explanation: &'a str,
    pub options: &'a [(&'a str, &'a str)],
    pub selected: usize,
    pub summary: &'a str,
    pub toggle: Option<&'a str>,
    pub expanded: bool,
    pub sessions: &'a [String],
    pub keys: &'a str,
}

/// The rendered grid and where its choices and toggle sit, in cells.
pub(super) struct Layout {
    pub grid: Grid,
    pub clusters: ClusterTable,
    pub options: Vec<Rect>,
    pub toggle: Option<Rect>,
}

fn rgb(c: [u8; 3]) -> Color {
    Color::Rgb(c[0], c[1], c[2])
}

/// Split `text` into lines of at most `max` display columns.
pub(super) fn wrap(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let wide = lunatui_core::width(&line) + 1 + lunatui_core::width(word);
        if !line.is_empty() && wide > max {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Lay names out left to right, two columns apart, wrapping at `max`.
pub(super) fn flow(names: &[String], max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for name in names {
        match lines.last_mut() {
            Some(line) if lunatui_core::width(line) + 2 + lunatui_core::width(name) <= max => {
                line.push_str("  ");
                line.push_str(name);
            }
            _ => lines.push(name.clone()),
        }
    }
    lines
}

/// Render the dialog into a grid `width` columns wide.
pub(super) fn layout(view: &View<'_>, width: u16, palette: Palette) -> Layout {
    let width = width.max(24);
    // Border, then one column of padding on each side.
    let inner = usize::from(width).saturating_sub(4);
    let explanation = wrap(view.explanation, inner);
    let names = if view.expanded {
        flow(view.sessions, inner.saturating_sub(2))
    } else {
        vec![]
    };
    // Border, padding, title, explanation, gap, rule, gap, choices with a
    // blank row between them, gap, rule, gap, footer and names, gap, key
    // hints, padding, border.
    let choices = (2 * view.options.len()).saturating_sub(1);
    let rows = 13 + explanation.len() + choices + names.len();
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    let mut grid = Grid::new(width, height);
    let mut clusters = ClusterTable::new();
    let theme = Theme::builtin(lunatui_theme::Variant::Dark);
    let mut cx = RenderCx::new(&mut grid, &mut clusters, &theme);
    let base = Style::default()
        .fg(rgb(palette.text))
        .bg(rgb(palette.panel));
    let muted = base.fg(rgb(palette.muted));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(base.fg(rgb(palette.border)))
        .style(base)
        .render(Rect::new(0, 0, width, height), &mut cx);
    let left = 2;
    let right = width.saturating_sub(2);
    let mut y = 1;
    let strong = base
        .fg(rgb(palette.strong))
        .with_attrs(Attrs::new().set_bold(true));
    cx.put_str(left, y, view.title, strong);
    let title_w = u16::try_from(lunatui_core::width(view.title)).unwrap_or(0);
    cx.put_str(left + title_w + 2, y, view.version, muted);
    y += 1;
    for line in &explanation {
        cx.put_str(left, y, line, muted);
        y += 1;
    }
    y += 1;
    let rule = "─".repeat(usize::from(width.saturating_sub(2)));
    cx.put_str(1, y, &rule, base.fg(rgb(palette.border)));
    y += 2;
    let mut options = Vec::new();
    for (index, (label, hint)) in view.options.iter().enumerate() {
        let row = Rect::new(1, y, width.saturating_sub(2), 1);
        let style = if index == view.selected {
            let fill = base.bg(rgb(palette.selected));
            cx.put_str(1, y, &" ".repeat(usize::from(row.width)), fill);
            cx.put_str(1, y, "▌", fill.fg(rgb(palette.accent)));
            fill
        } else {
            base
        };
        cx.put_str(left, y, label, style.fg(rgb(palette.strong)));
        let hint_w = u16::try_from(lunatui_core::width(hint)).unwrap_or(0);
        cx.put_str(
            right.saturating_sub(hint_w),
            y,
            hint,
            style.fg(rgb(palette.muted)),
        );
        options.push(row);
        y += 2;
    }
    cx.put_str(1, y, &rule, base.fg(rgb(palette.border)));
    y += 2;
    let mut toggle = None;
    let area = Rect::new(left, y, right.saturating_sub(left), 1 + names.len() as u16);
    match view.toggle {
        Some(label) => {
            let accordion =
                Accordion::new([AccordionSection::new(view.summary, names.len() as u16)
                    .open(view.expanded)
                    .badge(label)]);
            accordion.render(area, &mut cx);
            // The accordion's caret and badge keep its own colours; give the
            // header the dialog's muted text and the badge the link colour.
            let header = Rect::new(left, y, area.width, 1);
            let caret = if view.expanded { "▾" } else { "▸" };
            cx.put_str(left, y, caret, muted);
            cx.put_str(left + 2, y, view.summary, muted);
            let badge_w = u16::try_from(lunatui_core::width(label)).unwrap_or(0);
            cx.put_str(
                right.saturating_sub(badge_w),
                y,
                label,
                base.fg(rgb(palette.accent)),
            );
            toggle = Some(header);
            for (offset, section) in accordion.layout(area).iter().enumerate() {
                let _ = offset;
                for (line_index, line) in names.iter().enumerate() {
                    let row = section.body.y + line_index as u16;
                    if row < section.body.y + section.body.height {
                        cx.put_str(left + 2, row, line, base);
                    }
                }
            }
        }
        None => cx.put_str(left, y, view.summary, muted),
    }
    y += 1 + names.len() as u16;
    y += 1;
    cx.put_str(left, y, view.keys, muted);
    Layout {
        grid,
        clusters,
        options,
        toggle,
    }
}

/// The colour a cell asks for, with `fallback` for the terminal default.
fn resolve(color: Color, fallback: Option<[u8; 3]>) -> Option<[u8; 3]> {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        _ => fallback,
    }
}

/// Paint `layout` with its top-left cell at pixel (`x`, `y`).
#[allow(clippy::too_many_arguments)]
pub(super) fn paint(
    layout: &Layout,
    buffer: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    font: &crate::raster::FontMetrics,
    palette: Palette,
) {
    let (cw, ch) = (font.cell_w.max(1), font.cell_h.max(1));
    for row in 0..layout.grid.height() {
        for column in 0..layout.grid.width() {
            let Some(cell) = layout.grid.get(column, row) else {
                continue;
            };
            let style = cell.style();
            let px = x + usize::from(column) * cw;
            let py = y + usize::from(row) * ch;
            if let Some(bg) = resolve(style.bg, Some(palette.panel)) {
                crate::raster::fill_cell_rect(buffer, stride, px, py, cw, ch, bg);
            }
            let text = match cell.glyph().decode() {
                DecodedGlyph::Inline(' ') | DecodedGlyph::Continuation => continue,
                DecodedGlyph::Inline(c) => c.to_string(),
                DecodedGlyph::Cluster(id) => match layout.clusters.cluster_text(id) {
                    Some(text) => text.to_string(),
                    None => continue,
                },
            };
            let fg = resolve(style.fg, Some(palette.text)).unwrap_or(palette.text);
            crate::raster::draw_cell_text(buffer, stride, font, &text, px, py, fg);
            if style.attrs.bold() {
                // The cell font has no bold face here; double-strike instead.
                crate::raster::draw_cell_text(buffer, stride, font, &text, px + 1, py, fg);
            }
        }
    }
}

/// A cell rectangle in pixels, with the grid's top-left at (`x`, `y`).
pub(super) fn pixels(rect: Rect, x: usize, y: usize, cell: (usize, usize)) -> [usize; 4] {
    let (cw, ch) = (cell.0.max(1), cell.1.max(1));
    [
        x + usize::from(rect.x) * cw,
        y + usize::from(rect.y) * ch,
        usize::from(rect.width) * cw,
        usize::from(rect.height) * ch,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: Palette = Palette {
        panel: [1, 1, 1],
        border: [2, 2, 2],
        text: [3, 3, 3],
        strong: [4, 4, 4],
        muted: [5, 5, 5],
        accent: [6, 6, 6],
        selected: [7, 7, 7],
    };

    fn text_row(layout: &Layout, row: u16) -> String {
        (0..layout.grid.width())
            .filter_map(|x| match layout.grid.get(x, row)?.glyph().decode() {
                DecodedGlyph::Inline(c) => Some(c.to_string()),
                DecodedGlyph::Cluster(id) => layout.clusters.cluster_text(id).map(str::to_string),
                DecodedGlyph::Continuation => None,
            })
            .collect()
    }

    fn view<'a>(sessions: &'a [String], expanded: bool, selected: usize) -> View<'a> {
        View {
            title: "Restart to update",
            version: "0.3.69 → 0.3.70",
            explanation: "Save the session configuration to recreate your sessions after restart.",
            options: &[
                ("Save and restart", "Recreate sessions afterwards"),
                ("Restart without saving", "Sessions are discarded"),
                ("Cancel", "Nothing changes now"),
            ],
            selected,
            summary: "3 sessions · names, directories",
            toggle: Some(if expanded { "Hide" } else { "Show" }),
            expanded,
            sessions,
            keys: "Enter select · Esc close · ↑↓ move",
        }
    }

    #[test]
    fn the_panel_has_a_rounded_border_the_title_and_every_choice_with_its_hint() {
        let names: Vec<String> = vec!["api".into(), "web".into(), "docs".into()];
        let layout = layout(&view(&names, false, 0), 60, PALETTE);
        let top = text_row(&layout, 0);
        assert!(top.starts_with('╭') && top.ends_with('╮'), "{top}");
        assert!(text_row(&layout, 1).contains("Restart to update  0.3.69 → 0.3.70"));
        for (index, (label, hint)) in [
            ("Save and restart", "Recreate sessions afterwards"),
            ("Restart without saving", "Sessions are discarded"),
            ("Cancel", "Nothing changes now"),
        ]
        .iter()
        .enumerate()
        {
            let row = text_row(&layout, layout.options[index].y);
            assert!(row.contains(label), "{row}");
            assert!(row.trim_end_matches(['│', ' ']).ends_with(hint), "{row}");
        }
        let bottom = text_row(&layout, layout.grid.height() - 1);
        assert!(bottom.starts_with('╰') && bottom.ends_with('╯'), "{bottom}");
    }

    #[test]
    fn the_selected_choice_gets_the_accent_bar_and_fill() {
        let layout = layout(&view(&[], false, 1), 60, PALETTE);
        let row = layout.options[1].y;
        let bar = layout.grid.get(1, row).unwrap();
        assert!(matches!(bar.glyph().decode(), DecodedGlyph::Inline('▌')));
        assert_eq!(bar.style().fg, Color::Rgb(6, 6, 6));
        assert_eq!(
            layout.grid.get(10, row).unwrap().style().bg,
            Color::Rgb(7, 7, 7)
        );
        assert_eq!(
            layout.grid.get(10, layout.options[0].y).unwrap().style().bg,
            Color::Rgb(1, 1, 1)
        );
    }

    #[test]
    fn sessions_stay_hidden_until_the_disclosure_opens() {
        let names: Vec<String> = (0..12).map(|i| format!("session-{i}")).collect();
        let closed = layout(&view(&names, false, 0), 50, PALETTE);
        let toggle = closed.toggle.unwrap();
        let header = text_row(&closed, toggle.y);
        assert!(
            header.contains("▸ 3 sessions")
                && header.trim_end_matches(['│', ' ']).ends_with("Show"),
            "{header}"
        );
        let all: String = (0..closed.grid.height())
            .map(|r| text_row(&closed, r))
            .collect();
        assert!(!all.contains("session-0"), "names hidden while closed");
        let open = layout(&view(&names, true, 0), 50, PALETTE);
        let header = text_row(&open, open.toggle.unwrap().y);
        assert!(
            header.contains("▾ 3 sessions")
                && header.trim_end_matches(['│', ' ']).ends_with("Hide"),
            "{header}"
        );
        let all: String = (0..open.grid.height())
            .map(|r| text_row(&open, r))
            .collect();
        for name in &names {
            assert!(all.contains(name.as_str()), "{name} shown when open");
        }
        assert!(open.grid.height() > closed.grid.height());
    }

    #[test]
    fn wrapping_and_flow_respect_the_width() {
        let lines = wrap("one two three four five six seven", 10);
        assert!(lines.iter().all(|l| lunatui_core::width(l) <= 10));
        assert_eq!(lines.join(" "), "one two three four five six seven");
        let names: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        let rows = flow(&names, 12);
        assert!(rows.iter().all(|l| lunatui_core::width(l) <= 12));
        assert_eq!(rows.join("  ").split("  ").count(), 20);
    }

    #[test]
    fn cell_rects_map_to_pixels() {
        assert_eq!(
            pixels(Rect::new(2, 3, 4, 1), 100, 50, (10, 20)),
            [120, 110, 40, 20]
        );
    }
}
