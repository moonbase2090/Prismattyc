use super::*;

fn write(screen: &mut Screen, text: &str) {
    for ch in text.chars() {
        screen.put_char(ch);
    }
}

#[test]
fn narrow_then_wide_resize_keeps_snapshots_valid_in_all_buffers() {
    let mut screen = Screen::new(8, 1, 20);
    screen.set_style(Style {
        bold: true,
        ..Style::default()
    });
    screen.set_hyperlink(None, "https://example.com/resize");
    let texts = ["中", "🇺🇸", "👩\u{200d}💻", "e\u{0300}\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}\u{0306}\u{0307}\u{0308}\u{0309}\u{030a}\u{030b}"];
    for text in texts {
        write(&mut screen, text);
        screen.carriage_return();
        screen.line_feed();
    }
    screen.put_char('中');
    screen.enter_alt_screen(AltScreenMode::Mode47);
    screen.put_char('中');
    screen.resize(1, 1);
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
    screen.resize(16, 2);
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
    let lead = screen.view_cell(0, 0, 0);
    let continuation = screen.view_cell(0, 0, 1);
    assert_eq!(lead.character, '中');
    assert!(continuation.wide_cont);
    assert_eq!(continuation.style, lead.style);
    assert_eq!(continuation.hyperlink_id(), lead.hyperlink_id());
    screen.leave_alt_screen(AltScreenMode::Mode47);
    for (row, text) in texts.into_iter().enumerate() {
        assert_eq!(screen.history_line_text(row), text);
    }
    assert!(screen.view_cell(0, 0, 1).wide_cont);
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
}

#[test]
fn padding_preserves_a_wide_lead_at_a_later_column_with_style_and_link() {
    let mut screen = Screen::new(8, 2, 10);
    screen.set_style(Style {
        italic: true,
        ..Style::default()
    });
    screen.set_hyperlink(None, "https://example.com/edge");
    write(&mut screen, "abc中");
    screen.resize(4, 2);
    screen.resize(8, 2);
    assert_eq!(screen.view_cell(0, 0, 3).character, '中');
    assert!(screen.view_cell(0, 0, 4).wide_cont);
    assert!(screen.view_cell(0, 0, 4).style.italic);
    assert_eq!(
        screen.hyperlink_uri_at_view(0, 0, 4),
        Some("https://example.com/edge")
    );
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
}

#[test]
fn widening_restores_clipped_leads_on_each_grid_row() {
    let mut screen = Screen::new(8, 3, 10);
    for row in 0..3 {
        screen.set_cursor_position(row, 0);
        write(&mut screen, "abc中");
    }
    screen.resize(4, 3);
    screen.resize(8, 3);
    for row in 0..3 {
        assert_eq!(screen.view_cell(0, row, 3).character, '中');
        assert!(screen.view_cell(0, row, 4).wide_cont, "row {row}");
    }
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
}

#[test]
fn padding_does_not_extend_narrow_cells_or_existing_continuations() {
    let mut empty_source = [Cell::default(); 2];
    restore_padded_wide_pair(&mut empty_source, 0);
    assert_eq!(empty_source, [Cell::default(); 2]);
    for text in ["abcd", "ab中", "abe\u{0301}"] {
        let mut screen = Screen::new(4, 2, 10);
        write(&mut screen, text);
        screen.resize(8, 2);
        assert_eq!(*screen.view_cell(0, 0, 4), Cell::default());
        assert_eq!(screen.history_line_text(0), text);
        assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
    }
}
