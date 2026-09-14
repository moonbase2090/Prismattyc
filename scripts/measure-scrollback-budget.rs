// Compile against the candidate release core. Run outside host RSS measurement.
// Print retained rows and shared cluster payload separately.
use prismattyc_core::Screen;

fn emit(screen: &mut Screen, text: &str) {
    for ch in text.chars() {
        screen.put_char(ch);
    }
    screen.carriage_return();
    screen.line_feed();
}

fn report(label: &str, screen: &Screen) {
    println!(
        "label={label} columns={} history_rows={} row_allocation_bytes={} cluster_payload_bytes={}",
        screen.columns(), screen.history_len(), screen.scrollback_bytes(),
        screen.cluster_storage_payload_bytes(),
    );
}

fn main() {
    for columns in [45, 92, 189, 250, 1000] {
        let mut screen = Screen::new(columns, 1, 10_000);
        for _ in 0..10_050 {
            emit(&mut screen, "1234567890");
        }
        report("ascii", &screen);
        assert!(screen.scrollback_bytes() <= screen.scrollback_byte_budget());
        if columns <= 250 {
            assert_eq!(screen.history_len(), 10_000);
        }
        for _ in 0..10_050 {
            emit(&mut screen, "e\u{301}");
        }
        report("repeated_tail", &screen);
        screen.erase_display(3);
        report("cleared", &screen);
        assert_eq!(screen.scrollback_bytes(), 0);
        assert_eq!(screen.cluster_storage_payload_bytes(), 0);
    }
    let mut screen = Screen::new(45, 1, 10_000);
    for i in 0..20_000 {
        screen.put_char('e');
        screen.put_char(char::from_u32(0x300 + i % 16).unwrap());
        screen.put_char(char::from_u32(0x300 + (i / 16) % 16).unwrap());
        screen.put_char(char::from_u32(0x300 + (i / 256) % 16).unwrap());
        screen.put_char(char::from_u32(0x300 + (i / 4096) % 16).unwrap());
        screen.carriage_return();
        screen.line_feed();
    }
    report("distinct_tails", &screen);
    for _ in 0..30_000 {
        emit(&mut screen, "ascii");
    }
    report("distinct_then_ascii", &screen);
    screen.erase_display(3);
    report("distinct_cleared", &screen);
    assert_eq!(screen.cluster_storage_payload_bytes(), 0);
}
