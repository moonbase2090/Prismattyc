//! Real-window render tests. Each test process owns a private Xvfb display.

use super::*;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::{Child, Command, Stdio};
use winit::platform::x11::EventLoopBuilderExtX11;

const CHILD_ENV: &str = "PRISMATTYC_RENDER_TEST_CHILD";
const RESULT_ENV: &str = "PRISMATTYC_RENDER_TEST_RESULT";
const TEST_NAME: &str = "render_window_tests::real_window_paint_reaches_the_backend";

const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 0x90, 0x77, 0x53, 0xde, 0, 0, 0, 0x0c, 0x49, 0x44, 0x41, 0x54,
    0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0, 0, 3, 1, 1, 0, 0xf7, 3, 0x41, 0x43, 0, 0, 0, 0, 0x49,
    0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pt283-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Own the whole child process group and reap the direct child on panic too.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.0.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for _ in 0..20 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_window_paint_reaches_the_backend() {
    if std::env::var_os(CHILD_ENV).is_some() {
        paint_in_real_window(false);
        std::fs::write(std::env::var_os(RESULT_ENV).unwrap(), b"complete").unwrap();
        return;
    }
    run_in_private_display(TEST_NAME);
}

#[test]
#[should_panic(expected = "window fixture did not complete")]
fn missing_child_test_is_rejected() {
    // Rust's test harness returns success when --exact matches no tests.
    // A completion marker must reject that success without window assertions.
    run_in_private_display("render_window_tests::nonexistent_fixture");
}

pub(super) fn run_in_private_display(test_name: &str) {
    let scratch = Scratch::new();
    let started = Instant::now();
    let time_scale = std::env::var("PRISMATTYC_TEST_TIME_SCALE")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1)
        .clamp(1, 10);
    let (_display, number) = start_private_display(&scratch, started, time_scale);
    let display_ready = started.elapsed();
    let mut command = window_child_command(&scratch, number, test_name);
    let mut child = ChildGuard(command.spawn().unwrap());
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30 * time_scale),
            "window test child timed out\n{}\n{}",
            std::fs::read_to_string(scratch.0.join("test.stdout")).unwrap_or_default(),
            std::fs::read_to_string(scratch.0.join("test.stderr")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = std::fs::read_to_string(scratch.0.join("test.stdout")).unwrap();
    let stderr = std::fs::read_to_string(scratch.0.join("test.stderr")).unwrap();
    eprintln!(
        "PT283 display_startup={display_ready:?} total={:?}\n{stdout}\n{stderr}",
        started.elapsed()
    );
    assert!(status.success(), "real-window child failed ({status})");
    assert_eq!(
        std::fs::read(scratch.0.join("result")).ok().as_deref(),
        Some(b"complete".as_slice()),
        "window fixture did not complete; the child may have selected zero tests"
    );
}

/// Keep display allocation separate from the child test's deadline and result.
fn start_private_display(
    scratch: &Scratch,
    started: Instant,
    time_scale: u64,
) -> (ChildGuard, u32) {
    let display_file = scratch.0.join("display");
    // Xvfb allocates an unused display itself and reports only its own number
    // after initialization. Never connect to a guessed or shared :99 display.
    let mut display = ChildGuard(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1024x768x24",
                "-nolisten",
                "tcp",
            ])
            .process_group(0)
            .stdout(std::fs::File::create(&display_file).unwrap())
            .stderr(std::fs::File::create(scratch.0.join("xvfb.log")).unwrap())
            .spawn()
            .expect("real-window tests require Xvfb installed"),
    );
    let number = loop {
        if let Ok(number) = std::fs::read_to_string(&display_file)
            .unwrap()
            .trim()
            .parse::<u32>()
        {
            break number;
        }
        assert!(
            display.0.try_wait().unwrap().is_none(),
            "Xvfb exited before allocating a display: {}",
            std::fs::read_to_string(scratch.0.join("xvfb.log")).unwrap_or_default()
        );
        assert!(
            started.elapsed() < Duration::from_secs(10 * time_scale),
            "Xvfb startup timed out: {}",
            std::fs::read_to_string(scratch.0.join("xvfb.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    };
    (display, number)
}

/// Give the child its own mux, configuration and display discovery paths.
fn window_child_command(scratch: &Scratch, number: u32, test_name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test_name, "--nocapture", "--test-threads=1"]);
    // Isolate host discovery and config writes from the developer's mux seat.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if private_child_removes_env(&name) {
            command.env_remove(key);
        }
    }
    command
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XAUTHORITY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("DISPLAY", format!(":{number}"))
        .env(CHILD_ENV, "1")
        .env(RESULT_ENV, scratch.0.join("result"))
        .env("XDG_CONFIG_HOME", scratch.0.join("config"))
        .env("XDG_DATA_HOME", scratch.0.join("data"))
        .env("XDG_STATE_HOME", scratch.0.join("state"))
        .env("XDG_RUNTIME_DIR", &scratch.0)
        .env("PMUX_SOCKET", scratch.0.join("pmux.sock"))
        .process_group(0)
        .stdout(std::fs::File::create(scratch.0.join("test.stdout")).unwrap())
        .stderr(std::fs::File::create(scratch.0.join("test.stderr")).unwrap());
    command
}

fn private_child_removes_env(name: &str) -> bool {
    name.starts_with("PMUX_")
        || (name.starts_with("PRISMATTYC_") && name != "PRISMATTYC_TEST_TIME_SCALE")
}

pub(super) fn restore_in_private_window() {
    if std::env::var_os(CHILD_ENV).is_some() {
        paint_in_real_window(true);
        std::fs::write(std::env::var_os(RESULT_ENV).unwrap(), b"complete").unwrap();
    } else {
        run_in_private_display("restore_prompt::tests::real_window_choices");
    }
}

fn paint_in_real_window(restore_only: bool) {
    struct PaintProof {
        app: App,
        painted: bool,
        restore_only: bool,
    }
    impl ApplicationHandler<UserAction> for PaintProof {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.restore_only {
                verify_startup_restore(&mut self.app, event_loop);
                self.painted = true;
                event_loop.exit();
                return;
            }
            let create_started = Instant::now();
            let id = self.app.open_window(event_loop, false).unwrap();
            let window_created = create_started.elapsed();
            let host = self.app.windows.get_mut(&id).unwrap();
            assert!(matches!(
                host.present,
                Some(PresentBackend::Softbuffer { .. })
            ));
            let backend = host.present.as_ref().unwrap();
            assert!(backend.carries_alpha(&host.window));
            let probe = PresentBackend::Probe;
            assert!(!probe.carries_alpha(&host.window));
            assert!(host.window.inner_size().width > 0);
            let _ = host.emulator.feed(b"PT-283 real window");
            assert_eq!(host.render_frame.cells_painted, 0);
            let paint_started = Instant::now();
            App::paint(host).unwrap();
            eprintln!(
                "PT283 window_create={window_created:?} paint={:?}",
                paint_started.elapsed()
            );
            assert!(host.present.is_some(), "paint must restore its backend");
            assert!(!host.dirty, "successful paint must retire dirty state");
            assert!(
                host.render_frame.cells_painted > 0,
                "the real backend must rasterize the pane, not return a no-op success"
            );
            verify_paint_bookkeeping(host);
            verify_repaint_reasons(host);
            verify_pixels_and_overlays(host);
            verify_background(host);
            verify_alt_screen_partial_and_scrollback(host);
            verify_pane_damage_and_chrome(host);
            verify_render_guards(host);
            verify_caption_click(host);
            verify_decision_handlers(host);
            self.app.register_host_pid();
            self.app.publish_render_status();
            let snapshot =
                prismattyc_mux::host_render_status::read(&host_mux_socket().unwrap()).unwrap();
            let guards = snapshot["windows"][0]["last_raster"]["guards"]
                .as_array()
                .expect("render status guards must be an array");
            for guard in ["osd", "preedit"] {
                assert!(guards.iter().any(|value| value == guard), "missing {guard}");
            }
            assert!(!guards.iter().any(|value| value == "backend-no-partial"));
            verify_config_reload(&mut self.app, id);
            chrome_contract_tests::verify(self.app.windows.get_mut(&id).unwrap());
            self.painted = true;
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
    let mut proof = PaintProof {
        app,
        painted: false,
        restore_only,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.painted, "the event loop must run the paint assertion");
}

/// Exercise the first-window cache boundary, not just the prompt's key map.
fn verify_startup_restore(app: &mut App, event_loop: &ActiveEventLoop) {
    app.windows.clear();
    app.cli.no_splash = false;
    app.cli.explicit_program = false;
    let fresh = app.open_window(event_loop, false).unwrap();
    assert!(
        app.windows[&fresh].splash.is_some(),
        "a fresh bare launch keeps its splash"
    );
    assert!(app.windows[&fresh].restore_prompt.is_none());
    app.windows.clear();
    let path = attach_tabs::layout_path_from_socket(&host_mux_socket().unwrap());
    // Missing sessions fall back to a nested pmux child. The unit-test runner
    // need not have an installed pmux, so own an exited-session stand-in too.
    let bin_dir = path.parent().unwrap().join("fixture-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let pmux = bin_dir.join("pmux");
    std::fs::write(&pmux, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&pmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("PMUX", pmux);
    let saved = attach_tabs::AttachTabsFile {
        tabs: vec![
            attach_tabs::AttachTabRecord {
                title: "one".into(),
                sessions: vec!["missing-one".into()],
            },
            attach_tabs::AttachTabRecord {
                title: "two".into(),
                sessions: vec!["missing-two".into()],
            },
        ],
        active_tab: 1,
        focused_session: Some("missing-two".into()),
        ..Default::default()
    };
    attach_tabs::persist_if_changed(&path, &mut None, saved.clone()).unwrap();
    let original = std::fs::read(&path).unwrap();
    let id = app.open_window(event_loop, false).unwrap();
    app.poll_attach_tabs();
    {
        let host = app.windows.get_mut(&id).unwrap();
        assert!(host.restore_prompt.is_some());
        assert!(host.splash.is_none());
        assert_eq!(host.mux.pane_count(), 1);
        assert!(host.attach_pane_sessions.is_empty());
        let pixels = frame(host);
        let size = host.window.inner_size();
        let rail = space_rail::RailLayout::for_window(
            host.mux.geom(),
            size.width as usize,
            size.height as usize,
            &host.space_rail.names,
        )
        .unwrap();
        let (x, y, _, _) = rail
            .chip_bounds(host.space_rail.names.len(), host.space_rail.names.len())
            .unwrap();
        let rail_point = Some(((x + 1) as f64, (y + 1) as f64));
        host.pointer_px = rail_point;
        assert_eq!(
            hover_target_at_pointer(host),
            None,
            "the prompt blocks chrome hover"
        );
        assert!(host
            .render_frame
            .guards
            .contains(render_diagnostics::Guard::RestorePrompt));
        assert!(matches!(
            chrome_overlay(host),
            a11y::OverlayKind::RestorePrompt { .. }
        ));
        persist_attach_layout_from_live(host);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        restore_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), true);
        assert!(host.restore_prompt.is_some(), "a held key must not answer");
        assert!(restore_prompt::handle_pointer(
            host,
            &WindowEvent::Ime(winit::event::Ime::Commit("secret".into()))
        ));
        let device_id = winit::event::DeviceId::dummy();
        let layout = host.palette_layout.as_ref().unwrap();
        let x = (layout.panel_x + 1) as f64;
        let y = (layout.rows.iter().find(|row| row.global == 1).unwrap().y + 1) as f64;
        for position in [
            winit::dpi::PhysicalPosition::new(f64::NAN, y),
            winit::dpi::PhysicalPosition::new(-1.0, y),
            winit::dpi::PhysicalPosition::new(x, y),
        ] {
            assert!(restore_prompt::handle_pointer(
                host,
                &WindowEvent::CursorMoved {
                    device_id,
                    position
                }
            ));
        }
        assert_eq!(host.restore_prompt.as_ref().unwrap().selected, 1);
        assert!(restore_prompt::handle_pointer(
            host,
            &WindowEvent::MouseInput {
                device_id,
                state: ElementState::Pressed,
                button: MouseButton::Left,
            }
        ));
        assert!(host.restore_prompt.is_none());
        host.pointer_px = rail_point;
        assert_eq!(
            hover_target_at_pointer(host),
            Some(HoverTarget::Rail(space_rail::RailHit::Plus))
        );
        assert_ne!(frame(host), pixels, "the modal must leave the framebuffer");
    }
    app.poll_attach_tabs();
    app.poll_attach_tabs();
    let host = app.windows.get(&id).unwrap();
    assert_eq!(host.mux.pane_count(), 1);
    assert!(
        host.attach_pane_sessions.is_empty(),
        "decline must not restore husks later"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let later = app.open_window(event_loop, false).unwrap();
    assert!(
        app.windows[&later].restore_prompt.is_none(),
        "ask only on the first window"
    );
    assert!(app.windows[&later].cache_writer);
    assert_ne!(
        app.windows[&later].attach_layout_path.as_ref(),
        Some(&path),
        "later windows own an independent view cache"
    );
    assert!(app.windows[&later].splash.is_none());

    // A later explicit space-open cache update must still work after decline.
    let mut updated = saved.clone();
    updated.tabs[0].title = "Updated by space open".into();
    attach_tabs::persist_if_changed(&path, &mut None, updated.clone()).unwrap();
    app.poll_attach_tabs();
    let host = app.windows.get_mut(&id).unwrap();
    assert_eq!(host.mux.tab_count(), 2);
    assert_eq!(host.attach_layout.as_ref(), Some(&updated));
    assert!(host.restore_prompt.is_none());
    // Local edits must continue to persist after the startup decision.
    host.mux
        .rename_window(host.mux.active_window(), "Locally renamed")
        .unwrap();
    persist_attach_layout_from_live(host);
    let written = attach_tabs::load(&path).unwrap();
    assert_eq!(written.tabs[written.active_tab].title, "Locally renamed");
    assert_ne!(written, updated);
    attach_tabs::persist_if_changed(&path, &mut None, saved.clone()).unwrap();

    // Simulate another cold launch against the same intact saved layout.
    app.windows.clear();
    let id = app.open_window(event_loop, false).unwrap();
    let host = app.windows.get_mut(&id).unwrap();
    assert!(host.restore_prompt.is_some());
    dispatch_overlay_activate(host, 0, "/bin/cat", &[]);
    assert!(host.restore_prompt.is_none());
    assert_eq!(host.mux.tab_count(), 2);
    assert_eq!(host.attach_pane_sessions.len(), 2);
    assert_eq!(host.mux.selected_tab_index(), 1);
    assert_eq!(
        host.attach_pane_sessions
            .get(&host.mux.focused_id())
            .map(String::as_str),
        Some("missing-two")
    );
    let restored = host.attach_layout.as_ref().unwrap();
    assert_eq!(restored.tabs, saved.tabs);
    assert_eq!(restored.focused_session, saved.focused_session);
    assert_eq!(
        restored
            .session_names
            .get("missing-two")
            .map(String::as_str),
        Some("missing-two")
    );
    assert!(host.mux.is_placeholder(host.mux.focused_id()));
    app.poll_attach_tabs();
    assert!(app.windows[&id].restore_prompt.is_none());
    app.windows.clear();
    // Verify the real dispatch path, in addition to the pure key map and AT.
    let id = app.open_window(event_loop, false).unwrap();
    let host = app.windows.get_mut(&id).unwrap();
    restore_prompt::dispatch_key(host, &Key::Named(NamedKey::Escape), false);
    assert!(host.restore_prompt.is_none());
    assert!(host.attach_pane_sessions.is_empty());
    app.windows.clear();
    let id = app.open_window(event_loop, false).unwrap();
    let host = app.windows.get_mut(&id).unwrap();
    restore_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
    assert!(host.restore_prompt.is_none());
    assert_eq!(host.mux.tab_count(), 2);
    app.windows.clear();
}

fn verify_config_reload(app: &mut App, id: WindowId) {
    let (tx, rx) = std::sync::mpsc::channel::<config::WatchDelivery>();
    app.config_rx = Some(rx);

    let mut reloaded = app.file_config.clone();
    reloaded.render_timer = Some(config::RenderTimer::Log);
    {
        let host = app.windows.get_mut(&id).unwrap();
        host.render_timer = config::RenderTimer::Off;
        host.dirty = false;
        host.pending_full_repaint = None;
    }

    tx.send(Ok(reloaded.clone())).unwrap();
    app.poll_config_reload();

    assert_eq!(app.file_config, reloaded);
    let host = app.windows.get(&id).unwrap();
    assert_eq!(host.render_timer, config::RenderTimer::Log);
    assert!(host.dirty);
    assert_eq!(host.pending_full_repaint, Some(FullRepaintReason::Fallback));
}

/// Dimensioned CPU framebuffer with guards outside the renderer's slice.
/// Pixel assertions use relative frames or solid colors, not font snapshots.
fn frame(host: &mut HostState) -> Vec<u32> {
    let size = host.window.inner_size();
    let len = size.width as usize * size.height as usize;
    let mut guarded = vec![0x12345678; len + 2];
    rasterize_frame(host, &mut guarded[1..=len], size.width, size.height, false);
    assert_eq!(guarded[0], 0x12345678);
    assert_eq!(guarded[len + 1], 0x12345678);
    guarded[1..=len].to_vec()
}

fn verify_paint_bookkeeping(host: &mut HostState) {
    host.dirty = true;
    host.render_timer = config::RenderTimer::Log;
    host.render_timer_log_every_frame = false;
    host.last_render_log = None;
    host.render_window = RenderWindow {
        started_at: Some(Instant::now() - Duration::from_secs(2)),
        ..RenderWindow::default()
    };
    host.render_frame.timing.raster_us = u64::MAX;
    host.render_frame.timing.present_us = u64::MAX;
    App::paint(host).unwrap();
    assert!(host.present.is_some());
    assert!(!host.dirty);
    assert_eq!(host.render_osd.frame_count, 1);
    assert_eq!(
        host.render_osd.max_cells_painted,
        host.render_frame.cells_painted
    );
    assert_ne!(host.render_frame.timing.raster_us, u64::MAX);
    assert_ne!(host.render_frame.timing.present_us, u64::MAX);
    assert!(host.last_render_log.is_some());
    host.render_timer = config::RenderTimer::Off;

    let Some(PresentBackend::Softbuffer { fail_paint, .. }) = host.present.as_mut() else {
        panic!("fixture requires a real softbuffer backend");
    };
    *fail_paint = true;
    host.dirty = true;
    let previous_frames = host.render_window.frame_count;
    let previous_osd = host.render_osd;
    let error = App::paint(host).unwrap_err();
    assert!(error.to_string().contains("injected present failure"));
    assert_eq!(host.pending_full_repaint, Some(FullRepaintReason::Fallback));
    assert!(!host.render_frame.present_succeeded);
    assert!(
        host.dirty,
        "failed paint must leave the frame dirty for retry"
    );
    assert_eq!(host.render_window.frame_count, previous_frames);
    assert_eq!(host.render_osd, previous_osd);
    let Some(PresentBackend::Softbuffer { fail_paint, .. }) = host.present.as_mut() else {
        panic!("failed paint must restore the backend before returning its error");
    };
    *fail_paint = false;
    App::paint(host).unwrap();
    assert!(!host.dirty, "the restored real backend must paint on retry");
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::Fallback)
    );
    assert!(host.render_frame.present_succeeded);
    assert!(host.pending_full_repaint.is_none());
}

fn verify_repaint_reasons(host: &mut HostState) {
    host.pending_full_repaint = None;
    host.pending_full_repaint = Some(FullRepaintReason::Theme);
    frame(host);
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::Theme)
    );
    assert!(host.pending_full_repaint.is_none());
    let _ = host.emulator.feed(b"\x1b[?1049h");
    frame(host);
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::AltScreen)
    );
    let _ = host.emulator.feed(b"\x1b[?1049l");
    frame(host);
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::AltScreen)
    );
    host.view_scroll = 1;
    frame(host);
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::Scrollback)
    );
    host.view_scroll = 0;
}

fn verify_pixels_and_overlays(host: &mut HostState) {
    // Keep the guest quiet and remove the cursor for stable frame comparisons.
    let _ = host.emulator.feed(b"\x1b[2J\x1b[H\x1b[?25l");
    let plain = frame(host);
    let _ = host.emulator.feed(b"\x1b[48;2;17;34;51m \x1b[0m");
    let colored = frame(host);
    assert_ne!(colored, plain, "SGR cell must reach the framebuffer");
    let geom = host.mux.geom();
    let (_, rect) = host.mux.rects().next().unwrap();
    let (x, y, _, _) = geom.pane_content_px(rect);
    let width = host.window.inner_size().width as usize;
    assert_eq!(
        colored[(y + host.font.cell_h / 2) * width + x + host.font.cell_w / 2] & 0xffffff,
        0x112233
    );
    let _ = host.emulator.feed(b"\x1b[2J\x1b[H");
    assert_eq!(frame(host), plain, "erasing a cell must remove its pixels");

    host.preedit = Preedit {
        text: "compose".into(),
        cursor: Some((0, 7)),
    };
    assert_ne!(
        frame(host),
        plain,
        "IME preedit must paint with the VT cursor hidden"
    );
    host.preedit = Preedit::default();
    assert_eq!(
        frame(host),
        plain,
        "clearing preedit must remove its pixels"
    );

    host.find = FindMode {
        active: true,
        query: "needle".into(),
        ..FindMode::default()
    };
    assert_ne!(frame(host), plain, "find prompt must reach the framebuffer");
    host.find = FindMode::default();
    assert_eq!(frame(host), plain);

    host.palette = Some(Palette::default());
    assert_ne!(frame(host), plain, "palette must reach the framebuffer");
    assert!(
        host.palette_layout.is_some(),
        "paint publishes the palette hit geometry"
    );
    host.palette = None;
    assert_eq!(
        frame(host),
        plain,
        "closing the palette must clear its pixels"
    );

    host.theme_picker = Some(ThemePicker {
        original: host.theme.clone(),
        selected: Some(0),
        family: None,
        scroll: 0,
    });
    assert_ne!(
        frame(host),
        plain,
        "theme picker must reach the framebuffer"
    );
    host.theme_picker = None;
    assert_eq!(frame(host), plain);

    host.bell_toasts.push(BellToast {
        pane: host.mux.focused_id(),
        until: Instant::now() + Duration::from_secs(10),
        label: " test bell ".into(),
    });
    assert_ne!(
        frame(host),
        plain,
        "pane bell toast must reach the framebuffer"
    );
    host.bell_toasts.clear();
    assert_eq!(frame(host), plain);
}

fn verify_background(host: &mut HostState) {
    let plain = frame(host);
    host.background_png = Some(RED_PNG.to_vec());
    host.background_opacity = 1.0;
    host.background_blur_px = 0;
    let image = frame(host);
    let cache = host
        .background
        .as_ref()
        .expect("PNG must build the background cache");
    assert_eq!(
        (cache.w, cache.h),
        (
            host.window.inner_size().width,
            host.window.inner_size().height
        )
    );
    assert_eq!(
        image[host.mux.geom().top_chrome_px * host.window.inner_size().width as usize] & 0xffffff,
        0xff0000,
        "background image covers the outer window ground"
    );
    assert_ne!(image, plain);
    assert_eq!(
        frame(host),
        image,
        "cached background must match its first frame"
    );
    host.window_alpha = 127;
    let translucent = frame(host);
    assert_eq!(
        translucent[host.mux.geom().top_chrome_px * host.window.inner_size().width as usize] >> 24,
        127,
        "cached background must adopt the new alpha"
    );
    host.window_alpha = OPAQUE_ALPHA;
    host.background_png = None;
    host.background = None;
    assert_eq!(frame(host), plain);
}

fn verify_pane_damage_and_chrome(host: &mut HostState) {
    let first = host.mux.focused_id();
    verify_split_panes(host, first);
    verify_steady_four_pane_partial(host);
    verify_cursor_only_partial_frames(host);
    verify_same_layout_tab_switch(host);
    verify_idle_pulse_damage(host);
    verify_light_cycle_settles_without_head_trails(host);
}

fn verify_light_cycle_settles_without_head_trails(host: &mut HostState) {
    verify_light_cycle_frames(host, None);
    host.background_png = Some(RED_PNG.to_vec());
    host.background_opacity = 1.0;
    host.background_blur_px = 0;
    host.window_alpha = 127;
    // Configured images force a full raster and restore the cached RGB before
    // pane alpha is applied. Check this path with a translucent surface too.
    verify_light_cycle_frames(host, Some(FullRepaintReason::Fallback));
    host.background_png = None;
    host.background = None;
    host.window_alpha = OPAQUE_ALPHA;
    frame(host);
}

fn verify_light_cycle_frames(host: &mut HostState, expected_full: Option<FullRepaintReason>) {
    host.light_cycle = true;
    host.light_cycle_ms = 5000;
    host.light_cycle_head = true;
    let mut retained = frame(host);
    let screen = host.mux.focused_mut().emulator.screen();
    let painted_cells = if expected_full.is_some() {
        render_cells_painted(host)
    } else {
        screen.columns() as u64 * screen.rows() as u64
    };
    for step in [2, 6, 10, 14] {
        host.border_anim = Some(Instant::now() - Duration::from_millis(step * 250));
        host.last_cycle_step = step as u8;
        let painted = paint_retained(host, &mut retained);
        assert_eq!(painted.full_repaint_reason, expected_full);
        assert_eq!(painted.cells_painted, painted_cells);
    }
    host.border_anim = None;
    let settled = paint_retained(host, &mut retained);
    assert_eq!(settled.full_repaint_reason, expected_full);
    assert_eq!(settled.cells_painted, painted_cells);
    let oracle = full_frame_oracle(host);
    let different = retained.iter().zip(&oracle).filter(|(a, b)| a != b).count();
    assert_eq!(
        different, 0,
        "settled light-cycle must clear every old head pixel"
    );
    host.light_cycle = false;
}

fn verify_cursor_only_partial_frames(host: &mut HostState) {
    let _ = host
        .mux
        .focused_mut()
        .emulator
        .feed(b"\x1b[0m\x1b[2J\x1b[H\x1b[?25htest");
    let mut retained = frame(host);
    // Readline uses BS for Left and CR for Home. Also cover relative CSI
    // motion across rows so both the old and new caret get repainted.
    for bytes in [
        b"\x08".as_slice(),
        b"\r",
        b"\x1b[C",
        b"\x1b[B",
        b"\x1b[A",
        b"\x1b[D",
    ] {
        let before = retained.clone();
        let _ = host.mux.focused_mut().emulator.feed(bytes);
        let painted = paint_retained(host, &mut retained);
        assert_eq!(painted.full_repaint_reason, None);
        assert!(
            painted.cells_painted > 0,
            "cursor-only output needs damage: {bytes:?}"
        );
        assert_ne!(retained, before, "caret must visibly move: {bytes:?}");
        assert_eq!(
            retained,
            full_frame_oracle(host),
            "old caret must clear: {bytes:?}"
        );
    }
}

fn verify_split_panes(host: &mut HostState, first: PaneId) {
    host.mux
        .split_focused("/bin/cat", &[], prismattyc_mux::Axis::Horizontal, 0.5)
        .unwrap();
    assert_eq!(host.mux.pane_count(), 2);
    for id in host.mux.active_pane_ids() {
        assert!(host.mux.focus(id));
        let pane = host.mux.focused_mut();
        let _ = pane.emulator.feed(if id == first {
            b"\x1b[2J\x1b[H\x1b[?25l\x1b[48;2;255;0;0m "
        } else {
            b"\x1b[2J\x1b[H\x1b[?25l\x1b[48;2;0;255;0m "
        });
    }
    let painted = frame(host);
    let width = host.window.inner_size().width as usize;
    let geom = host.mux.geom();
    for (id, rect) in host.mux.rects() {
        let (x, y, _, _) = geom.pane_content_px(rect);
        let pixel =
            painted[(y + host.font.cell_h / 2) * width + x + host.font.cell_w / 2] & 0xffffff;
        assert_eq!(
            pixel,
            if id == first { 0xff0000 } else { 0x00ff00 },
            "each pane must paint at its own content origin"
        );
    }
    for id in host.mux.active_pane_ids() {
        assert!(host.mux.focus(id));
        assert_eq!(
            host.mux
                .focused_mut()
                .emulator
                .take_damage()
                .dirty_cell_count(),
            0,
            "paint must drain each pane's damage"
        );
    }
    let chrome = frame(host);
    host.focus_border = (host.focus_border + 1) % 6;
    assert_ne!(
        frame(host),
        chrome,
        "split-pane focus chrome must use the chosen border color"
    );
}

fn verify_steady_four_pane_partial(host: &mut HostState) {
    while host.mux.pane_count() < 4 {
        host.mux
            .split_focused("/bin/cat", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        frame(host);
    }
    let _ = host.mux.focus(host.mux.active_pane_ids()[0]);
    frame(host);
    let mut retained = full_frame_oracle(host);
    let _ = host
        .mux
        .focused_mut()
        .emulator
        .feed(b"\x1b[2;2Hfour-pane partial");
    let partial = paint_retained(host, &mut retained);
    assert_eq!(
        partial.full_repaint_reason, None,
        "steady four-pane output must use partial raster"
    );
    assert!(partial.cells_painted > 0);
    assert!(partial.cells_painted < render_cells_painted(host));
    assert_eq!(retained, full_frame_oracle(host));
}

fn verify_same_layout_tab_switch(host: &mut HostState) {
    let original_tab = host.mux.selected_tab_index();
    let original_layout = layout_snapshot(&host.mux, host.mux.geom());
    let original_panes = original_layout.panes.len();
    host.mux.new_tab("/bin/cat", &[]).unwrap();
    while host.mux.pane_count() < original_panes {
        host.mux
            .split_focused("/bin/cat", &[], prismattyc_mux::Axis::Horizontal, 0.5)
            .unwrap();
        frame(host);
    }
    let sibling_layout = layout_snapshot(&host.mux, host.mux.geom());
    let mut original_slots: Vec<_> = original_layout.panes.iter().map(|pane| pane.slot).collect();
    let mut sibling_slots: Vec<_> = sibling_layout.panes.iter().map(|pane| pane.slot).collect();
    original_slots.sort_unstable_by_key(|slot| (slot.y, slot.x, slot.width, slot.height));
    sibling_slots.sort_unstable_by_key(|slot| (slot.y, slot.x, slot.width, slot.height));
    assert_eq!(
        original_slots, sibling_slots,
        "tab fixture must keep pane geometry"
    );

    let mut retained = frame(host);
    host.mux.select_tab(original_tab).unwrap();
    let switched = paint_retained(host, &mut retained);
    assert!(
        switched.full_repaint_reason.is_some(),
        "same-layout tab switch must repaint the full frame"
    );
    assert!(
        switched
            .guards
            .contains(render_diagnostics::Guard::LayoutTransition),
        "same-layout tab switch must record the layout-transition guard"
    );
    assert_eq!(retained, full_frame_oracle(host));
    verify_strip_pulse_damage(host);
    // Leave the pulse fixture with one tab. The pulse test below exercises
    // bounded pane-dot damage, while the strip pulse is conservatively full.
    host.mux.close_tab_at(1).unwrap();
    frame(host);
}

fn verify_strip_pulse_damage(host: &mut HostState) {
    let focused = host.mux.focused_id();
    host.mux.focus(focused);
    host.mux.focused_mut().last_output_at = Some(Instant::now());
    host.last_pulse_step = 0;
    let mut retained = frame(host);
    for step in 1..=PULSE_STEPS as u8 {
        host.last_pulse_step = step % PULSE_STEPS as u8;
        let painted = paint_retained(host, &mut retained);
        assert_eq!(painted.full_repaint_reason, None);
        assert_eq!(retained, full_frame_oracle(host));
    }
}

fn verify_idle_pulse_damage(host: &mut HostState) {
    let prior_tab_strip_mode = host.tab_strip_mode;
    host.tab_strip_mode = config::TabStripMode::Multi;
    assert!(!show_tab_strip(host));
    let focused = host.mux.focused_id();
    let now = Instant::now();
    host.mux.focused_mut().last_output_at = Some(now);
    host.last_pulse_step = 0;
    frame(host);
    let mut retained = full_frame_oracle(host);
    let started = Instant::now();
    let mut step = 0u8;
    while started.elapsed() < Duration::from_secs(10) {
        // Keep the pane in the active dot window while the terminal itself is
        // idle. The loop exercises every quantized pulse step in the Xvfb
        // retained-buffer oracle for a full ten seconds.
        host.mux.focus(focused);
        host.mux.focused_mut().last_output_at = Some(Instant::now());
        step = step.wrapping_add(1) % PULSE_STEPS as u8;
        host.last_pulse_step = step;
        let painted = paint_retained(host, &mut retained);
        assert_eq!(painted.full_repaint_reason, None);
        assert_eq!(retained, full_frame_oracle(host));
        thread::sleep(Duration::from_millis(70));
    }
    host.tab_strip_mode = prior_tab_strip_mode;
}

fn verify_render_guards(host: &mut HostState) {
    use render_diagnostics::Guard;
    host.pending_full_repaint = None;
    host.view_scroll = 0;
    host.preedit.text = "composition".to_owned();
    let _ = host.emulator.feed(b"\x1b[?1049h");
    frame(host);
    assert_eq!(
        host.render_frame.full_repaint_reason,
        Some(FullRepaintReason::AltScreen)
    );
    for guard in [Guard::AltScreen, Guard::Backend, Guard::Preedit] {
        assert!(
            host.render_frame.guards.contains(guard),
            "missing simultaneous guard: {guard:?}"
        );
    }
    host.bell_flash = Some(Instant::now());
    frame(host);
    assert!(host.render_frame.guards.contains(Guard::BellFlash));
    host.bell_flash = None;
    host.render_timer = config::RenderTimer::Osd;
    frame(host);
    assert!(host.render_frame.guards.contains(Guard::Osd));
    assert!(!host.render_frame.guards.contains(Guard::Backend));
}

fn verify_caption_click(host: &mut HostState) {
    start_walkthrough(host);
    frame(host);
    let band = walkthrough_band(host).expect("caption band");
    let show = band.show_me.expect("show me");
    host.pointer_px = Some((show.x as f64 + 1.0, show.y as f64 + 1.0));
    assert!(handle_caption_click(
        host,
        winit::event::MouseButton::Left,
        "/bin/cat",
        &[]
    ));
    let band = walkthrough_band(host).expect("band after show me");
    let skip = band.skip.expect("skip");
    host.pointer_px = Some((skip.x as f64 + 1.0, skip.y as f64 + 1.0));
    assert!(handle_caption_click(
        host,
        winit::event::MouseButton::Left,
        "/bin/cat",
        &[]
    ));
    host.pointer_px = Some((0.0, 0.0));
    assert!(!handle_caption_click(
        host,
        winit::event::MouseButton::Left,
        "/bin/cat",
        &[]
    ));
    host.pointer_px = Some((f64::NAN, 1.0));
    assert!(!handle_caption_click(
        host,
        winit::event::MouseButton::Left,
        "/bin/cat",
        &[]
    ));
}

/// Keep the framebuffer between frames, as the real softbuffer path does.
fn paint_retained(host: &mut HostState, pixels: &mut [u32]) -> RenderFrame {
    let size = host.window.inner_size();
    rasterize_frame(host, pixels, size.width, size.height, true);
    host.render_frame
}

fn paint_full(host: &mut HostState, pixels: &mut [u32]) -> RenderFrame {
    let size = host.window.inner_size();
    rasterize_frame(host, pixels, size.width, size.height, false);
    host.render_frame
}

/// Render a full-frame oracle without changing the host state used by the test.
fn full_frame_oracle(host: &mut HostState) -> Vec<u32> {
    let size = host.window.inner_size();
    let mut pixels = vec![0x12345678; size.width as usize * size.height as usize];
    let saved_frame = host.render_frame;
    let saved_pending = host.pending_full_repaint;
    let saved_pane_damage = std::mem::take(&mut host.pane_damage);
    let saved_last_pane_views = host.last_pane_views.clone();
    let saved_last_painted_cursor_rows = host.last_painted_cursor_rows.clone();
    let saved_last_layout_snapshot = host.last_layout_snapshot.clone();
    let saved_last_chrome_snapshot = host.last_chrome_snapshot.clone();
    let saved_last_transient_overlay_visible = host.last_transient_overlay_visible;
    let saved_last_frame_size = host.last_frame_size;
    let saved_background = host.background.take();
    let saved_palette_layout = host.palette_layout.take();
    rasterize_frame(host, &mut pixels, size.width, size.height, false);
    host.render_frame = saved_frame;
    host.pending_full_repaint = saved_pending;
    host.pane_damage = saved_pane_damage;
    host.last_pane_views = saved_last_pane_views;
    host.last_painted_cursor_rows = saved_last_painted_cursor_rows;
    host.last_layout_snapshot = saved_last_layout_snapshot;
    host.last_chrome_snapshot = saved_last_chrome_snapshot;
    host.last_transient_overlay_visible = saved_last_transient_overlay_visible;
    host.last_frame_size = saved_last_frame_size;
    host.background = saved_background;
    host.palette_layout = saved_palette_layout;
    pixels
}

fn assert_partial_matches_full(host: &mut HostState, pixels: &mut [u32]) {
    let painted = paint_retained(host, pixels);
    assert_eq!(
        painted.full_repaint_reason, None,
        "a steady TUI row update must use partial raster"
    );
    assert!(painted.cells_painted > 0);
    assert!(painted.cells_painted < render_cells_painted(host));
    assert_eq!(
        pixels,
        full_frame_oracle(host),
        "partial pixels must equal a full repaint"
    );
}

fn verify_alt_screen_partial_and_scrollback(host: &mut HostState) {
    let size = host.window.inner_size();
    let mut pixels = vec![0x12345678; size.width as usize * size.height as usize];
    let _ = host.emulator.feed(b"\x1b[2J\x1b[H\x1b[?25lprimary");
    let initial = paint_full(host, &mut pixels);
    assert!(initial.cells_painted > 0);
    let primary = pixels.clone();

    let _ = host.emulator.feed(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?25lTUI");
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::AltScreen)
    );
    assert_eq!(pixels, full_frame_oracle(host));
    let _ = host.emulator.feed(b"\x1b[4;1Hstatus: running");
    assert_partial_matches_full(host, &mut pixels);
    let _ = host.emulator.feed(b"\x1b[4;1H\x1b[2Kdone");
    assert_partial_matches_full(host, &mut pixels);
    let idle = pixels.clone();
    assert_eq!(paint_retained(host, &mut pixels).cells_painted, 0);
    assert_eq!(pixels, idle);

    let _ = host.emulator.feed(b"\x1b[?1049l");
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::AltScreen)
    );
    assert_eq!(
        pixels, primary,
        "leaving a TUI must restore the primary grid"
    );
    let _ = host.emulator.feed(b"\x1b[5;1Hprimary update");
    assert_partial_matches_full(host, &mut pixels);

    let rows = host.emulator.screen().rows();
    for row in 0..rows + 10 {
        let _ = host.emulator.feed(format!("\r\nline {row:03}").as_bytes());
    }
    let settled = paint_retained(host, &mut pixels);
    assert_eq!(
        settled.full_repaint_reason, None,
        "a newly visible scrollbar stays on the bounded partial path"
    );
    assert!(settled.cells_painted > 0);
    assert!(host.emulator.screen().max_view_scroll() >= 5);
    host.view_scroll = 3;
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::Scrollback)
    );
    assert_eq!(pixels, full_frame_oracle(host));
    let idle = pixels.clone();
    let painted = paint_retained(host, &mut pixels);
    assert_eq!(
        painted.full_repaint_reason, None,
        "an idle unchanged scrollback view needs no full repaint"
    );
    assert_eq!(painted.cells_painted, 0);
    assert_eq!(pixels, idle);
    host.view_scroll = 5;
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::Scrollback)
    );
    assert_eq!(pixels, full_frame_oracle(host));
    let _ = host.emulator.feed(b"\r\noutput while scrolled back");
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::Scrollback)
    );
    assert_eq!(
        pixels,
        full_frame_oracle(host),
        "live damage must not use live row indices in scrollback"
    );
    host.view_scroll = 0;
    assert_eq!(
        paint_retained(host, &mut pixels).full_repaint_reason,
        Some(FullRepaintReason::Scrollback)
    );
    assert_eq!(pixels, full_frame_oracle(host));
}

#[test]
fn pane_view_events_cover_both_screen_transitions_and_scrollback_output() {
    for (previous, current, damaged, expected) in [
        (None, (false, 0), false, (false, false)),
        (None, (false, 3), false, (false, true)),
        (None, (true, 0), false, (true, false)),
        (Some((false, 0)), (true, 0), true, (true, false)),
        (Some((true, 0)), (true, 0), true, (false, false)),
        (Some((true, 0)), (false, 0), false, (true, false)),
        (Some((false, 0)), (false, 3), false, (false, true)),
        (Some((false, 3)), (false, 3), false, (false, false)),
        (Some((false, 3)), (false, 3), true, (false, true)),
        (Some((false, 3)), (false, 4), false, (false, true)),
        (Some((false, 3)), (false, 0), false, (false, true)),
        (Some((false, 3)), (true, 0), true, (true, true)),
    ] {
        assert_eq!(pane_view_repaint(previous, current, damaged), expected);
    }
}

fn verify_decision_handlers(host: &mut HostState) {
    let args: Vec<String> = Vec::new();
    host.space_rail.names = vec!["pt308-test".to_string()];
    host.space_rail.leave();
    apply_space_rail_focus_action(host);
    assert!(
        host.space_rail.focus.is_some(),
        "space rail focus action must focus the current chip"
    );
    host.space_rail.leave();
    apply_save_space_action(host);
    assert!(
        host.space_rail
            .edit
            .as_ref()
            .is_some_and(|edit| edit.target.is_none()),
        "save-space action must open the new-space editor"
    );
    host.space_rail.edit = None;

    host.context_menu_target = Some(ContextMenuTarget::SpaceChip(0));
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::SpaceChip));
    activate_context_menu(host, 4, "/bin/sh", &args);
    assert_eq!(
        host.space_rail.edit.as_ref().and_then(|edit| edit.target),
        Some(0),
        "space rename action must reach the rail"
    );
    host.space_rail.edit = None;
    close_context_menu(host);

    host.context_menu_target = Some(ContextMenuTarget::SpaceChip(99));
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::SpaceChip));
    activate_context_menu(host, 3, "/bin/sh", &args);
    assert_eq!(
        host.context_menu.as_ref().and_then(|menu| menu.confirm),
        Some(3),
        "the first save activation must arm confirmation"
    );
    close_context_menu(host);

    host.tab_strip_mode = config::TabStripMode::Always;
    App::refit_geom(host, host.window.inner_size(), Some("decision test"));
    let stride = host.window.inner_size().width as usize;
    let close_x = (0..stride).find(|x| {
        matches!(
            host.mux.tab_strip_hit(*x, 0, stride, false),
            Some(mux::StripHit::Tab { close: true, .. })
        )
    });
    let Some(close_x) = close_x else {
        panic!("real tab strip must expose a close hit for the decision test");
    };

    host.tab_rename = None;
    host.pointer_px = Some((close_x as f64, 0.0));
    assert_eq!(
        handle_strip_click(host, MouseButton::Right),
        StripClickResult::Handled
    );
    assert!(
        host.tab_rename.is_some(),
        "title-row click must rename the tab"
    );
    cancel_tab_rename(host);

    host.pointer_px = Some((close_x as f64, host.font.cell_h as f64));
    assert_eq!(
        handle_strip_click(host, MouseButton::Right),
        StripClickResult::Handled
    );
    assert!(
        host.tab_rename.is_none(),
        "the first row after the title must not rename the tab"
    );

    verify_mux_apply_wrappers(host, &args);
    verify_pane_context_apply_wrapper(host, &args);
}

fn verify_mux_apply_wrappers(host: &mut HostState, args: &[String]) {
    let _ =
        apply_mux_navigation_command(host, MuxNavigationCommand::Focus(mux::FocusDirection::Left));
    let _ = apply_mux_navigation_command(host, MuxNavigationCommand::SwapPane(1));
    let _ = apply_mux_navigation_command(host, MuxNavigationCommand::RotatePanes(1));
    let _ = apply_mux_navigation_command(host, MuxNavigationCommand::FocusLastPane);
    let _ = apply_mux_navigation_command(host, MuxNavigationCommand::LastTab);

    let _ = apply_mux_tab_command(host, MuxTabCommand::NewTab, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::CloseTab, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::NextTab, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::PrevTab, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::SelectTab(0), "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::MovePaneToTab(1), "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::BreakPane, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::JoinPane, "/bin/sh", args);
    let _ = apply_mux_tab_command(host, MuxTabCommand::MoveTab(1), "/bin/sh", args);
}

fn verify_pane_context_apply_wrapper(host: &mut HostState, args: &[String]) {
    let pane = host.mux.focused_id();
    for action in [
        PaneContextAction::SplitRight,
        PaneContextAction::SplitDown,
        PaneContextAction::Zoom,
        PaneContextAction::Rename,
        PaneContextAction::MovePaneNextTab,
        PaneContextAction::MoveToSpace,
        PaneContextAction::Detach,
        PaneContextAction::Close,
        PaneContextAction::SaveSpace,
    ] {
        let _ = apply_pane_context_action(host, pane, action, "/bin/sh", args);
        if matches!(action, PaneContextAction::MoveToSpace) {
            host.space_picker = None;
        }
        if matches!(action, PaneContextAction::SaveSpace) {
            host.space_rail.edit = None;
        }
    }
}

pub(super) fn session_naming_in_private_window() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_in_private_display("session_prompt::tests::real_window_session_naming");
        return;
    }
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::env::set_var("PMUX", bin.join("pmux"));
    let socket = host_mux_socket().unwrap();
    std::env::set_var("PMUX_SESSION_AGENTS", socket.with_extension("agents.json"));
    let _daemon = ChildGuard(
        Command::new(bin.join("pmuxd"))
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("build pmuxd before the session naming fixture"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while attach_log::live_snapshot().is_none() {
        assert!(Instant::now() < deadline, "private mux did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    struct NamingProof {
        app: App,
        completed: bool,
    }
    impl ApplicationHandler<UserAction> for NamingProof {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let id = self.app.open_window(event_loop, false).unwrap();
            let host = self.app.windows.get_mut(&id).unwrap();
            assert_eq!(host.mux.pane_count(), 1);
            assert!(show_tab_strip(host), "single-pane launch must show the tab");
            assert!(host.mux.geom().top_chrome_px > 0);
            let before = attach_log::live_snapshot().unwrap().sessions.len();
            let plain = frame(host);
            handle_mux_command(
                host,
                MuxCommand::NewTab,
                keybind::Action::NewTab,
                "/bin/sh",
                &[],
            );
            assert_eq!(host.session_prompt.as_ref().unwrap().buffer, "session-1");
            assert_eq!(
                attach_log::live_snapshot().unwrap().sessions.len(),
                before,
                "popup must precede creation"
            );
            let popup = frame(host);
            assert_ne!(plain, popup, "popup must be painted");
            if let Some(dir) = std::env::var_os("SESSION_NAMING_ARTIFACT_DIR") {
                std::fs::create_dir_all(&dir).unwrap();
                let size = host.window.inner_size();
                write_present_png(
                    &PathBuf::from(dir).join("new-session.png"),
                    &popup,
                    size.width,
                    size.height,
                )
                .unwrap();
            }
            session_prompt::dispatch_key(host, &Key::Named(NamedKey::Escape), false);
            assert!(host.session_prompt.is_none());
            assert_eq!(frame(host), plain, "cancel must remove popup pixels");
            assert_eq!(attach_log::live_snapshot().unwrap().sessions.len(), before);
            session_prompt::create_pane(host, None);
            session_prompt::dispatch_key(host, &Key::Character("reviewer".into()), false);
            session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
            assert!(host.session_prompt.is_none(), "creation failed");
            let pane = host.mux.focused_id();
            let session = host.mux.attach_session_of(pane).unwrap().to_string();
            let snapshot = attach_log::live_snapshot().unwrap();
            let live = snapshot
                .sessions
                .iter()
                .find(|s| s.name == "reviewer")
                .unwrap();
            assert_eq!(live.agent_id.as_deref(), Some("reviewer"));
            let child = live.windows[0].panes[0].child_pid;
            assert_eq!(host.mux.tab_count(), 2);
            begin_pane_rename(host);
            session_prompt::dispatch_key(host, &Key::Character("bad name".into()), false);
            session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
            assert!(
                host.session_prompt.is_some(),
                "invalid input must keep the popup open"
            );
            session_prompt::finish(host, false);
            begin_pane_rename(host);
            session_prompt::dispatch_key(host, &Key::Character("astra-spaces".into()), false);
            session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
            assert!(host.session_prompt.is_none());
            assert_eq!(host.mux.focused_id(), pane);
            assert_eq!(host.mux.attach_session_of(pane), Some(session.as_str()));
            let snapshot = attach_log::live_snapshot().unwrap();
            let renamed = snapshot
                .sessions
                .iter()
                .find(|s| s.name == "astra-spaces")
                .unwrap();
            assert_eq!(renamed.agent_id.as_deref(), Some("astra-spaces"));
            assert_eq!(renamed.windows[0].panes[0].child_pid, child);
            // The new-space flow asks for the first session name before any spawn.
            session_prompt::create_space(host, "work".into());
            assert_eq!(host.session_prompt.as_ref().unwrap().buffer, "work-1");
            session_prompt::finish(host, false);
            assert!(!space_json_exists("work"));
            let before = attach_log::live_snapshot().unwrap().sessions.len();
            for axis in [None, Some(prismattyc_mux::Axis::Horizontal)] {
                session_prompt::create_pane(host, axis);
                session_prompt::dispatch_key(host, &Key::Named(NamedKey::Tab), false);
                session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
                assert!(host.session_prompt.is_none());
                assert!(host.mux.attach_session_of(host.mux.focused_id()).is_none());
                assert!(host.mux.is_retained_local_terminal(host.mux.focused_id()));
                assert_eq!(attach_log::live_snapshot().unwrap().sessions.len(), before);
            }
            config::save_preference(
                &config::config_path(),
                "session_naming",
                toml_edit::value("auto"),
            )
            .unwrap();
            for (index, axis) in [None, Some(prismattyc_mux::Axis::Horizontal)]
                .into_iter()
                .enumerate()
            {
                session_prompt::create_pane(host, axis);
                assert!(
                    host.session_prompt.is_none(),
                    "automatic creation must not leave a dialog"
                );
                assert!(host.mux.attach_session_of(host.mux.focused_id()).is_some());
                assert_eq!(
                    attach_log::live_snapshot().unwrap().sessions.len(),
                    before + index + 1
                );
            }
            config::save_preference(
                &config::config_path(),
                "session_naming",
                toml_edit::value("ask"),
            )
            .unwrap();
            session_prompt::create_pane(host, None);
            assert!(host.session_prompt.is_some());
            session_prompt::finish(host, false);
            // A view with blank shells parks intact under a stable Space owner.
            host.mux.space_id = Some("owner-one".into());
            let layout = host.mux.tab_layouts();
            let focused = host.mux.focused_id();
            let local_pids: Vec<_> = host
                .mux
                .tab_panes()
                .iter()
                .flat_map(|(_, panes)| panes)
                .filter(|pane| host.mux.is_retained_local_terminal(**pane))
                .map(|pane| (*pane, host.mux.pane(*pane).unwrap().child_pid()))
                .collect();
            assert!(!local_views::switch(host, Some("owner-two".into())).unwrap());
            assert!(!local_views::has_local(&host.mux));
            assert_eq!(host.mux.pane_count(), 1);
            local_views::drain(host);
            assert!(local_views::switch(host, Some("owner-one".into())).unwrap());
            assert_eq!(host.mux.tab_layouts(), layout);
            assert_eq!(host.mux.focused_id(), focused);
            for (pane, pid) in local_pids {
                assert_eq!(host.mux.pane(pane).unwrap().child_pid(), pid);
            }
            self.completed = true;
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
        ..Default::default()
    };
    let app = App::new(cli, config, None, event_loop.create_proxy()).unwrap();
    let mut proof = NamingProof {
        app,
        completed: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.completed);
    std::fs::write(std::env::var_os(RESULT_ENV).unwrap(), b"complete").unwrap();
}
