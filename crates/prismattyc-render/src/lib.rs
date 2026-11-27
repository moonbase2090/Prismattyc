//! Classic cell-grid rendering for Prismattyc.

use std::io::{self, Write};

use prismattyc_core::{CellRange, Color, Screen, Style};

/// Deterministic renderer used by tests and non-interactive consumers.
pub struct PlainTextRenderer;

impl PlainTextRenderer {
    /// Classic fast path: stream characters from the grid with no extra buffers.
    pub fn render(screen: &Screen) -> String {
        let mut output = String::new();
        for row in 0..screen.rows() {
            if row > 0 {
                output.push('\n');
            }
            for cell in screen.view_row(row).expect("row is inside screen bounds") {
                cell.write_grapheme_into(&mut output);
            }
        }
        output
    }
}

/// Full-grid ANSI renderer for the interactive classic path.
pub struct AnsiRenderer<W> {
    writer: W,
}

impl<W: Write> AnsiRenderer<W> {
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Paint the live grid with no selection and place the caret.
    pub fn render(&mut self, screen: &Screen) -> io::Result<()> {
        self.render_scrolled(screen, 0, true, None)
    }

    /// Paint the viewport from history: `scroll_offset` rows above the live
    /// bottom. Offset 0 is the live grid. When `place_cursor` is false (host is
    /// scrolled up), the caret is parked at (1,1) after paint — caller should keep
    /// the host caret hidden. Optional `selection` is viewport-coordinate chrome
    /// (including history view).
    pub fn render_scrolled(
        &mut self,
        screen: &Screen,
        scroll_offset: usize,
        place_cursor: bool,
        selection: Option<CellRange>,
    ) -> io::Result<()> {
        self.writer.write_all(b"\x1b[H")?;
        let mut active_style = None;
        for row_index in 0..screen.rows() {
            if row_index > 0 {
                write!(self.writer, "\x1b[{};1H", row_index + 1)?;
            }
            for col in 0..screen.columns() {
                let cell = screen.view_cell(scroll_offset, row_index, col);
                // wide text: continuation half is not painted; the lead wide glyph
                // already advanced the outer host by two columns.
                if cell.wide_cont {
                    continue;
                }
                let mut style = cell.style;
                if selection.is_some_and(|r| {
                    // Host stores absolute history rows (scrollback-inclusive select).
                    screen.selection_covers_abs_at_view(scroll_offset, r, row_index, col)
                }) {
                    style.inverse = !style.inverse;
                }
                if active_style != Some(style) {
                    write_style(&mut self.writer, style)?;
                    active_style = Some(style);
                }
                // Base + combining; sanitize only the base scalar for C0 safety.
                write!(self.writer, "{}", sanitize_paint_char(cell.character))?;
                for &mark in cell.combining_marks() {
                    write!(self.writer, "{mark}")?;
                }
            }
        }
        if place_cursor && scroll_offset == 0 {
            let cursor = screen.cursor();
            write!(
                self.writer,
                "\x1b[0m\x1b[{};{}H",
                cursor.row + 1,
                cursor.column + 1
            )?;
        } else {
            // Scrolled away from live bottom: no live caret in the history view.
            write!(self.writer, "\x1b[0m\x1b[1;1H")?;
        }
        self.writer.flush()
    }
}

/// Defense in depth: never emit raw C0/C1 control scalars from the
/// host paint path (would confuse the outer terminal). Keep tab/LF/CR as space
/// so geometry stays one cell wide; printable and other Unicode pass through.
fn sanitize_paint_char(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' => ' ',
        c if c.is_control() => ' ',
        c => c,
    }
}

fn write_style(writer: &mut impl Write, style: Style) -> io::Result<()> {
    writer.write_all(b"\x1b[0m")?;
    if style.bold {
        writer.write_all(b"\x1b[1m")?;
    }
    if style.italic {
        writer.write_all(b"\x1b[3m")?;
    }
    if style.underline {
        writer.write_all(b"\x1b[4m")?;
    }
    if style.inverse {
        writer.write_all(b"\x1b[7m")?;
    }
    write_color(writer, style.foreground, true)?;
    write_color(writer, style.background, false)
}

fn write_color(writer: &mut impl Write, color: Color, foreground: bool) -> io::Result<()> {
    match color {
        Color::Default => Ok(()),
        Color::Ansi(index) => {
            let code = match (foreground, index < 8) {
                (true, true) => 30 + index,
                (true, false) => 90 + index - 8,
                (false, true) => 40 + index,
                (false, false) => 100 + index - 8,
            };
            write!(writer, "\x1b[{code}m")
        }
        Color::Indexed(index) => {
            let ground = if foreground { 38 } else { 48 };
            write!(writer, "\x1b[{ground};5;{index}m")
        }
        Color::Rgb { r, g, b } => {
            let ground = if foreground { 38 } else { 48 };
            write!(writer, "\x1b[{ground};2;{r};{g};{b}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_preserves_grid_shape() {
        let mut screen = Screen::new(3, 2, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        assert_eq!(PlainTextRenderer::render(&screen), "abc\nd  ");
    }

    #[test]
    fn sanitize_paint_char_maps_c0_to_space() {
        assert_eq!(sanitize_paint_char('\u{0007}'), ' '); // BEL
        assert_eq!(sanitize_paint_char('\n'), ' ');
        assert_eq!(sanitize_paint_char('\t'), ' ');
        assert_eq!(sanitize_paint_char('A'), 'A');
        assert_eq!(sanitize_paint_char('中'), '中');
    }

    #[test]
    fn ansi_render_does_not_emit_raw_bel_from_cell() {
        let mut screen = Screen::new(2, 1, 0);
        // Grid can hold a control scalar (defense path if feed ever stores one).
        screen.put_char('\u{0007}');
        let mut out = Vec::new();
        AnsiRenderer::new(&mut out).render(&screen).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(
            !s.contains('\u{0007}'),
            "host paint must not emit raw BEL: {s:?}"
        );
    }

    #[test]
    fn ansi_renderer_emits_style_and_restores_cursor() {
        let mut screen = Screen::new(2, 1, 0);
        screen.set_style(Style {
            bold: true,
            foreground: Color::Ansi(1),
            ..Style::default()
        });
        screen.put_char('X');
        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes).render(&screen).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("\x1b[1m\x1b[31mX"));
        assert!(rendered.ends_with("\x1b[0m\x1b[1;2H"));
    }

    #[test]
    fn ansi_renderer_emits_indexed_and_rgb() {
        let mut screen = Screen::new(2, 1, 0);
        screen.set_style(Style {
            foreground: Color::Indexed(196),
            background: Color::Rgb { r: 1, g: 2, b: 3 },
            ..Style::default()
        });
        screen.put_char('Z');
        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes).render(&screen).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("\x1b[38;5;196m"), "indexed: {rendered:?}");
        assert!(rendered.contains("\x1b[48;2;1;2;3m"), "rgb: {rendered:?}");
    }

    // / F6: the selection paint must emit inverse (CSI 7) for selected cells.
    #[test]
    fn selection_selection_emits_inverse_sgr() {
        let mut screen = Screen::new(4, 1, 0);
        for character in "abcd".chars() {
            screen.put_char(character);
        }
        let selection = CellRange {
            start_row: 0,
            start_col: 1,
            end_row: 0,
            end_col: 2,
        };

        let mut without = Vec::new();
        AnsiRenderer::new(&mut without).render(&screen).unwrap();
        let without = String::from_utf8(without).unwrap();

        let mut with = Vec::new();
        AnsiRenderer::new(&mut with)
            .render_scrolled(&screen, 0, true, Some(selection))
            .unwrap();
        let with = String::from_utf8(with).unwrap();

        assert!(
            !without.contains("\x1b[7m"),
            "no selection must not emit inverse: {without:?}"
        );
        // write_style path: reset + CSI 7 for the selected span "bc".
        assert!(
            with.contains("\x1b[7m"),
            "selection must emit CSI 7 inverse via write_style: {with:?}"
        );
        assert!(
            with.contains("\x1b[7mbc") || with.contains("\x1b[0m\x1b[7mbc"),
            "selected cells must paint under inverse: {with:?}"
        );
        assert_ne!(without, with, "selection must change the wire stream");
    }

    /// selection inverse XOR-toggles cells that already carry inverse SGR.
    #[test]
    fn selection_selection_xors_existing_inverse() {
        let mut screen = Screen::new(3, 1, 0);
        screen.set_style(Style {
            inverse: true,
            ..Style::default()
        });
        screen.put_char('A');
        screen.set_style(Style::default());
        screen.put_char('B');
        screen.put_char('C');

        let selection = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1, // A (already inverse) and B (plain)
        };

        let mut without = Vec::new();
        AnsiRenderer::new(&mut without).render(&screen).unwrap();
        let without = String::from_utf8(without).unwrap();

        let mut with = Vec::new();
        AnsiRenderer::new(&mut with)
            .render_scrolled(&screen, 0, true, Some(selection))
            .unwrap();
        let with = String::from_utf8(with).unwrap();

        assert_ne!(
            without, with,
            "XOR selection must differ from unselected paint"
        );
        // Storage inverse on A → CSI 7 without selection.
        assert!(
            without.contains("\x1b[7mA"),
            "pre-styled inverse A on wire: {without:?}"
        );
        // Selected A: inverse XOR → not inverse (no CSI 7 immediately before A).
        assert!(
            !with.contains("\x1b[7mA"),
            "selection must XOR-clear inverse on already-inverse A: {with:?}"
        );
        // Selected B: plain XOR → inverse on wire.
        assert!(
            with.contains("\x1b[7mB"),
            "selection must inverse plain B: {with:?}"
        );
    }

    // / F6: multi-row visual trim — pure trailing spaces get no inverse paint.
    #[test]
    fn selection_multi_row_selection_skips_trailing_space_inverse() {
        let mut screen = Screen::new(8, 2, 0);
        for character in "ab".chars() {
            screen.put_char(character);
        }
        screen.carriage_return();
        screen.line_feed();
        for character in "cd".chars() {
            screen.put_char(character);
        }
        let range = CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 7,
        };

        // Geometry includes blanks; visual paint (selection_covers_cell) does not.
        assert!(range.contains(0, 5));
        assert!(!screen.selection_covers_cell(range, 0, 5));
        assert!(screen.selection_covers_cell(range, 0, 0));
        assert!(screen.selection_covers_cell(range, 0, 1));
        assert!(!screen.selection_covers_cell(range, 1, 4));

        let mut bytes = Vec::new();
        AnsiRenderer::new(&mut bytes)
            .render_scrolled(&screen, 0, true, Some(range))
            .unwrap();
        let rendered = String::from_utf8(bytes).unwrap();

        assert!(
            rendered.contains("\x1b[7m"),
            "covered cells still get inverse: {rendered:?}"
        );
        // Correct trim: inverse only "ab", then SGR reset before trailing spaces.
        // Wrong (stream contains): inverse stays through "ab      ".
        assert!(
            rendered.contains("\x1b[7mab\x1b[0m"),
            "trailing spaces must drop inverse after covered text: {rendered:?}"
        );
        assert!(
            !rendered.contains("\x1b[7mab "),
            "must not keep CSI 7 active over pure trailing spaces: {rendered:?}"
        );
        assert!(
            rendered.contains("\x1b[7mcd\x1b[0m"),
            "row 1 covered text inverse + reset: {rendered:?}"
        );
        assert!(
            !rendered.contains("\x1b[7mcd "),
            "row 1 trailing spaces must not stay inverse: {rendered:?}"
        );
    }
}
