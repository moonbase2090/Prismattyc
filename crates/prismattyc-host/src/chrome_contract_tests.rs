//! Interaction contracts exercised in the existing private native window.
use super::*;
use winit::keyboard::{Key, ModifiersState, NamedKey};

pub(super) fn verify(host: &mut HostState) {
    host.preedit = Preedit::default();
    host.find = FindMode::default();
    host.theme_picker = None;
    host.session_prompt = None;
    host.space_picker = None;
    host.space_panel = None;
    host.space_rail.edit = None;
    close_context_menu(host);
    verify_menu_and_pointer(host);
    verify_rename_and_rail(host);
    verify_scrollback_navigation(host);
    verify_close_target_tab(host);
    verify_find_and_theme_modals(host);
    verify_find_alt_screen_routing(host);
    verify_accessibility_bounds(host);
}

fn center(rect: graphite::Rect) -> (usize, usize) {
    (rect.x + rect.w / 2, rect.y + rect.h / 2)
}

/// Each chrome node's accessibility frame is where a pointer hit finds that
/// same control, so a client that locates or clicks by frame lands on it.
fn verify_accessibility_bounds(host: &mut HostState) {
    host.mux
        .new_tab("/bin/sh", &["-c".into(), "sleep 60".into()])
        .unwrap();
    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    App::paint(host).unwrap();
    let size = host.window.inner_size();
    let (width, height) = (size.width as usize, size.height as usize);
    let snap = chrome_snapshot(host, None);
    let bounds = &snap.bounds;
    assert_eq!(
        bounds.window,
        Some(graphite::Rect::new(0, 0, width, height))
    );

    assert!(show_tab_strip(host), "two tabs show the strip");
    assert_eq!(bounds.tabs.len(), host.mux.tab_count());
    for (index, rect) in bounds.tabs.iter().enumerate() {
        let (x, y) = center(rect.expect("every tab fits a wide window"));
        let hit = host.mux.tab_strip_hit(x, y, width, false);
        assert!(
            matches!(hit, Some(mux::StripHit::Tab { index: i, .. }) | Some(mux::StripHit::Pane { tab: i, .. }) if i == index),
            "tab {index} frame center hit {hit:?}"
        );
    }

    let rail = host
        .space_rail
        .layout(
            host.mux.geom(),
            width,
            height,
            host.spacing.space_rail_pane_names,
        )
        .expect("the contract docks the rail");
    assert_eq!(bounds.rail.len(), 2);
    for (index, rect) in bounds.rail.iter().enumerate() {
        let (x, y) = center(rect.expect("both chips are painted"));
        assert!(
            matches!(rail.hit(x, y, 2), Some(space_rail::RailHit::Chip { index: i, .. }) if i == index),
            "chip {index} frame center hit {:?}",
            rail.hit(x, y, 2)
        );
    }

    if pane_header_shown(host) {
        assert_eq!(bounds.arrange.len(), 3);
        for (index, rect) in bounds.arrange.iter().enumerate() {
            let (x, y) = center(rect.expect("the header paints Arrange"));
            assert_eq!(arrange_hit_at(host, x, y), Some(index), "Arrange {index}");
        }
    }

    let focused = host.mux.focused_id();
    let (_, _, cells) = host
        .mux
        .panes_and_rects()
        .find(|(pane, _, _)| *pane == focused)
        .unwrap();
    let (x, y, w, h) = host.mux.geom().pane_slot_px(cells);
    assert_eq!(bounds.document, Some(graphite::Rect::new(x, y, w, h)));
    assert!(w > 0 && h > 0 && x + w <= width && y + h <= height);

    open_command_palette(host, keybind::Action::CommandPalette);
    App::paint(host).unwrap();
    let snap = chrome_snapshot(host, None);
    let layout = host.palette_layout.as_ref().unwrap();
    let panel = snap.bounds.overlay.expect("the palette panel has a frame");
    let painted: Vec<usize> = layout.rows.iter().map(|row| row.global).collect();
    assert!(!painted.is_empty());
    for index in painted {
        let rect = snap.bounds.overlay_rows[index].expect("painted rows have frames");
        let (x, y) = center(rect);
        assert!(panel.contains(x, y));
        assert_eq!(
            palette_hit(layout, x, y),
            Some(index),
            "palette row {index}"
        );
    }
    let update = a11y::tree_update(&snap);
    let framed = |id: u64| {
        update
            .nodes
            .iter()
            .find(|(node, _)| node.0 == id)
            .and_then(|(_, node)| node.bounds())
            .is_some()
    };
    assert!(framed(a11y::WINDOW_ID));
    assert!(framed(a11y::TAB_BASE));
    assert!(framed(a11y::OVERLAY_ROW_BASE));
    host.palette = None;
    host.palette_layout = None;
    App::paint(host).unwrap();
}

fn verify_find_and_theme_modals(host: &mut HostState) {
    host.find = FindMode::default();
    host.modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;
    assert!(find_key_try_open(
        host,
        &Key::Character("f".into()),
        Some(keybind::Action::Find),
        false,
    ));
    assert!(host.find.active);
    host.modifiers = ModifiersState::empty();
    assert!(find_key_while_open(host, &Key::Character("z".into())));
    assert_eq!(host.find.query, "z");
    assert!(find_key_while_open(host, &Key::Named(NamedKey::Escape)));
    assert!(!host.find.active);

    host.theme_picker = None;
    open_theme_picker(host);
    assert!(host.theme_picker.is_some());
    let original = host.theme_picker.as_ref().unwrap().original.id.clone();
    cancel_theme_picker(host);
    assert!(host.theme_picker.is_none());
    assert_eq!(host.theme.id, original);
}

fn verify_find_alt_screen_routing(host: &mut HostState) {
    host.find = FindMode::default();
    let _ = host.emulator.feed(b"\x1b[?1049h");
    assert!(host.emulator.screen().alt_active());
    host.modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;
    assert!(!handle_find_key_logical(
        host,
        &Key::Character("f".into()),
        Some(keybind::Action::Find),
        false,
    ));
    assert!(!host.find.active);

    let _ = host.emulator.feed(b"\x1b[?1049l");
    open_find_prompt(host);
    assert!(host.find.active);
    let _ = host.emulator.feed(b"\x1b[?1049h");
    assert!(!handle_find_key_logical(
        host,
        &Key::Character("x".into()),
        None,
        false,
    ));
    assert!(!host.find.active);
    assert!(!handle_find_key_logical(
        host,
        &Key::Character("f".into()),
        Some(keybind::Action::Find),
        false,
    ));
    assert!(!host.find.active);

    let _ = host.emulator.feed(b"\x1b[?1049l");
    host.find = FindMode::default();
}

fn verify_scrollback_navigation(host: &mut HostState) {
    let _ = host.emulator.feed(b"\x1b[?1049l");
    for _ in 0..host.emulator.screen().rows() + 12 {
        let _ = host.emulator.feed(b"history\r\n");
    }
    host.view_scroll = 0;
    host.left_button_down = false;
    host.keyboard_select_mode = true;
    pan_view_scroll(host, 5);
    assert_eq!(host.view_scroll, 5);
    assert!(!host.keyboard_select_mode);
    host.scroll_new_output = true;
    pan_view_scroll(host, -100);
    assert_eq!(host.view_scroll, 0);
    assert!(!host.scroll_new_output);
    pan_view_scroll(host, 100_000);
    assert_eq!(host.view_scroll, host.emulator.screen().max_view_scroll());
    let _ = host.emulator.feed(b"\x1b[?1049h");
    pan_view_scroll(host, 5);
    assert_eq!(
        host.view_scroll, 0,
        "alternate screens have no history view"
    );
    let _ = host.emulator.feed(b"\x1b[?1049l");
}

fn verify_close_target_tab(host: &mut HostState) {
    let previous = host.mux.window_ids();
    host.mux
        .new_tab("/bin/sh", &["-c".into(), "sleep 60".into()])
        .unwrap();
    let target = host.mux.selected_tab_index();
    assert_eq!(host.mux.tab_count(), previous.len() + 1);
    assert_eq!(
        close_tab_from_strip(host, usize::MAX),
        StripClickResult::NotHandled
    );
    assert_eq!(host.mux.tab_count(), previous.len() + 1);
    assert_eq!(
        close_tab_from_strip(host, target),
        StripClickResult::Handled
    );
    assert_eq!(
        host.mux.window_ids(),
        previous,
        "close only the requested tab"
    );
    App::paint(host).unwrap();
}

fn verify_menu_and_pointer(host: &mut HostState) {
    open_command_palette(host, keybind::Action::CommandPalette);
    assert!(host.palette.is_some());
    App::paint(host).unwrap();
    let layout = host.palette_layout.as_ref().unwrap();
    let row = layout.rows.iter().find(|row| row.global > 0).unwrap();
    let target = row.global;
    let pointer = ((layout.panel_x + 2) as f64, (row.y + 1) as f64);
    host.pointer_px = Some(pointer);
    apply_palette_pointer(host);
    assert_eq!(host.palette.as_ref().unwrap().selected, target);
    for point in [(f64::NAN, pointer.1), (-1.0, pointer.1)] {
        host.pointer_px = Some(point);
        apply_palette_pointer(host);
        assert_eq!(host.palette.as_ref().unwrap().selected, target);
    }
    host.palette = None;
    host.context_menu_target = Some(ContextMenuTarget::Pane(host.mux.focused_id()));
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::Pane));
    let (_, rows) = context_menu_rows(host).unwrap();
    assert_eq!(rows[10].name, "Remove session from space");
    assert!(rows[10].describe.contains("keep the session running"));
    assert_eq!(rows[11].name, "Remove and kill session…");
    host.context_menu.as_mut().unwrap().confirm = Some(11);
    let (header, rows) = context_menu_rows(host).unwrap();
    assert!(header.starts_with("Kill "));
    assert_eq!(rows[11].describe, "Enter: kill session · Esc: cancel");
    App::paint(host).unwrap();
    let layout = host.palette_layout.as_ref().unwrap();
    let row = layout.rows.iter().find(|row| row.global == 1).unwrap();
    host.pointer_px = Some(((layout.panel_x + 2) as f64, (row.y + 1) as f64));
    apply_palette_pointer(host);
    assert_eq!(host.context_menu.as_ref().unwrap().selected, 1);
    close_context_menu(host);
    host.space_rail.names = vec!["missing-contract-space".into()];
    host.context_menu_target = Some(ContextMenuTarget::SpaceChip(0));
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::SpaceChip));
    host.context_menu.as_mut().unwrap().confirm = Some(6);
    let (header, rows) = context_menu_rows(host).unwrap();
    assert_eq!(header, "missing-contract-space · saved space");
    assert_eq!(rows[5].name, "Move focused pane here");
    assert!(rows[6].describe.contains("press Enter again to confirm"));
    close_context_menu(host);
}

fn verify_rename_and_rail(host: &mut HostState) {
    let index = host.mux.selected_tab_index();
    let window = host.mux.window_at_tab(index).unwrap();
    host.tab_rename = Some(TabRename {
        index,
        window,
        pane: None,
        buffer: "Review workspace".into(),
        selected: false,
    });
    commit_tab_rename(host);
    assert!(host.tab_rename.is_none());
    assert_eq!(
        host.mux.tab_infos()[index].fixed_name.as_deref(),
        Some("Review workspace")
    );
    assert!(host.layout_dirty);
    host.spacing.space_rail = space_rail::RailSide::Left;
    host.spacing.space_rail_width_cols = 14;
    App::refit_geom(host, host.window.inner_size(), Some("rail contract"));
    let geom = host.mux.geom();
    host.pointer_px = Some((geom.rail_px as f64, (geom.top_chrome_px + 10) as f64));
    assert!(!rail_resize::button(
        host,
        ElementState::Pressed,
        MouseButton::Right
    ));
    assert!(rail_resize::button(
        host,
        ElementState::Pressed,
        MouseButton::Left
    ));
    assert!(host.rail_resizing);
    // Classic columns are terminal cells. Graphite's side column is 220
    // design px at 18 columns, independent of the cell width.
    let drag_x = if geom.chrome.graphite {
        super::graphite::side_rail_px(geom.chrome, 18) as f64
    } else {
        (host.font.cell_w * 18) as f64
    };
    assert!(rail_resize::motion(
        host,
        drag_x,
        (geom.top_chrome_px + 10) as f64
    ));
    assert_eq!(host.spacing.space_rail_width_cols, 18);
    assert!(rail_resize::button(
        host,
        ElementState::Released,
        MouseButton::Left
    ));
    assert!(!host.rail_resizing);
    let saved = config::load(&config::config_path()).unwrap();
    assert_eq!(saved.space_rail_width_cols, Some(18));
    assert!(!rail_resize::button(
        host,
        ElementState::Released,
        MouseButton::Left
    ));
}
