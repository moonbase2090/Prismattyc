//! Behavior of the three host input functions above the CRAP floor:
//! `apply_transparency_live`, `handle_rail_click`, and `dispatch_action`.

use super::*;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::platform::x11::EventLoopBuilderExtX11;

const CHILD_ENV: &str = "PRISMATTYC_RENDER_TEST_CHILD";

#[test]
fn routes_transparency_rail_clicks_and_actions() {
    if std::env::var_os(CHILD_ENV).is_some() {
        exercise_in_window();
        std::fs::write(
            std::env::var_os(render_window_tests::RESULT_ENV).unwrap(),
            b"complete",
        )
        .unwrap();
        return;
    }
    render_window_tests::run_in_private_display(
        "input_routing_tests::routes_transparency_rail_clicks_and_actions",
    );
}

fn exercise_in_window() {
    struct Proof {
        app: App,
        done: bool,
    }
    impl ApplicationHandler<UserAction> for Proof {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let id = self.app.open_window(event_loop, false).unwrap();
            let host = self.app.windows.get_mut(&id).unwrap();
            verify_transparency(host);
            verify_rail(host);
            verify_dispatch(host);
            self.done = true;
            self.app.windows.clear();
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(["--no-splash", "/bin/cat"].into_iter().map(String::from)).unwrap();
    let config = config::ConfigFile {
        a11y: Some(config::A11ySection {
            os_tree: Some(false),
            announce: Some(false),
        }),
        ..config::ConfigFile::default()
    };
    let app = App::new(cli, config, None, event_loop.create_proxy()).unwrap();
    let mut proof = Proof { app, done: false };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done, "the event loop must run the input assertions");
}

fn values_at(window: f32, chrome: f32, follows: bool) -> transparency::Values {
    transparency::Values {
        window_opacity: window,
        chrome_opacity: chrome,
        chrome_follows: follows,
        window_blur: false,
        pane_opacity_active: 0.91,
        pane_opacity_inactive: 0.82,
        background_image: None,
        background_opacity: 0.4,
        background_blur_px: 3,
    }
}

fn verify_transparency(host: &mut HostState) {
    let before_window = host.window_alpha;
    let before_chrome = host.chrome_alpha;
    host.dirty = false;
    apply_transparency_live(host, &values_at(1.0, 1.0, true), &[]);
    assert!(host.dirty, "a live apply always repaints");
    assert_eq!(host.window_alpha, before_window);
    assert_eq!(host.chrome_alpha, before_chrome);

    host.pane_opacity_active = 1.0;
    host.pane_opacity_inactive = 1.0;
    host.background = Some(BgCache {
        w: 1,
        h: 1,
        px: vec![7],
    });
    apply_transparency_live(
        host,
        &values_at(1.0, 1.0, true),
        &[
            transparency::Write::PaneActive(0.91),
            transparency::Write::PaneInactive(0.82),
        ],
    );
    assert_eq!(host.pane_opacity_active, 0.91);
    assert_eq!(host.pane_opacity_inactive, 0.82);
    assert!(
        host.background.is_some(),
        "pane opacity leaves the background cache alone"
    );

    apply_transparency_live(
        host,
        &values_at(1.0, 1.0, true),
        &[
            transparency::Write::ImageOpacity(0.4),
            transparency::Write::ImageBlur(3),
        ],
    );
    assert_eq!(host.background_opacity, 0.4);
    assert_eq!(host.background_blur_px, 3);
    assert!(host.background.is_none(), "image look drops the cache");

    let path = std::env::temp_dir().join(format!("crap-bg-{}", std::process::id()));
    std::fs::write(&path, b"not-a-real-png").unwrap();
    let mut with_image = values_at(1.0, 1.0, true);
    with_image.background_image = Some(path.display().to_string());
    apply_transparency_live(
        host,
        &with_image,
        &[transparency::Write::BackgroundImage(Some(
            path.display().to_string(),
        ))],
    );
    assert_eq!(
        host.background_png.as_deref(),
        Some(b"not-a-real-png".as_slice())
    );
    let _ = std::fs::remove_file(&path);

    with_image.background_image = Some("/no/such/crap-background.png".into());
    apply_transparency_live(
        host,
        &with_image,
        &[transparency::Write::BackgroundImage(Some(
            "/no/such/crap-background.png".into(),
        ))],
    );
    assert!(
        host.background_png.is_none(),
        "a missing image clears the bytes"
    );

    apply_transparency_live(
        host,
        &values_at(1.0, 1.0, true),
        &[transparency::Write::BackgroundImage(None)],
    );
    assert!(host.background_png.is_none());

    host.alpha_visual = true;
    host.background = Some(BgCache {
        w: 1,
        h: 1,
        px: vec![9],
    });
    apply_transparency_live(
        host,
        &values_at(0.5, 0.25, true),
        &[transparency::Write::WindowOpacity(0.5)],
    );
    assert_eq!(host.window_alpha, opacity_to_alpha(0.5));
    assert_eq!(
        host.chrome_alpha,
        opacity_to_alpha(0.5),
        "chrome follows the window when the key is absent"
    );
    assert!(host.background.is_none());

    host.background = Some(BgCache {
        w: 1,
        h: 1,
        px: vec![9],
    });
    apply_transparency_live(
        host,
        &values_at(0.5, 0.25, false),
        &[transparency::Write::ChromeOpacity(Some(0.25))],
    );
    assert_eq!(host.window_alpha, opacity_to_alpha(0.5));
    assert_eq!(host.chrome_alpha, opacity_to_alpha(0.25));
    assert!(host.background.is_none());

    host.alpha_visual = false;
    host.window_alpha = 200;
    host.chrome_alpha = 180;
    apply_transparency_live(
        host,
        &values_at(0.4, 0.4, true),
        &[transparency::Write::WindowOpacity(0.4)],
    );
    assert_eq!(
        host.window_alpha, 200,
        "no alpha visual keeps the old bytes"
    );
    assert_eq!(host.chrome_alpha, 180);

    host.window_alpha = 200;
    apply_transparency_live(
        host,
        &values_at(1.0, 1.0, true),
        &[transparency::Write::WindowOpacity(1.0)],
    );
    assert_eq!(host.window_alpha, 200);

    apply_transparency_live(
        host,
        &values_at(1.0, 1.0, true),
        &[transparency::Write::WindowBlur(true)],
    );
    assert!(host.dirty);
}

fn show_rail(host: &mut HostState, side: space_rail::RailSide, graphite: bool) {
    host.spacing.layout = config::LayoutMode::Bars;
    host.spacing.space_rail = side;
    host.spacing.chrome_style = if graphite {
        config::ChromeStyle::Graphite
    } else {
        config::ChromeStyle::Classic
    };
    App::refit_geom(host, host.window.inner_size(), Some("input rail"));
}

fn rail_layout(host: &HostState) -> space_rail::RailLayout {
    let size = host.window.inner_size();
    host.space_rail
        .layout(
            host.mux.geom(),
            size.width as usize,
            size.height as usize,
            host.spacing.space_rail_pane_names,
        )
        .unwrap_or_else(|| {
            panic!(
                "rail {:?} did not lay out at {}x{}",
                host.mux.geom().rail_side,
                size.width,
                size.height
            )
        })
}

fn click(host: &mut HostState, at: (f64, f64), button: MouseButton) -> bool {
    host.pointer_px = Some(at);
    handle_rail_click(host, button)
}

fn body_point(layout: &space_rail::RailLayout, index: usize, n: usize) -> (f64, f64) {
    let (x, y, w, h) = layout
        .chip_bounds(index, n)
        .unwrap_or_else(|| panic!("chip {index} of {n}"));
    let right = layout.close_left(x, w).unwrap_or(x + w);
    let px = x + (right.saturating_sub(x) / 2).max(1) - 1;
    ((px.min(x + w - 1)) as f64, (y + h / 2) as f64)
}

fn close_point(layout: &space_rail::RailLayout, index: usize, n: usize) -> (f64, f64) {
    let (x, y, w, h) = layout.chip_bounds(index, n).unwrap();
    let left = layout
        .close_left(x, w)
        .unwrap_or_else(|| panic!("chip {index} has no close"));
    ((left + 1) as f64, (y + h / 2) as f64)
}

fn expect_hit(
    layout: &space_rail::RailLayout,
    at: (f64, f64),
    n: usize,
    want: space_rail::RailHit,
) {
    let hit = layout.hit(at.0 as usize, at.1 as usize, n);
    assert_eq!(hit, Some(want), "pixel {at:?} n={n}");
}

fn verify_rail(host: &mut HostState) {
    host.spacing.layout = config::LayoutMode::Sidebar;
    host.pointer_px = Some((4.0, 4.0));
    host.space_rail.confirm = None;
    assert!(
        !handle_rail_click(host, MouseButton::Left),
        "sidebar mode does not use the bar rail"
    );
    assert!(host.space_rail.confirm.is_none());

    show_rail(host, space_rail::RailSide::Off, false);
    host.pointer_px = Some((4.0, 4.0));
    assert!(!handle_rail_click(host, MouseButton::Left));

    show_rail(host, space_rail::RailSide::Bottom, false);
    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(Some("alpha".into()));
    host.space_rail.destinations.clear();
    for pointer in [None, Some((f64::NAN, 1.0)), Some((-1.0, 4.0))] {
        host.pointer_px = pointer;
        assert!(!handle_rail_click(host, MouseButton::Left));
    }

    host.space_rail.focus_rail();
    assert!(host.space_rail.is_active());
    let layout = rail_layout(host);
    let outside = (layout.x as f64 + layout.w as f64 + 3.0, layout.y as f64);
    assert!(layout
        .hit(outside.0 as usize, outside.1 as usize, 2)
        .is_none());
    assert!(!click(host, outside, MouseButton::Left));
    assert!(!host.space_rail.is_active(), "a miss leaves keyboard focus");

    let n = host.space_rail.names.len();
    let layout = rail_layout(host);
    let current_close = close_point(&layout, 0, n);
    expect_hit(
        &layout,
        current_close,
        n,
        space_rail::RailHit::Chip {
            index: 0,
            close: true,
        },
    );
    assert!(click(host, current_close, MouseButton::Left));
    assert_eq!(host.space_rail.focus, Some(0));
    assert!(
        host.space_rail.confirm.is_none(),
        "the current chip's marker is not a delete"
    );

    let other_close = close_point(&layout, 1, n);
    expect_hit(
        &layout,
        other_close,
        n,
        space_rail::RailHit::Chip {
            index: 1,
            close: true,
        },
    );
    assert!(click(host, other_close, MouseButton::Left));
    assert_eq!(host.space_rail.confirm, Some(1));

    assert!(click(host, other_close, MouseButton::Left));
    assert!(
        host.space_rail.confirm.is_none(),
        "the second close confirms the delete"
    );

    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(Some("alpha".into()));
    host.space_rail.begin_rename(0);
    let layout = rail_layout(host);
    let editing = body_point(&layout, 0, 2);
    expect_hit(
        &layout,
        editing,
        2,
        space_rail::RailHit::Chip {
            index: 0,
            close: false,
        },
    );
    assert!(click(host, editing, MouseButton::Left));
    assert_eq!(
        host.space_rail.edit.as_ref().map(|edit| edit.target),
        Some(Some(0))
    );

    host.space_rail.leave();
    let body = body_point(&rail_layout(host), 0, 2);
    assert!(click(host, body, MouseButton::Left));
    assert!(
        toast_text(host).contains("alpha is the current space"),
        "opening the current chip reports that, got {}",
        toast_text(host)
    );

    let beta = body_point(&rail_layout(host), 1, 2);
    assert!(click(host, beta, MouseButton::Left));
    assert!(
        toast_text(host).contains("opening space beta"),
        "got {}",
        toast_text(host)
    );

    host.space_rail.names = (0..24).map(|i| format!("space-{i}-long")).collect();
    host.space_rail.set_current(Some("space-0-long".into()));
    host.space_rail.destinations.clear();
    let layout = rail_layout(host);
    let n = host.space_rail.names.len();
    assert!(layout.overflow, "a crowded bottom rail shows More");
    let (x, y, w, h) = layout.chip_bounds(n + 1, n).expect("overflow chip");
    let more = ((x + w / 2) as f64, (y + h / 2) as f64);
    expect_hit(&layout, more, n, space_rail::RailHit::Overflow);
    host.space_picker = None;
    assert!(click(host, more, MouseButton::Left));
    assert_eq!(
        host.space_picker.as_ref().map(|picker| picker.kind),
        Some(palette::SpacePickerKind::Open)
    );

    host.space_picker = None;
    let (x, y, _, h) = layout.chip_bounds(n, n).expect("plus");
    let plus = (x as f64 + 1.0, (y + h / 2) as f64);
    expect_hit(&layout, plus, n, space_rail::RailHit::Plus);
    host.space_rail.begin_new();
    assert!(click(host, plus, MouseButton::Left));
    assert_eq!(
        host.space_rail.edit.as_ref().map(|edit| edit.target),
        Some(None)
    );
    host.space_rail.leave();
    assert!(click(host, plus, MouseButton::Left));
    assert_eq!(
        host.space_rail.edit.as_ref().map(|edit| edit.target),
        Some(None)
    );
    assert!(host.dirty);

    let empty = empty_point(&layout, n);
    host.space_panel = None;
    assert!(click(host, empty, MouseButton::Right));
    assert_eq!(panel_header(host).as_deref(), Some("SPACES SETTINGS"));

    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(Some("alpha".into()));
    host.space_rail.leave();
    install_remote(host);
    show_rail(host, space_rail::RailSide::Bottom, false);
    let layout = rail_layout(host);
    let n = host.space_rail.names.len();
    let (x, y, _, h) = layout.dest_bounds(0, n).expect("destination chip");
    let dest = ((x + 1) as f64, (y + h / 2) as f64);
    expect_hit(&layout, dest, n, space_rail::RailHit::Destination(0));
    host.space_picker = None;
    assert!(click(host, dest, MouseButton::Left));
    assert_eq!(
        host.space_picker.as_ref().map(|picker| picker.kind),
        Some(palette::SpacePickerKind::Remote)
    );
    assert!(host.remote_open.is_some());

    let layout = rail_layout(host);
    let chip = body_point(&layout, 1, n);
    host.context_menu = None;
    assert!(click(host, chip, MouseButton::Right));
    assert!(matches!(
        host.context_menu_target,
        Some(ContextMenuTarget::SpaceChip(1))
    ));

    host.space_rail.leave();
    assert!(click(host, chip, MouseButton::Middle));
    assert_eq!(host.space_rail.confirm, Some(1));
    assert!(click(host, chip, MouseButton::Other(8)));
    assert!(
        host.space_rail.confirm.is_none() && host.space_rail.edit.is_none(),
        "any other button dismisses confirm"
    );
    assert!(click(
        host,
        empty_point(&rail_layout(host), n),
        MouseButton::Other(8)
    ));
    assert!(host.space_rail.confirm.is_none());

    show_rail(host, space_rail::RailSide::Left, true);
    host.space_rail.names = (0..40).map(|i| format!("side-{i}")).collect();
    host.space_rail.set_current(Some("side-0".into()));
    host.space_rail.destinations.clear();
    host.space_rail.leave();
    let layout = rail_layout(host);
    let thumb = layout.thumb.expect("a long side list has a thumb");
    let at = (
        (thumb.0 + thumb.2 / 2) as f64,
        (thumb.1 + thumb.3 / 2) as f64,
    );
    let n = host.space_rail.names.len();
    expect_hit(&layout, at, n, space_rail::RailHit::Thumb);
    host.rail_thumb_drag = None;
    assert!(click(host, at, MouseButton::Left));
    assert_eq!(
        host.rail_thumb_drag.map(|(_, scroll)| scroll),
        Some(host.space_rail.side_scroll())
    );

    let chip = body_point(&layout, 0, n);
    host.context_menu = None;
    assert!(click(host, chip, MouseButton::Right));
    assert!(
        matches!(
            host.context_menu_target,
            Some(ContextMenuTarget::RailSpace(0))
        ),
        "a side rail opens the rail menu, got {:?}",
        host.context_menu_target
    );
}

fn empty_point(layout: &space_rail::RailLayout, n: usize) -> (f64, f64) {
    for y in layout.y..layout.y + layout.h {
        for x in layout.x..layout.x + layout.w {
            if layout.hit(x, y, n) == Some(space_rail::RailHit::Empty) {
                return (x as f64, y as f64);
            }
        }
    }
    panic!("rail has no empty pixel");
}

fn install_remote(host: &mut HostState) {
    use prismattyc_mux::remote_catalog::{DestinationId, SshAlias, SshDestination};
    let dest = SshDestination {
        id: DestinationId::from_str("lab").unwrap(),
        label: "lab".into(),
        ssh_alias: SshAlias::from_str("lab").unwrap(),
    };
    let remote = remote_rail::RemoteRail::with_fetcher(
        vec![dest],
        remote_catalog::CatalogFetcher::with_launcher(
            Arc::new(|_| std::process::Command::new("/bin/true")),
            Arc::new(|| {}),
            Duration::from_secs(1),
        ),
    );
    host.space_rail.destinations = remote.views(None);
    *host.remote.borrow_mut() = remote;
}

fn clear_toasts(host: &mut HostState) {
    host.status_history = status_toasts::History::default();
}

fn divider_axes(host: &HostState) -> Vec<prismattyc_mux::Axis> {
    host.mux
        .dividers()
        .into_iter()
        .map(|divider| divider.axis)
        .collect()
}

fn axis_count(axes: &[prismattyc_mux::Axis], axis: prismattyc_mux::Axis) -> usize {
    axes.iter().filter(|item| **item == axis).count()
}

/// A blank split adds one pane on `axis` and leaves no naming prompt.
fn expect_blank_split(host: &mut HostState, action: keybind::Action, axis: prismattyc_mux::Axis) {
    host.session_prompt = None;
    let panes = host.mux.active_pane_count();
    let before = divider_axes(host);
    assert_eq!(dispatch(host, action), Dispatch::Handled);
    assert!(
        host.session_prompt.is_none(),
        "a blank split does not keep a naming prompt"
    );
    assert_eq!(host.mux.active_pane_count(), panes + 1);
    assert_eq!(
        axis_count(&divider_axes(host), axis),
        axis_count(&before, axis) + 1,
        "the new blank split is {axis:?}"
    );
}

/// A named tab or split either opens on `axis` or records this attempt's pmux error.
fn expect_named_terminal(
    host: &mut HostState,
    action: keybind::Action,
    axis: Option<prismattyc_mux::Axis>,
) {
    host.session_prompt = None;
    let panes = host.mux.active_pane_count();
    let tabs = host.mux.tab_count();
    let before = divider_axes(host);
    assert_eq!(dispatch(host, action), Dispatch::Handled);
    let grew = match axis {
        Some(_) => host.mux.active_pane_count() > panes,
        None => host.mux.tab_count() > tabs,
    };
    if grew {
        assert!(
            host.session_prompt.is_none(),
            "a created terminal closes the naming prompt"
        );
        if let Some(axis) = axis {
            assert_eq!(
                axis_count(&divider_axes(host), axis),
                axis_count(&before, axis) + 1,
                "the new named split is {axis:?}"
            );
        }
        return;
    }
    let error = host
        .session_prompt
        .as_ref()
        .and_then(|prompt| prompt.error.clone());
    assert_eq!(
        error.as_deref(),
        Some("current Space ownership is unresolved"),
        "a named terminal in an unresolved space reports that"
    );
    assert_eq!(host.mux.active_pane_count(), panes);
    assert_eq!(host.mux.tab_count(), tabs);
}

fn toast_text(host: &HostState) -> String {
    status_toasts::rows(&host.status_history, Instant::now())
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn panel_header(host: &HostState) -> Option<String> {
    space_panel::rows(host).map(|(header, _)| header)
}

fn quiet(host: &mut HostState) {
    host.palette = None;
    host.palette_layout = None;
    host.find.active = false;
    host.theme_picker = None;
    host.transparency = None;
    host.space_panel = None;
    host.context_menu = None;
    host.context_menu_target = None;
    host.space_picker = None;
    host.session_prompt = None;
    host.terminal_targets = None;
}

fn dispatch(host: &mut HostState, action: keybind::Action) -> Dispatch {
    dispatch_action(host, action, "/bin/true", &[])
}

fn verify_dispatch(host: &mut HostState) {
    use keybind::Action as A;
    show_rail(host, space_rail::RailSide::Bottom, false);
    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(None);
    host.palette_recent.clear();

    let _ = host.emulator.feed(b"\x1b[?1049l");
    for _ in 0..host.emulator.screen().rows() + 4 {
        let _ = host.emulator.feed(b"history\r\n");
    }
    host.view_scroll = 0;
    assert_eq!(dispatch(host, A::ScrollLineUp), Dispatch::Handled);
    assert_eq!(host.view_scroll, 1);
    assert_eq!(host.palette_recent.first(), Some(&A::ScrollLineUp));
    assert_eq!(dispatch(host, A::ScrollLineDown), Dispatch::Handled);
    assert_eq!(host.view_scroll, 0);

    let _ = host.emulator.feed(b"hello rail\r\n");
    assert_eq!(dispatch(host, A::SelectAll), Dispatch::Handled);
    let selected = selected_clipboard_text(&host.selection, host.emulator.screen());
    assert!(
        selected.as_deref().unwrap_or("").contains("hello rail"),
        "select all copies the viewport, got {selected:?}"
    );
    assert_eq!(dispatch(host, A::Copy), Dispatch::Handled);
    assert_eq!(dispatch(host, A::ClearScrollback), Dispatch::Handled);
    assert_eq!(host.view_scroll, 0);
    assert!(selected_clipboard_text(&host.selection, host.emulator.screen()).is_none());

    assert_eq!(dispatch(host, A::Paste), Dispatch::Handled);

    quiet(host);
    assert_eq!(dispatch(host, A::Find), Dispatch::Handled);
    assert!(host.find.active);
    assert!(host.find.query.is_empty());

    quiet(host);
    assert_eq!(dispatch(host, A::CommandPalette), Dispatch::Handled);
    assert!(host
        .palette
        .as_ref()
        .is_some_and(|palette| palette.filter.is_none()));
    quiet(host);
    assert_eq!(dispatch(host, A::PaletteFilterNext), Dispatch::Handled);
    assert!(host
        .palette
        .as_ref()
        .is_some_and(|palette| palette.filter.is_some()));
    quiet(host);
    assert_eq!(dispatch(host, A::PaletteFilterPrev), Dispatch::Handled);
    assert!(host
        .palette
        .as_ref()
        .is_some_and(|palette| palette.filter.is_some()));

    quiet(host);
    for action in [
        A::ThemePicker,
        A::Transparency,
        A::OpenSpace,
        A::DeleteSpace,
        A::MovePaneToSpace,
    ] {
        assert_eq!(dispatch(host, action), Dispatch::Handled);
        assert!(host.theme_picker.is_none());
        assert!(host.transparency.is_none());
        assert!(host.space_picker.is_none());
    }

    host.walkthrough = None;
    assert_eq!(dispatch(host, A::Walkthrough), Dispatch::Handled);
    assert!(
        host.walkthrough.is_some(),
        "walkthrough starts when none is open"
    );
    let progress = walkthrough::progress_path();
    if let Some(dir) = progress.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(&progress, b"not-progress").unwrap();
    assert_eq!(dispatch(host, A::WalkthroughReset), Dispatch::Handled);
    assert!(host.walkthrough.is_some());
    let rewritten = std::fs::read(&progress).unwrap_or_default();
    assert_ne!(rewritten, b"not-progress");

    assert_eq!(host.font_zoom_steps, 0);
    assert_eq!(dispatch(host, A::IncreaseFontSize), Dispatch::Handled);
    assert_eq!(host.font_zoom_steps, 1);
    assert_eq!(dispatch(host, A::DecreaseFontSize), Dispatch::Handled);
    assert_eq!(host.font_zoom_steps, 0);
    assert_eq!(dispatch(host, A::IncreaseFontSize), Dispatch::Handled);
    assert_eq!(host.font_zoom_steps, 1);
    assert_eq!(dispatch(host, A::ResetFontSize), Dispatch::Handled);
    assert_eq!(host.font_zoom_steps, 0);

    host.spacing.chrome_style = config::ChromeStyle::Classic;
    let classic = host.bar_color;
    assert_eq!(dispatch(host, A::BarColorNext), Dispatch::Handled);
    assert_eq!(host.bar_color, classic);
    host.spacing.chrome_style = config::ChromeStyle::Graphite;
    assert_eq!(dispatch(host, A::BarColorNext), Dispatch::Handled);
    assert_ne!(host.bar_color, classic);
    let stepped = host.bar_color;
    assert_eq!(dispatch(host, A::BarColorPrev), Dispatch::Handled);
    assert_ne!(host.bar_color, stepped);

    host.spacing.chrome_style = config::ChromeStyle::Classic;
    quiet(host);
    assert_eq!(dispatch(host, A::ChromeLayout), Dispatch::Handled);
    assert!(
        host.space_panel.is_none(),
        "classic chrome has no layout page"
    );
    host.spacing.chrome_style = config::ChromeStyle::Graphite;
    assert_eq!(dispatch(host, A::ChromeLayout), Dispatch::Handled);
    assert_eq!(panel_header(host).as_deref(), Some("LAYOUT"));

    host.spacing.layout = config::LayoutMode::Bars;
    host.spacing.sidebar_collapsed = false;
    assert_eq!(dispatch(host, A::SidebarCollapse), Dispatch::Handled);
    assert!(!host.spacing.sidebar_collapsed);
    host.spacing.layout = config::LayoutMode::Sidebar;
    host.spacing.chrome_style = config::ChromeStyle::Graphite;
    App::refit_geom(host, host.window.inner_size(), Some("sidebar collapse"));
    assert_eq!(dispatch(host, A::SidebarCollapse), Dispatch::Handled);
    assert!(host.spacing.sidebar_collapsed);

    show_rail(host, space_rail::RailSide::Off, false);
    host.space_rail.leave();
    assert_eq!(dispatch(host, A::SpaceRailFocus), Dispatch::Handled);
    assert!(!host.space_rail.is_active());
    show_rail(host, space_rail::RailSide::Bottom, true);
    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(Some("alpha".into()));
    assert_eq!(dispatch(host, A::SpaceRailFocus), Dispatch::Handled);
    assert!(host.space_rail.is_active());

    quiet(host);
    host.space_rail.leave();
    host.dirty = false;
    assert_eq!(dispatch(host, A::SpaceRailContextMenu), Dispatch::Handled);
    assert!(host.context_menu.is_none());
    assert!(!host.dirty);
    host.space_rail.focus = Some(0);
    assert_eq!(dispatch(host, A::SpaceRailContextMenu), Dispatch::Handled);
    assert!(matches!(
        host.context_menu_target,
        Some(ContextMenuTarget::RailSpace(0))
    ));

    quiet(host);
    assert_eq!(dispatch(host, A::SpaceSettings), Dispatch::Handled);
    assert_eq!(panel_header(host).as_deref(), Some("SPACES SETTINGS"));
    quiet(host);
    assert_eq!(dispatch(host, A::UpdateRestart), Dispatch::Handled);
    assert_eq!(panel_header(host).as_deref(), Some("UPDATE AND RESTART"));
    quiet(host);
    assert_eq!(dispatch(host, A::RecentMessages), Dispatch::Handled);
    assert_eq!(panel_header(host).as_deref(), Some("RECENT MESSAGES"));

    quiet(host);
    host.terminal_messages = false;
    assert_eq!(dispatch(host, A::TerminalSwitcher), Dispatch::Handled);
    assert!(!host.terminal_messages);
    assert!(host.terminal_targets.is_some());
    assert_eq!(
        host.space_picker.as_ref().map(|picker| picker.kind),
        Some(palette::SpacePickerKind::Open)
    );
    quiet(host);
    assert_eq!(dispatch(host, A::AgentMessages), Dispatch::Handled);
    assert!(host.terminal_messages);
    assert!(host.terminal_targets.is_some());

    host.space_polish.undo = None;
    assert_eq!(dispatch(host, A::UndoSpaceChange), Dispatch::Handled);
    assert!(
        toast_text(host).contains("Nothing to undo"),
        "{}",
        toast_text(host)
    );

    host.space_rail.set_current(None);
    host.space_rail.leave();
    assert_eq!(dispatch(host, A::SaveSpace), Dispatch::Handled);
    assert_eq!(
        host.space_rail.edit.as_ref().map(|edit| edit.target),
        Some(None)
    );
    host.space_rail.set_current(Some("alpha".into()));
    host.space_polish.failed = false;
    assert_eq!(dispatch(host, A::SaveSpace), Dispatch::Handled);
    let saved = toast_text(host);
    assert!(
        host.space_polish.failed || saved.contains("Saved"),
        "saving the current space reports a result, got {saved}"
    );

    host.dirty = false;
    assert_eq!(dispatch(host, A::JumpNeedsYou), Dispatch::Handled);

    host.space_rail.names = vec!["alpha".into(), "beta".into()];
    host.space_rail.set_current(Some("alpha".into()));
    clear_toasts(host);
    assert_eq!(dispatch(host, A::SpaceRailNext), Dispatch::Handled);
    assert!(
        toast_text(host).contains("opening space beta"),
        "next space asks to open beta, got {}",
        toast_text(host)
    );
    host.space_rail.names.clear();
    let before = toast_text(host);
    assert_eq!(dispatch(host, A::SpaceRailPrev), Dispatch::Handled);
    assert_eq!(toast_text(host), before, "no neighbour means no new toast");

    assert_eq!(dispatch(host, A::RichFocus), Dispatch::Handled);
    assert!(host.palette.is_none());
    assert_eq!(dispatch(host, A::NewWindow), Dispatch::OpenWindow);
    assert_eq!(dispatch(host, A::OpenConfig), Dispatch::OpenConfig);
    assert_eq!(dispatch(host, A::Quit), Dispatch::Exit);

    host.session_prompt = None;
    let tabs = host.mux.tab_count();
    assert_eq!(dispatch(host, A::NewBlankTab), Dispatch::Handled);
    assert!(host.session_prompt.is_none());
    assert_eq!(host.mux.tab_count(), tabs + 1, "a blank tab opens a shell");
    expect_named_terminal(host, A::NewSessionTab, None);
    expect_blank_split(host, A::BlankSplitRight, prismattyc_mux::Axis::Horizontal);
    expect_blank_split(host, A::BlankSplitDown, prismattyc_mux::Axis::Vertical);
    expect_named_terminal(
        host,
        A::SessionSplitRight,
        Some(prismattyc_mux::Axis::Horizontal),
    );
    expect_named_terminal(
        host,
        A::SessionSplitDown,
        Some(prismattyc_mux::Axis::Vertical),
    );

    assert_eq!(dispatch(host, A::FocusLeft), Dispatch::Handled);
    while host.mux.tab_count() > 1 {
        let before = host.mux.tab_count();
        assert_eq!(dispatch(host, A::CloseTab), Dispatch::Handled);
        assert!(host.mux.tab_count() < before, "close tab removes one");
    }
    assert_eq!(dispatch(host, A::Detach), Dispatch::Exit);
}
