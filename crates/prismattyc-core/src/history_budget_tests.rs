use super::*;

fn line(screen: &mut Screen, text: &str) {
    for ch in text.chars() {
        screen.put_char(ch);
    }
    screen.carriage_return();
    screen.line_feed();
}

fn row_charge(columns: usize) -> usize {
    columns * std::mem::size_of::<Cell>()
        + std::mem::size_of::<Vec<Cell>>()
        + std::mem::size_of::<bool>()
}

fn check_accounting(screen: &Screen) {
    let cells: usize = screen
        .scrollback()
        .iter()
        .map(|row| {
            assert_eq!(row.capacity(), screen.columns);
            row.capacity() * std::mem::size_of::<Cell>()
        })
        .sum();
    let metadata = screen.scrollback.capacity() * std::mem::size_of::<Vec<Cell>>()
        + screen.scrollback_wrapped.capacity();
    assert_eq!(screen.scrollback_bytes(), cells + metadata);
    assert!(screen.scrollback_bytes() <= screen.scrollback_byte_budget());
    assert!(screen.history_len() <= screen.max_scrollback);
    assert_eq!(screen.scrollback.len(), screen.scrollback_wrapped.len());
}

#[test]
fn budget_keeps_complete_newest_rows_and_warns_on_first_eviction() {
    let mut screen = Screen::new(8, 1, 100);
    screen.set_scrollback_byte_budget(3 * row_charge(8));
    for text in ["one", "two", "three"] {
        line(&mut screen, text);
        check_accounting(&screen);
    }
    assert_eq!(screen.history_len(), 3);
    assert!(!screen.scrollback_budget_warned);
    line(&mut screen, "four");
    assert_eq!(screen.history_line_text(0), "two");
    assert_eq!(screen.history_line_text(2), "four");
    assert!(screen.scrollback_budget_warned);
    line(&mut screen, "five");
    assert_eq!(screen.history_line_text(0), "three");
    check_accounting(&screen);
    screen.set_scrollback_byte_budget(row_charge(8) - 1);
    assert_eq!(screen.history_len(), 0);
    assert_eq!(screen.scrollback_bytes(), 0);
    screen.set_scrollback_byte_budget(0);
    line(&mut screen, "six");
    assert_eq!(screen.history_len(), 0);
    check_accounting(&screen);
}

#[test]
fn budget_diagnostic_is_emitted_once_by_a_real_child() {
    const CHILD_FLAG: &str = "PRISMATTYC_BUDGET_NOTICE_TEST_CHILD";
    if std::env::var_os(CHILD_FLAG).is_some() {
        let mut screen = Screen::new(8, 1, 10);
        screen.set_scrollback_byte_budget(row_charge(8));
        for text in ["a", "b", "c", "d"] {
            line(&mut screen, text);
        }
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "history_budget_tests::budget_diagnostic_is_emitted_once_by_a_real_child",
            "--nocapture",
        ])
        .env(CHILD_FLAG, "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.matches("pmux: scrollback byte budget").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("before the 10-row limit"));
}

#[test]
fn cluster_storage_is_reported_separately_and_reclaimed_after_clear() {
    let mut screen = Screen::new(8, 1, 10);
    let rows_before = screen.scrollback_bytes();
    assert_eq!(screen.cluster_storage_payload_bytes(), 0);
    line(&mut screen, "e\u{0301}");
    let cluster_bytes = screen.cluster_storage_payload_bytes();
    assert!(cluster_bytes >= std::mem::size_of::<char>());
    assert_eq!(
        screen.scrollback_bytes() - rows_before,
        8 * std::mem::size_of::<Cell>()
    );
    for _ in 0..20 {
        line(&mut screen, "e\u{0301}");
    }
    assert_eq!(screen.cluster_storage_payload_bytes(), cluster_bytes);
    screen.erase_display(3);
    assert_eq!(screen.cluster_storage_payload_bytes(), 0);
    check_accounting(&screen);
}

#[test]
fn ordinary_widths_keep_ten_thousand_rows_under_the_default() {
    for columns in [45, 92, 189, 250] {
        let mut screen = Screen::new(columns, 1, 10_000);
        for _ in 0..10_001 {
            screen.line_feed();
        }
        assert_eq!(screen.history_len(), 10_000);
        assert!(!screen.scrollback_budget_warned);
        assert_eq!(screen.scrollback_byte_budget(), 96 * 1024 * 1024);
        check_accounting(&screen);
    }
}

#[test]
fn row_cap_still_binds_and_budget_policy_survives_round_trip() {
    let mut screen = Screen::new(8, 1, 2);
    screen.set_scrollback_byte_budget(4 * row_charge(8));
    for text in ["one", "two", "three"] {
        line(&mut screen, text);
    }
    assert_eq!(screen.history_len(), 2);
    assert!(!screen.scrollback_budget_warned);
    let restored = Screen::import_state(screen.export_state()).unwrap();
    assert_eq!(screen, restored);
    assert!(!restored.scrollback_budget_warned);
    assert_eq!(restored.scrollback_byte_budget(), 4 * row_charge(8));
    check_accounting(&restored);
    let mut changed = screen.clone();
    changed.set_scrollback_byte_budget(5 * row_charge(8));
    assert_ne!(screen, changed);
    for _ in 0..8 {
        changed.line_feed();
    }
    check_accounting(&changed);
}

#[test]
fn growing_width_evicts_oldest_rows_and_shrinking_releases_capacity() {
    let mut screen = Screen::new(4, 1, 100);
    screen.set_scrollback_byte_budget(4 * row_charge(4));
    for text in ["a", "b", "c", "d"] {
        line(&mut screen, text);
    }
    screen.resize(8, 1);
    assert_eq!(screen.history_len(), 2);
    assert_eq!(screen.history_line_text(0), "c");
    assert_eq!(screen.history_line_text(1), "d");
    check_accounting(&screen);
    let wide_bytes = screen.scrollback_bytes();
    screen.resize(2, 3);
    assert_eq!(screen.history_len(), 2);
    assert!(screen.scrollback_bytes() < wide_bytes);
    check_accounting(&screen);
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
}

#[test]
fn import_budget_preserves_newest_unicode_links_and_wrap_flags() {
    let mut source = Screen::new(16, 1, 20);
    source.set_hyperlink(Some("history"), "https://example.com/history");
    for text in ["old", "e\u{0301}", "🇺🇸", "👩\u{200d}💻"] {
        line(&mut source, text);
    }
    let mut state = source.export_state();
    state.scrollback[3].wrapped = true;
    state.max_scrollback_bytes = (2 * row_charge(16)) as u64;
    let mut restored = Screen::import_state(state).unwrap();
    assert_eq!(restored.history_len(), 2);
    assert_eq!(restored.history_line_text(0), "🇺🇸");
    assert_eq!(restored.history_line_text(1), "👩\u{200d}💻");
    assert!(restored.history_line_wrapped(1));
    assert_eq!(
        restored.hyperlink_uri_at_view(2, 0, 0),
        Some("https://example.com/history")
    );
    assert!(restored.scrollback_budget_warned);
    check_accounting(&restored);
    assert_eq!(
        restored,
        Screen::import_state(restored.export_state()).unwrap()
    );
    restored.erase_display(3);
    assert_eq!(restored.scrollback_bytes(), 0);
    line(&mut restored, "again");
    check_accounting(&restored);
}

#[test]
fn narrow_then_wide_resize_preserves_full_clusters_in_every_buffer() {
    let mut screen = Screen::new(8, 1, 20);
    screen.set_style(Style {
        bold: true,
        ..Style::default()
    });
    screen.set_hyperlink(None, "https://example.com/resize");
    for text in ["中", "🇺🇸", "👩\u{200d}💻", "e\u{0300}\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}\u{0306}\u{0307}\u{0308}\u{0309}\u{030a}\u{030b}"] {
        line(&mut screen, text);
    }
    screen.put_char('中');
    screen.enter_alt_screen(AltScreenMode::Mode47);
    screen.put_char('中');
    screen.resize(1, 1);
    assert_eq!(screen, Screen::import_state(screen.export_state()).unwrap());
    screen.resize(16, 2);
    let state = screen.export_state();
    let restored = Screen::import_state(state.clone()).unwrap();
    assert_eq!(screen, restored);
    assert!(state.scrollback[3]
        .text
        .starts_with("e\u{0300}\u{0301}\u{0302}"));
    assert_eq!(
        screen.clusters.get(screen.scrollback[3][0].cluster).len(),
        12
    );
    assert_eq!(
        screen.view_cell(0, 0, 1).hyperlink_id(),
        screen.view_cell(0, 0, 0).hyperlink_id()
    );
    assert!(screen.view_cell(0, 0, 1).wide_cont);
    assert!(screen.view_cell(0, 0, 1).style.bold);
    check_accounting(&screen);
}

#[test]
fn import_rejects_invalid_text_even_when_that_old_row_would_be_evicted() {
    let mut screen = Screen::new(8, 1, 10);
    for text in ["old", "new"] {
        line(&mut screen, text);
    }
    let mut state = screen.export_state();
    state.max_scrollback_bytes = row_charge(8) as u64;
    state.scrollback[0].text = "\0".into();
    assert!(Screen::import_state(state).is_err());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic]
fn accounting_rejects_a_short_retained_row_in_debug_builds() {
    let mut screen = Screen::new(8, 1, 10);
    line(&mut screen, "row");
    screen.scrollback.front_mut().unwrap().pop();
    screen.scrollback_bytes();
}

#[test]
fn lowering_budget_invalidates_history_views_and_reclaims_evicted_tails() {
    let mut screen = Screen::new(8, 1, 100);
    for text in ["e\u{0301}", "two", "three"] {
        line(&mut screen, text);
    }
    let epoch = screen.content_epoch();
    assert!(screen.cluster_storage_payload_bytes() > 0);
    screen.set_scrollback_byte_budget(2 * row_charge(8));
    assert_eq!(screen.history_line_text(0), "two");
    assert_eq!(screen.history_len(), 2);
    assert!(screen.content_epoch() > epoch);
    assert!(screen.scrollback_budget_warned);
    assert_eq!(screen.cluster_storage_payload_bytes(), 0);
    check_accounting(&screen);

    // Reapplying the same policy does not invalidate an unchanged view.
    let epoch = screen.content_epoch();
    screen.set_scrollback_byte_budget(2 * row_charge(8));
    assert_eq!(screen.content_epoch(), epoch);
    assert_eq!(screen.history_line_text(0), "two");
}

#[test]
fn enforcement_counts_spare_metadata_and_releases_an_oversized_empty_deque() {
    let mut screen = Screen::new(8, 1, 3);
    for text in ["one", "two", "three"] {
        line(&mut screen, text);
    }
    // Exercise enforcement with existing allocations, without reconfiguration.
    // Spare metadata means two rows cannot fit this nominal two-row budget.
    screen.max_scrollback_bytes = 2 * row_charge(8);
    screen.enforce_scrollback_budget();
    assert_eq!(screen.history_len(), 1);
    assert_eq!(screen.history_line_text(0), "three");
    assert!(screen.scrollback_budget_warned);
    check_accounting(&screen);

    let mut spare = Screen::new(8, 1, 100);
    line(&mut spare, "row");
    spare.max_scrollback_bytes = row_charge(8);
    spare.enforce_scrollback_budget();
    assert_eq!(spare.history_len(), 0);
    assert_eq!(spare.scrollback_bytes(), 0);
    assert_eq!(spare.scrollback.capacity(), 0);
    assert_eq!(spare.scrollback_wrapped.capacity(), 0);
    check_accounting(&spare);
}

#[test]
fn cluster_payload_estimate_covers_entries_lookup_keys_and_import_handles() {
    let mut source = ClusterStore::default();
    let handles: Vec<_> = (0..16)
        .map(|i| source.intern(&[char::from_u32(0x0300 + i).unwrap()]))
        .collect();
    // Each logical tail needs an entry and a lookup key. Allow spare capacity,
    // but reject an estimate that multiplies the independent allocations.
    let minimum = handles.len()
        * (std::mem::size_of::<clusters::Cluster>()
            + std::mem::size_of::<(clusters::Cluster, u32)>());
    assert!((minimum..=4 * minimum).contains(&source.payload_bytes()));

    // The clone already owns these tails. Replication only adds handle-cache
    // storage, and repeating it must not keep growing the reported payload.
    let mut replica = source.clone();
    let before = replica.payload_bytes();
    for &handle in &handles {
        replica.import(&source, handle);
    }
    let after = replica.payload_bytes();
    let handle_bytes = handles.len() * std::mem::size_of::<u32>();
    assert!((before + handle_bytes..=before + 4 * handle_bytes).contains(&after));
    for &handle in &handles {
        replica.import(&source, handle);
    }
    assert_eq!(replica.payload_bytes(), after);
}
