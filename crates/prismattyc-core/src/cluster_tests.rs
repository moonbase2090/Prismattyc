use super::*;

fn write(screen: &mut Screen, text: &str) {
    for ch in text.chars() {
        screen.put_char(ch);
    }
}

#[test]
fn cells_remain_copy_and_shrink_to_thirty_six_bytes() {
    fn require_copy<T: Copy>() {}
    require_copy::<Cell>();
    assert_eq!(std::mem::size_of::<Cell>(), 36);
    let screen = Screen::new(80, 24, 10_000);
    assert_eq!(screen.clusters.len(), 0);
    assert!(screen
        .view_cell(0, usize::MAX, usize::MAX)
        .combining_marks()
        .is_empty());
    assert!(screen
        .history_view_cell(1, usize::MAX, usize::MAX)
        .combining_marks()
        .is_empty());
}

#[test]
fn full_tail_survives_clone_and_snapshot_without_prefix_entries() {
    let mut screen = Screen::new(8, 2, 3);
    screen.set_hyperlink(Some("cluster"), "https://example.com/cluster");
    screen.put_char('e');
    let marks: Vec<char> = (0x0300..0x0300 + MAX_COMBINING_MARKS as u32)
        .map(|c| char::from_u32(c).unwrap())
        .collect();
    for &mark in &marks {
        screen.put_char(mark);
    }
    screen.put_char('\u{036f}');
    assert_eq!(screen.view_cell(0, 0, 0).combining_marks(), marks);
    let cloned = screen.clone();
    let restored = Screen::import_state(screen.export_state()).unwrap();
    assert_eq!(screen, restored);
    assert_eq!(restored.clusters.len(), 1);
    screen.set_cursor_position(0, 0);
    write(&mut screen, "x\u{0301}");
    assert_eq!(cloned.view_cell(0, 0, 0).combining_marks(), marks);
    assert_eq!(restored.view_cell(0, 0, 0), cloned.view_cell(0, 0, 0));
    assert_eq!(
        restored.hyperlink_uri_at_view(0, 0, 0),
        Some("https://example.com/cluster")
    );
}

#[test]
fn replication_translates_handles_and_invalidates_both_store_generations() {
    let mut source = Screen::new(8, 2, 3);
    let mut replica = Screen::new(8, 2, 3);
    write(&mut source, "e\u{0301}x\u{0302}");
    write(&mut replica, "e\u{0300}");
    let damage = GridDamage::full(2, 8);
    replica.apply_damage(&source, &damage);
    assert_eq!(replica.view_cell(0, 0, 0), source.view_cell(0, 0, 0));
    assert_eq!(replica.view_cell(0, 0, 1), source.view_cell(0, 0, 1));
    // Keep a cached source handle while collecting the destination.
    replica.collect_clusters();
    replica.apply_damage(&source, &damage);
    assert_eq!(replica.view_cell(0, 0, 1), source.view_cell(0, 0, 1));
    // Remove the first source entry, so collection reassigns the other handle.
    source.set_cursor_position(0, 0);
    source.put_char('a');
    source.collect_clusters();
    replica.apply_damage(&source, &damage);
    assert_eq!(replica.view_cell(0, 0, 1).combining_marks(), &['\u{0302}']);
    // A clone can append a different tail at the same numerical index.
    let mut alternate_source = source.clone();
    source.set_cursor_position(1, 0);
    alternate_source.set_cursor_position(1, 0);
    write(&mut source, "s\u{0303}");
    write(&mut alternate_source, "s\u{0304}");
    replica.apply_damage(&source, &damage);
    assert_eq!(replica.view_cell(0, 1, 0).combining_marks(), &['\u{0303}']);
    replica.apply_damage(&alternate_source, &damage);
    assert_eq!(replica.view_cell(0, 1, 0).combining_marks(), &['\u{0304}']);
}

#[test]
fn collection_preserves_primary_alternate_history_and_snapshot_text() {
    let mut screen = Screen::new(8, 2, 3);
    for text in ["h\u{0301}", "i\u{0302}", "j\u{0303}"] {
        write(&mut screen, text);
        screen.carriage_return();
        screen.line_feed();
    }
    let primary = screen.export_state();
    screen.enter_alt_screen(AltScreenMode::Mode47);
    write(&mut screen, "a\u{0304}");
    for i in 0..5000 {
        screen.set_cursor_position(1, 0);
        screen.put_char('e');
        screen.put_char(char::from_u32(0x0300 + i / 112).unwrap());
        screen.put_char(char::from_u32(0x0300 + i % 112).unwrap());
    }
    assert!(
        screen.clusters.len() < 4096,
        "dead prefixes must be reclaimed during writes"
    );
    let before = screen.export_state();
    screen.collect_clusters();
    assert_eq!(screen.export_state(), before);
    assert_eq!(screen.clusters.len(), 5);
    assert_eq!(screen.view_cell(0, 0, 0).combining_marks(), &['\u{0304}']);
    screen.leave_alt_screen(AltScreenMode::Mode47);
    let after = screen.export_state();
    assert_eq!(after.primary.rows, primary.primary.rows);
    assert_eq!(after.scrollback, primary.scrollback);
    assert_eq!(screen, Screen::import_state(after).unwrap());
}

#[test]
fn shrinking_and_clearing_release_unreachable_tails() {
    let mut screen = Screen::new(8, 2, 3);
    write(&mut screen, "a\u{0301}b\u{0302}c\u{0303}");
    screen.resize(1, 1);
    assert_eq!(screen.clusters.len(), 1);
    assert_eq!(screen.view_cell(0, 0, 0).combining_marks(), &['\u{0301}']);
    screen.erase_display(3);
    assert_eq!(screen.clusters.len(), 0);
    assert!(screen.view_cell(0, 0, 0).combining_marks().is_empty());
}

#[test]
fn ascii_scroll_bounds_a_large_evicted_unicode_store() {
    let mut screen = Screen::new(2, 1, 5000);
    for i in 0..4500 {
        screen.put_char('e');
        screen.put_char(char::from_u32(0x0300 + i / 112).unwrap());
        screen.put_char(char::from_u32(0x0300 + i % 112).unwrap());
        screen.carriage_return();
        screen.line_feed();
    }
    assert!(screen.clusters.len() > 4096);
    for _ in 0..10_100 {
        screen.line_feed();
    }
    assert!(
        screen.clusters.len() <= 4096,
        "only the bounded small cache may remain"
    );
    assert_eq!(screen.history_len(), 5000);
    assert!(screen.history_line_text(0).is_empty());
}

#[test]
fn equality_distinguishes_text_and_attributes_across_owners() {
    let mut original = Screen::new(8, 2, 3);
    write(&mut original, "e\u{0301}");
    let mut other = Screen::new(8, 2, 3);
    write(&mut other, "e\u{0302}");
    // Equal numerical handles in different stores must not hide different text.
    assert_eq!(original.row(0).unwrap()[0], other.row(0).unwrap()[0]);
    assert_ne!(original.view_cell(0, 0, 0), other.view_cell(0, 0, 0));
    assert_ne!(original, other);

    for (text, bold, link) in [
        ("x\u{0301}", false, false),
        ("e\u{0301}", true, false),
        ("e\u{0301}", false, true),
    ] {
        let mut changed = Screen::new(8, 2, 3);
        changed.set_style(Style {
            bold,
            ..Style::default()
        });
        if link {
            changed.set_hyperlink(None, "https://example.com/changed");
        }
        write(&mut changed, text);
        assert_ne!(original.view_cell(0, 0, 0), changed.view_cell(0, 0, 0));
        assert_ne!(original, changed);
        assert_eq!(
            changed,
            Screen::import_state(changed.export_state()).unwrap()
        );
    }
}

#[test]
fn equality_includes_inactive_buffer_and_history_text() {
    let make = |mark: char, in_history: bool| {
        let mut screen = Screen::new(8, 2, 3);
        if !in_history {
            screen.enter_alt_screen(AltScreenMode::Mode47);
        }
        screen.put_char('e');
        screen.put_char(mark);
        if in_history {
            screen.carriage_return();
            screen.line_feed();
            screen.line_feed();
        } else {
            screen.leave_alt_screen(AltScreenMode::Mode47);
        }
        screen
    };
    for in_history in [false, true] {
        let a = make('\u{0301}', in_history);
        let b = make('\u{0302}', in_history);
        // The live view is identical. Hidden text remains part of screen state.
        assert_eq!(a.view_cell(0, 0, 0), b.view_cell(0, 0, 0));
        assert_ne!(a.export_state(), b.export_state());
        assert_ne!(a, b);
        assert_eq!(a, Screen::import_state(a.export_state()).unwrap());
    }
}

#[test]
fn shrinking_either_axis_reclaims_only_the_clipped_tails() {
    for (columns, rows, remaining) in [(1, 2, 2), (8, 1, 2)] {
        let mut screen = Screen::new(8, 2, 3);
        write(&mut screen, "a\u{0301}b\u{0302}");
        screen.set_cursor_position(1, 0);
        write(&mut screen, "c\u{0303}");
        screen.resize(columns, rows);
        assert_eq!(screen.clusters.len(), remaining);
        assert_eq!(screen.view_cell(0, 0, 0).combining_marks(), &['\u{0301}']);
        assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
    }
}

#[test]
fn overflowed_damage_still_retires_unreferenced_cluster_cache_entries() {
    let mut source = Screen::new(8, 1, 0);
    source.put_char('e');
    source.put_char('\u{301}');
    let mut replica = source.clone();
    for i in 0..5000 {
        let marks = std::array::from_fn::<_, 4, _>(|n| {
            char::from_u32(0x300 + ((i >> (4 * n)) & 15)).unwrap()
        });
        replica.clusters.intern(&marks);
    }
    replica.clusters.finish_collection();
    assert!(replica.clusters.len() > 4096);
    source.take_damage();
    for _ in 0..50_000 {
        source.line_feed();
    }
    let damage = source.take_damage();
    assert!(damage.scroll_events().is_empty());
    replica.apply_damage(&source, &damage);
    assert_eq!(replica.clusters.len(), 0);
    assert_eq!(replica.view_cell(0, 0, 0), source.view_cell(0, 0, 0));
}
