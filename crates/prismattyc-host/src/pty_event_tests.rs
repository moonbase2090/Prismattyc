//! What `App::drain_pty` and `App::user_event` do with a live window.
//!
//! Bells, attention, mail, and timers change host state the window shows.
//! `Wake` leaves that state untouched. A new window and a config window are
//! separate hosts. An accessibility click selects the tab it names.

use super::*;
use std::time::{Duration, Instant};
use winit::platform::x11::EventLoopBuilderExtX11;

const CHILD_ENV: &str = "PRISMATTYC_RENDER_TEST_CHILD";

#[test]
fn drain_pty_and_user_event_update_the_window() {
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
        "pty_event_tests::drain_pty_and_user_event_update_the_window",
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
            verify_wake_leaves_a_queued_bell(&mut self.app, event_loop, id);
            verify_bell_presentation(self.app.windows.get_mut(&id).unwrap());
            verify_attention_and_mail(self.app.windows.get_mut(&id).unwrap());
            verify_timers_and_hover(self.app.windows.get_mut(&id).unwrap());
            verify_attach_exit(self.app.windows.get_mut(&id).unwrap());
            verify_parked_exit(self.app.windows.get_mut(&id).unwrap());
            verify_new_windows_and_accesskit(&mut self.app, event_loop, id);
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
    let cli = Cli::parse(["--no-splash", "/bin/sh"].into_iter().map(String::from)).unwrap();
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
    assert!(proof.done, "the event loop must run the drain assertions");
}

fn verify_wake_leaves_a_queued_bell(app: &mut App, event_loop: &ActiveEventLoop, id: WindowId) {
    {
        let host = app.windows.get_mut(&id).unwrap();
        prepare(host);
        host.visual_bell = true;
        host.audible_bell = true;
        host.bell_toaster = true;
        host.os_notify_bell = true;
        host.window_focused = false;
        run_printf_queued(host, r"\007OK%d\n", 1);
        assert!(
            host.bell_flash.is_none(),
            "parsing output does not present the bell by itself"
        );
    }
    let windows = app.windows.len();
    app.user_event(event_loop, UserAction::Wake);
    assert_eq!(app.windows.len(), windows);
    let host = app.windows.get_mut(&id).unwrap();
    assert!(
        host.bell_flash.is_none(),
        "Wake does not present a queued bell"
    );
    assert!(
        host.bell_toasts.is_empty(),
        "Wake does not toast a queued bell"
    );
    let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
    assert!(
        host.bell_flash.is_some(),
        "the queued BEL flashes the window"
    );
    assert_eq!(toast_labels(host), vec![" bell "]);
    assert!(host.last_bell_sound.is_some(), "the audible bell plays");
    assert!(
        host.last_bell_notify.is_some(),
        "an unfocused window notifies"
    );
}

fn verify_bell_presentation(host: &mut HostState) {
    silence(host);
    host.visual_bell = true;
    host.pane_visual_bell = true;
    host.bell_toaster = true;
    host.window_occluded = false;
    host.last_pulse_step = 255;
    run_printf(host, r"\007OK%d\n", 2);
    assert!(
        !host.pane_bells.is_empty(),
        "a visible pane rings its own bell"
    );
    assert!(
        toast_labels(host).is_empty(),
        "a visible pane bell is not also a toast"
    );
    assert_ne!(host.last_pulse_step, 255, "fresh output advances the pulse");

    silence(host);
    host.visual_bell = true;
    host.pane_visual_bell = true;
    host.bell_toaster = true;
    host.window_occluded = true;
    run_printf(host, r"\007OK%d\n", 3);
    assert!(
        host.pane_bells.is_empty(),
        "an occluded window does not keep a pane ring"
    );
    assert_eq!(toast_labels(host), vec![" bell "]);

    silence(host);
    host.audible_bell = true;
    let held = Instant::now();
    host.last_bell_sound = Some(held);
    run_printf(host, r"\007OK%d\n", 4);
    assert_eq!(
        host.last_bell_sound,
        Some(held),
        "a second bell inside the gap does not play again"
    );

    silence(host);
    host.bell_toaster = true;
    let pane = host.mux.focused_id();
    host.bell_toasts.push(BellToast {
        pane,
        until: Instant::now() + Duration::from_secs(10),
        label: "old".to_string(),
        status: None,
    });
    run_printf(host, r"\007OK%d\n", 5);
    assert_eq!(toast_labels(host), vec![" bell "]);

    silence(host);
    host.os_notify_bell = true;
    host.window_focused = true;
    run_printf(host, r"\007OK%d\n", 6);
    assert!(
        host.last_bell_notify.is_none(),
        "a focused window does not raise a bell notification"
    );
}

fn verify_attention_and_mail(host: &mut HostState) {
    silence(host);
    host.attention_sound = true;
    host.os_notify_attention = true;
    host.window_focused = true;
    run_printf(host, r"\033]9;review the diff\033\\OK%d\n", 7);
    assert_eq!(
        host.pending_attention_announce.as_deref(),
        Some("main needs you — default: review the diff")
    );
    assert!(
        host.last_attention_notify.is_empty(),
        "a focused selected tab does not notify"
    );
    assert!(host.last_bell_sound.is_some(), "attention plays the bell");

    // A later chunk with no OSC retires the latch. The next OSC is then
    // collected for the host announce. A chunk that still holds the latch
    // consumes the OSC inside the pane and the host never hears it.
    retire_latched_attention(host, 70);
    assert!(host
        .mux
        .rename_window(host.mux.active_window(), "default")
        .expect("tab title can match the session name"));
    assert_eq!(
        host.mux.pane_tab_title(host.mux.focused_id()).as_deref(),
        Some("default")
    );
    host.window_focused = false;
    host.last_attention_notify.clear();
    let sounded = Instant::now();
    host.last_bell_sound = Some(sounded);
    run_printf(host, r"\033]9;same name\033\\OK%d\n", 8);
    assert_eq!(
        host.pending_attention_announce.as_deref(),
        Some("default needs you: same name")
    );
    assert_eq!(host.last_attention_notify.len(), 1);
    assert_eq!(
        host.last_bell_sound,
        Some(sounded),
        "attention inside the sound gap stays quiet"
    );

    let stamped = host.last_attention_notify[0].1;
    retire_latched_attention(host, 71);
    run_printf(host, r"\033]9;gap note\033\\OK%d\n", 9);
    assert_eq!(host.last_attention_notify[0].1, stamped);
    assert_eq!(
        host.pending_attention_announce.as_deref(),
        Some("default needs you: gap note")
    );

    let pane = host.mux.focused_id();
    host.last_attention_notify = vec![(pane, Instant::now() - Duration::from_secs(6))];
    retire_latched_attention(host, 72);
    run_printf(host, r"\033]9;stale note\033\\OK%d\n", 10);
    assert!(
        host.last_attention_notify[0].1.elapsed() < Duration::from_secs(2),
        "a stale attention notification is sent again"
    );
    assert_eq!(
        host.pending_attention_announce.as_deref(),
        Some("default needs you: stale note")
    );

    let pane = host.mux.focused_id();
    host.last_mail_depths.insert(pane.get(), 1);
    assert!(host.mux.apply_mail_attention(pane.get(), 4));
    let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
    assert_eq!(
        host.pending_mail_announce.as_deref(),
        Some("default: 4 mail")
    );
}

fn verify_timers_and_hover(host: &mut HostState) {
    silence(host);
    host.bell_flash = Some(Instant::now() - Duration::from_secs(1));
    host.bell_toasts.push(BellToast {
        pane: host.mux.focused_id(),
        until: Instant::now() - Duration::from_secs(1),
        label: " bell ".to_string(),
        status: None,
    });
    host.title_notice = Some(LiveTitleNotice {
        tab: 0,
        handle: 0,
        title: "old".to_string(),
        until: Instant::now() - Duration::from_secs(1),
    });
    host.splash = Some(splash::Splash {
        page: splash::Page::Main,
        tip: 0,
        shown_at: Instant::now() - Duration::from_secs(5),
        last_tick_ms: 0,
        animated: true,
        resume: false,
    });
    host.border_anim = Some(Instant::now() - Duration::from_secs(5));
    host.light_cycle_ms = 280;
    host.window_occluded = false;
    let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
    assert!(host.bell_flash.is_none(), "an old flash settles");
    assert!(toast_labels(host).is_empty(), "an expired toast leaves");
    assert!(
        host.title_notice.is_none(),
        "an expired title notice leaves"
    );
    assert_ne!(
        host.splash.as_ref().unwrap().last_tick_ms,
        0,
        "a visible splash advances"
    );
    assert!(host.border_anim.is_none(), "a finished light cycle settles");

    host.splash.as_mut().unwrap().last_tick_ms = 7;
    host.window_occluded = true;
    host.border_anim = Some(Instant::now());
    host.light_cycle_ms = 1_000_000;
    host.last_cycle_step = 255;
    host.mux
        .focused_mut()
        .emulator
        .feed(b"\x1b[Hhttps://moonbase2090.com/");
    host.hyperlink_hover.update(
        host.mux.focused_id(),
        host.mux.focused().emulator.screen(),
        0,
        0,
        0,
    );
    assert_eq!(
        host.hyperlink_hover.url(),
        Some("https://moonbase2090.com/")
    );
    assert!(App::drain_pty(
        host,
        Instant::now() - Duration::from_millis(5)
    ));
    assert_eq!(host.splash.as_ref().unwrap().last_tick_ms, 7);
    assert_eq!(host.last_cycle_step, 0);
    assert!(
        host.hyperlink_hover.url() == Some("https://moonbase2090.com/"),
        "an expired budget does not parse PTY bytes"
    );

    host.window_occluded = false;
    host.window_focused = true;
    run_printf(host, r"OK%d\n", 11);
    assert!(
        host.hyperlink_hover.url().is_none(),
        "new pane bytes clear the hyperlink"
    );
}

fn verify_attach_exit(host: &mut HostState) {
    silence(host);
    host.toasts = config::ToastLevel::All;
    let split = host
        .mux
        .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
        .expect("the window can split");
    assert_eq!(host.mux.pane_count(), 2);
    host.mux
        .pane(split)
        .expect("the split pane is live")
        .try_send_bytes(b"printf 'SPLIT\\n'\n".to_vec())
        .expect("the split shell is reading");
    let titled = Instant::now() + Duration::from_secs(4);
    while Instant::now() < titled && !x11_window_title(&host.window).contains("2 panes") {
        let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
        std::thread::sleep(Duration::from_millis(10));
    }
    let title = x11_window_title(&host.window);
    assert!(
        title.contains("2 panes"),
        "a new pane renames the window: {title}"
    );
    host.mux
        .mark_attach_session(split, "9".into(), "seat".into());
    host.mux
        .pane(split)
        .expect("the split pane is live")
        .try_send_bytes(b"exit 0\n".to_vec())
        .expect("the split shell is reading");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && host.mux.pane_count() != 1 {
        let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host.mux.pane_count(), 1, "the exited pane closes");
    let title = x11_window_title(&host.window);
    assert!(
        title.contains("1 panes"),
        "closing a pane renames the window: {title}"
    );
    assert!(
        host.bell_toasts
            .iter()
            .any(|toast| toast.label.contains("clean exit cleanup failed")),
        "cleanup retries when the space cannot be updated: {:?}",
        host.bell_toasts
            .iter()
            .map(|toast| toast.label.clone())
            .collect::<Vec<_>>()
    );
    assert!(host
        .pending_exited_cleanups
        .iter()
        .any(|(name, id)| name == "seat" && id == "9"));
}

fn verify_parked_exit(host: &mut HostState) {
    host.pending_exited_cleanups.clear();
    let pane = host.mux.focused_id();
    host.mux.retain_local_terminal(pane);
    host.mux
        .mark_attach_session(pane, "8".into(), "parked-seat".into());
    local_views::switch(host, Some("other-space".into())).expect("the shell view parks");
    assert!(
        host.local_views.parked.contains_key(&None),
        "the live shell moves to the parked list"
    );
    host.local_views
        .parked
        .get_mut(&None)
        .expect("parked shell")
        .mux
        .focused()
        .try_send_bytes(b"exit 0\n".to_vec())
        .expect("the parked shell is reading");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline
        && !host
            .pending_exited_cleanups
            .iter()
            .any(|(name, id)| name == "parked-seat" && id == "8")
    {
        let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        host.pending_exited_cleanups
            .iter()
            .any(|(name, id)| name == "parked-seat" && id == "8"),
        "a parked attach exit is retried: {:?}",
        host.pending_exited_cleanups
    );
}

fn verify_new_windows_and_accesskit(app: &mut App, event_loop: &ActiveEventLoop, id: WindowId) {
    let windows = app.windows.len();
    app.user_event(event_loop, UserAction::NewWindow);
    assert_eq!(app.windows.len(), windows + 1);
    // The config window spawns `$VISUAL` or `$EDITOR`, then `nano`. Pin an
    // editor that exists in the test image so the window actually opens.
    std::env::remove_var("VISUAL");
    std::env::set_var("EDITOR", "/bin/sh");
    app.user_event(event_loop, UserAction::OpenConfig);
    assert_eq!(app.windows.len(), windows + 2);
    assert_eq!(
        app.windows
            .values()
            .filter(|host| !host.cache_writer)
            .count(),
        1,
        "only the config window opts out of the shared cache"
    );

    {
        let host = app.windows.get_mut(&id).unwrap();
        host.mux
            .new_tab("/bin/sh", &[])
            .expect("a second tab opens");
        assert_eq!(host.mux.selected_tab_index(), 1);
    }
    app.user_event(event_loop, accesskit_action(id, accesskit::Action::Focus));
    assert_eq!(app.windows[&id].mux.selected_tab_index(), 1);
    app.user_event(event_loop, accesskit_action(id, accesskit::Action::Click));
    assert_eq!(
        app.windows[&id].mux.selected_tab_index(),
        0,
        "clicking the first tab selects it"
    );
}

fn retire_latched_attention(host: &mut HostState, n: u32) {
    run_printf(host, "OK%d\\n", n);
    assert!(
        host.mux.focused().attention.is_none(),
        "plain output retires a latched attention"
    );
}

fn prepare(host: &mut HostState) {
    host.bell_toaster_ms = Duration::from_secs(10);
    host.toasts = config::ToastLevel::All;
    silence(host);
}

fn silence(host: &mut HostState) {
    host.visual_bell = false;
    host.pane_visual_bell = false;
    host.audible_bell = false;
    host.bell_toaster = false;
    host.os_notify_bell = false;
    host.os_notify_attention = false;
    host.attention_sound = false;
    host.window_occluded = false;
    host.window_focused = true;
    host.bell_flash = None;
    host.bell_toasts.clear();
    host.pane_bells.cancel();
    host.last_bell_sound = None;
    host.last_bell_notify = None;
    host.pending_attention_announce = None;
    host.pending_mail_announce = None;
}

fn toast_labels(host: &HostState) -> Vec<&str> {
    host.bell_toasts
        .iter()
        .map(|toast| toast.label.as_str())
        .collect()
}

fn run_printf(host: &mut HostState, body: &str, n: u32) {
    let marker = format!("OK{n}");
    let script = format!("printf '{body}' {n}");
    drive(host, &script, &marker);
}

/// Parse the shell output without presenting it, so a later `drain_pty` is
/// the call that turns a queued BEL into a flash.
fn run_printf_queued(host: &mut HostState, body: &str, n: u32) {
    let marker = format!("OK{n}");
    let script = format!("printf '{body}' {n}\n");
    host.mux
        .focused()
        .try_send_bytes(script.into_bytes())
        .expect("the shell is still reading");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let _ = host
            .mux
            .drain_all_until(Instant::now() + Duration::from_millis(40));
        if pane_text(host).contains(&marker) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("shell never showed {marker}\n{}", pane_text(host));
}

fn drive(host: &mut HostState, script: &str, marker: &str) {
    host.mux
        .focused()
        .try_send_bytes(format!("{script}\n").into_bytes())
        .expect("the shell is still reading");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let _ = App::drain_pty(host, Instant::now() + Duration::from_millis(40));
        if pane_text(host).contains(marker) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("shell never showed {marker}\n{}", pane_text(host));
}

fn pane_text(host: &HostState) -> String {
    let pane = host.mux.focused();
    pane.emulator
        .screen()
        .viewport_range()
        .map(|range| pane.emulator.screen().extract_text(range))
        .unwrap_or_default()
}

/// `winit` reports an empty title on X11. The server property is the title
/// the window manager shows.
fn x11_window_title(window: &winit::window::Window) -> String {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::protocol::xproto::ConnectionExt;

    let handle = window.window_handle().expect("the X11 window handle");
    let xid = match handle.as_raw() {
        RawWindowHandle::Xlib(handle) => handle.window as u32,
        RawWindowHandle::Xcb(handle) => handle.window.get(),
        other => panic!("expected an X11 window, got {other:?}"),
    };
    let (conn, _) = x11rb::rust_connection::RustConnection::connect(None).expect("X display");
    let net = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .expect("intern _NET_WM_NAME")
        .reply()
        .expect("_NET_WM_NAME atom")
        .atom;
    let utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .expect("intern UTF8_STRING")
        .reply()
        .expect("UTF8_STRING atom")
        .atom;
    let value = conn
        .get_property(false, xid, net, utf8, 0, 1024)
        .expect("read _NET_WM_NAME")
        .reply()
        .expect("_NET_WM_NAME reply")
        .value;
    String::from_utf8(value).unwrap_or_default()
}

fn accesskit_action(window: WindowId, action: accesskit::Action) -> UserAction {
    UserAction::AccessKit(accesskit_winit::Event {
        window_id: window,
        window_event: accesskit_winit::WindowEvent::ActionRequested(accesskit::ActionRequest {
            action,
            target_tree: accesskit::TreeId::ROOT,
            target_node: accesskit::NodeId(a11y::TAB_BASE),
            data: None,
        }),
    })
}
