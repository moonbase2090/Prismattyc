//! Interaction contracts exercised in the existing private native window.
use super::*;

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
    assert_eq!(host.mux.tab_infos()[index].title, "Review workspace");
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
    assert!(rail_resize::motion(
        host,
        (host.font.cell_w * 18) as f64,
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
