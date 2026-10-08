//! Real daemon, two host windows, exclusive ownership, and rendered evidence.

use super::*;
use prismattyc_mux::local_socket::UnixStream;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use winit::platform::x11::EventLoopBuilderExtX11;

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn isolated_space_windows_create_move_and_render() {
    if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
        render_window_tests::run_in_private_display(
            "space_open_window_tests::isolated_space_windows_create_move_and_render",
        );
        return;
    }
    let binaries = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let pmux = binaries.join("pmux");
    let pmuxd = binaries.join("pmuxd");
    assert!(
        pmux.is_file() && pmuxd.is_file(),
        "build pmux and pmuxd at this checkout before the window fixture"
    );
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().unwrap();
    let _daemon = Daemon(
        Command::new(pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "private daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(["--no-splash", "/bin/cat"].into_iter().map(String::from)).unwrap();
    let file_config = config::ConfigFile {
        async_file_writes: Some(true),
        ..config::ConfigFile::default()
    };
    let app = App::new(cli, file_config, None, event_loop.create_proxy()).unwrap();
    let mut proof = Proof {
        app,
        windows: Vec::new(),
        phase: 0,
        changed: Instant::now(),
        started: Instant::now(),
        original: Vec::new(),
        split_pane: 0,
        complete: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.complete, "all space fixture phases must complete");
    std::fs::write(
        std::env::var_os("PRISMATTYC_RENDER_TEST_RESULT").unwrap(),
        "complete",
    )
    .unwrap();
}

struct Proof {
    app: App,
    windows: Vec<WindowId>,
    phase: usize,
    changed: Instant,
    started: Instant,
    original: Vec<(u64, u64, u32)>,
    split_pane: u64,
    complete: bool,
}

fn space_session(name: &str) -> prismattyc_mux::SessionSnapshot {
    let space = load_space(&spaces_dir(), name).unwrap();
    attach_log::live_snapshot()
        .unwrap()
        .sessions
        .into_iter()
        .find(|s| s.space_id == space.id)
        .unwrap()
}

fn text(host: &HostState) -> String {
    let screen = host.mux.focused().emulator.screen();
    screen
        .viewport_range()
        .map(|range| screen.extract_text(range))
        .unwrap_or_default()
}

fn command(args: &[&str]) {
    let result = Command::new(pmux_bin()).args(args).output().unwrap();
    assert!(
        result.status.success(),
        "pmux {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn split(session: &prismattyc_mux::SessionSnapshot) {
    let mut socket = UnixStream::connect(host_mux_socket().unwrap()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let request = prismattyc_mux::ControlRequest::Split {
        version: prismattyc_mux::PROTOCOL_VERSION,
        request_id: 1,
        window_id: session.windows[0].id,
        target_pane_id: session.windows[0].panes[0].id,
        axis: prismattyc_mux::AxisWire::Horizontal,
        ratio: 0.5,
        spawn: prismattyc_mux::SpawnSpec {
            program: "/bin/sh".into(),
            argv: vec![],
            cwd: None,
            env: BTreeMap::new(),
        },
        client_id: None,
    };
    serde_json::to_writer(&mut socket, &request).unwrap();
    socket.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    let reply: prismattyc_mux::ControlResponse = serde_json::from_str(&line).unwrap();
    assert!(
        matches!(reply.body, prismattyc_mux::ControlResponseBody::Ok { .. }),
        "{line}"
    );
}

fn capture(host: &mut HostState, label: &str) {
    App::paint(host).unwrap();
    let size = host.window.inner_size();
    let mut pixels = vec![0; size.width as usize * size.height as usize];
    rasterize_frame(host, &mut pixels, size.width, size.height, false);
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/spaces-host-proof");
    write_present_png(
        &output.join(format!("{label}.png")),
        &pixels,
        size.width,
        size.height,
    )
    .unwrap();
}

fn verify_empty_space_startup(app: &mut App, event_loop: &ActiveEventLoop, path: &Path) {
    let windows = std::mem::take(&mut app.windows);
    std::env::set_var("PMUX_SPACE", "a");
    std::env::set_var("PMUX_VIEW_PATH", path);
    let before = attach_log::live_snapshot().unwrap();
    let empty = app.open_window(event_loop, false).unwrap();
    let host = app.windows.get_mut(&empty).unwrap();
    assert_eq!(host.space_rail.current.as_deref(), Some("a"));
    assert!(host.attach_pane_sessions.is_empty());
    assert!(host.mux.focused().child_pid().is_none());
    assert!(text(host).contains("Empty space"));
    assert!(host.splash.is_none());
    capture(host, "a-empty-new-window");
    let after = attach_log::live_snapshot().unwrap();
    assert_eq!(before.sessions, after.sessions);
    app.windows.remove(&empty);
    app.windows = windows;
    std::env::remove_var("PMUX_SPACE");
    std::env::remove_var("PMUX_VIEW_PATH");
}

fn verify_space_rename(app: &mut App, event_loop: &ActiveEventLoop, source: WindowId) {
    let path = app.windows[&source].attach_layout_path.clone().unwrap();
    let before = attach_log::live_snapshot().unwrap();
    let windows = std::mem::take(&mut app.windows);
    let session = space_session("a");
    let targets = std::mem::replace(
        &mut app.cli.attach_sessions,
        vec![AttachTarget {
            session: session.id.to_string(),
            title: "alpha-worker".into(),
        }],
    );
    std::env::set_var("PMUX_SPACE", "a");
    std::env::set_var("PMUX_VIEW_PATH", path);
    let follower_id = app.open_window(event_loop, false).unwrap();
    app.cli.attach_sessions = targets;
    let mut follower = app.windows.remove(&follower_id).unwrap();
    follower.attach_layout_path = Some(
        host_mux_socket()
            .unwrap()
            .with_file_name("rename-view.json"),
    );
    persist_attach_layout_from_live(&mut follower);
    test_support::wait_for_attach_write(&follower);
    app.windows = windows;
    std::env::remove_var("PMUX_SPACE");
    std::env::remove_var("PMUX_VIEW_PATH");
    let owner = follower.mux.space_id.clone();
    let panes = follower.attach_pane_sessions.clone();
    assert!(owner.is_some() && !panes.is_empty());
    apply_rail_verdict(
        app.windows.get_mut(&source).unwrap(),
        space_rail::RailVerdict::Rename {
            old: "a".into(),
            new: "renamed-a".into(),
        },
    );
    assert!(!space_json_exists("a"), "host rename did not reach the CLI");
    // Reusing a name must not redirect a window that still displays that name.
    let mut replacement = load_space(&spaces_dir(), "renamed-a").unwrap();
    replacement.id = Some(prismattyc_mux::new_space_id().unwrap());
    replacement.sessions.clear();
    replacement.tabs.clear();
    prismattyc_mux::save_space(&spaces_dir(), "a", &replacement).unwrap();
    follower.last_space_refresh = None;
    refresh_space_views_settled(&mut follower);
    test_support::wait_for_attach_write(&follower);
    assert_eq!(follower.space_rail.current.as_deref(), Some("renamed-a"));
    assert_eq!(follower.mux.space_id, owner);
    assert_eq!(follower.attach_pane_sessions, panes);
    let view = attach_tabs::load(follower.attach_layout_path.as_ref().unwrap()).unwrap();
    assert_eq!(view.space.as_deref(), Some("renamed-a"));
    let started = Instant::now();
    while !text(&follower).contains("ONLY_A_366") {
        let _ = follower.mux.drain_all();
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(10));
    }
    follower.last_space_refresh = None;
    refresh_space_views_settled(&mut follower);
    assert_eq!(
        follower.space_rail.live_pane_names["renamed-a"],
        vec!["a-1"]
    );
    capture(&mut follower, "a-renamed-follower");
    command(&["space", "rm", "a"]);
    command(&["space", "rename", "renamed-a", "a"]);
    for host in [app.windows.get_mut(&source).unwrap(), &mut follower] {
        host.last_space_refresh = None;
        refresh_space_views_settled(host);
        assert_eq!(host.space_rail.current.as_deref(), Some("a"));
        assert_eq!(host.mux.space_id, owner);
    }
    let after = attach_log::live_snapshot().unwrap();
    assert_eq!(before.sessions.len(), after.sessions.len());
    for session in before.sessions {
        let live = after.sessions.iter().find(|s| s.id == session.id).unwrap();
        assert_eq!(live.space_id, session.space_id);
        let panes = |s: &prismattyc_mux::SessionSnapshot| {
            s.windows
                .iter()
                .flat_map(|window| &window.panes)
                .map(|pane| (pane.id, pane.child_pid))
                .collect::<Vec<_>>()
        };
        assert_eq!(panes(live), panes(&session));
    }
}

impl ApplicationHandler<UserAction> for Proof {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let a = self.app.open_window(event_loop, false).unwrap();
        self.windows.push(a);
        let host = self.app.windows.get_mut(&a).unwrap();
        // Exercise the same verdict as naming the + prompt.
        apply_rail_verdict(host, space_rail::RailVerdict::Create("a".into()));
        session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.app.pump(event_loop, None);
        for host in self.app.windows.values_mut() {
            let _ = host.mux.drain_all();
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(24),
            "space proof timed out at phase {}",
            self.phase
        );
        if self
            .app
            .windows
            .values()
            .any(|host| host.space_opens.busy())
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(30),
            ));
            return;
        }
        match self.phase {
            0 => {
                let a = self.app.windows.get(&self.windows[0]).unwrap();
                assert_eq!(
                    a.space_rail.current.as_deref(),
                    Some("a"),
                    "{:?}",
                    a.last_space_open
                );
                let b = self.app.open_window(event_loop, false).unwrap();
                self.windows.push(b);
                let path_a = self.app.windows[&self.windows[0]]
                    .attach_layout_path
                    .clone();
                let b = self.app.windows.get_mut(&b).unwrap();
                assert_ne!(b.attach_layout_path, path_a);
                apply_rail_verdict(b, space_rail::RailVerdict::Create("b".into()));
                session_prompt::dispatch_key(b, &Key::Named(NamedKey::Enter), false);
                self.phase = 1;
            }
            1 => {
                assert_eq!(
                    self.app.windows[&self.windows[0]]
                        .space_rail
                        .current
                        .as_deref(),
                    Some("a")
                );
                assert_eq!(
                    self.app.windows[&self.windows[1]]
                        .space_rail
                        .current
                        .as_deref(),
                    Some("b")
                );
                let a = space_session("a");
                let b = space_session("b");
                assert_ne!(a.id, b.id);
                assert_ne!(a.space_id, b.space_id);
                for session in [&a, &b] {
                    let pane = &session.windows[0].panes[0];
                    self.original
                        .push((session.id, pane.id, pane.child_pid.unwrap()));
                }
                assert_ne!(self.original[0].1, self.original[1].1);
                assert_ne!(self.original[0].2, self.original[1].2);
                for (index, marker) in ["ONLY_A_366", "ONLY_B_366"].into_iter().enumerate() {
                    self.app.windows[&self.windows[index]]
                        .mux
                        .focused()
                        .send_bytes(format!("printf '{marker}\\n'\r").into_bytes())
                        .unwrap();
                }
                command(&[
                    "rename-pane",
                    &self.original[0].1.to_string(),
                    "alpha-worker",
                ]);
                command(&[
                    "rename-pane",
                    &self.original[1].1.to_string(),
                    "beta-worker-with-a-long-name",
                ]);
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                open_space_from_host(a, "b", SpaceOpenMode::Switch);
                open_space_from_host(a, "a", SpaceOpenMode::Switch);
                self.changed = Instant::now();
                self.phase = 2;
            }
            2 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                let a = &self.app.windows[&self.windows[0]];
                let b = &self.app.windows[&self.windows[1]];
                assert!(text(a).contains("ONLY_A_366"), "{}", text(a));
                assert!(!text(a).contains("ONLY_B_366"));
                assert!(text(b).contains("ONLY_B_366"), "{}", text(b));
                assert!(!text(b).contains("ONLY_A_366"));
                for id in &self.windows {
                    let host = self.app.windows.get_mut(id).unwrap();
                    refresh_rail(host);
                    host.last_space_refresh = None;
                    refresh_space_views_settled(host);
                    assert_eq!(host.space_rail.live_pane_names["a"], vec!["a-1"]);
                    assert_eq!(host.space_rail.live_pane_names["b"], vec!["b-1"]);
                    capture(
                        host,
                        if *id == self.windows[0] {
                            "a-isolated"
                        } else {
                            "b-isolated"
                        },
                    );
                }
                verify_space_rename(&mut self.app, event_loop, self.windows[0]);
                self.app
                    .windows
                    .get_mut(&self.windows[0])
                    .unwrap()
                    .space_rail
                    .current = None;
                self.app.pump(event_loop, None);
                test_support::wait_for_attach_write(
                    self.app.windows.get(&self.windows[0]).unwrap(),
                );
                assert_eq!(
                    self.app.windows[&self.windows[0]]
                        .space_rail
                        .current
                        .as_deref(),
                    Some("a"),
                    "idle pump must infer the Space"
                );
                // A poisoned view must not attach a foreign session under A's label.
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                let original = a.attach_layout.clone().unwrap();
                let path = a.attach_layout_path.clone().unwrap();
                let mut foreign = original.clone();
                foreign.tabs[0].sessions = vec![self.original[1].0.to_string()];
                foreign.tabs[0].title.push_str(" poisoned layout");
                prismattyc_mux::attach_tabs::save(&path, &foreign).unwrap();
                set_cache_modified(&path, SystemTime::now() + Duration::from_secs(2));
                poll_host_attach_tabs(a);
                assert_eq!(
                    a.mux.remote_pane_id(a.mux.focused_id()),
                    Some(self.original[0].1),
                    "foreign view reached the source window"
                );
                assert!(a.space_opens.blocks_persist());
                prismattyc_mux::attach_tabs::save(&path, &original).unwrap();
                set_cache_modified(&path, SystemTime::now() + Duration::from_secs(4));
                let ack = prismattyc_mux::host_ack_path_from_socket(&host_mux_socket().unwrap());
                std::fs::write(&ack, b"stale\n").unwrap();
                set_cache_modified(&ack, SystemTime::now() - Duration::from_secs(10));
                let since = SystemTime::now() - Duration::from_secs(2);
                poll_host_attach_tabs(a);
                assert!(
                    prismattyc_mux::wait_host_ack(&ack, since, Duration::from_millis(100)),
                    "matching external cache layout must refresh the host ACK"
                );
                assert!(!a.space_opens.blocks_persist());
                // Multi-pane source: only the visible original pane moves.
                split(&space_session("a"));
                let a = space_session("a");
                assert_eq!(a.windows[0].panes.len(), 2);
                self.split_pane = a.windows[0]
                    .panes
                    .iter()
                    .find(|p| p.id != self.original[0].1)
                    .unwrap()
                    .id;
                move_to_space_from_host(
                    self.app.windows.get_mut(&self.windows[0]).unwrap(),
                    "b",
                    false,
                );
                self.changed = Instant::now();
                self.phase = 3;
            }
            3 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                let a = space_session("a");
                assert_eq!(a.windows[0].panes.len(), 1);
                assert_eq!(a.windows[0].panes[0].id, self.split_pane);
                let snapshot = attach_log::live_snapshot().unwrap();
                let owner_b = load_space(&spaces_dir(), "b").unwrap().id;
                let moved = snapshot
                    .sessions
                    .iter()
                    .find(|s| {
                        s.windows
                            .iter()
                            .flat_map(|w| &w.panes)
                            .any(|p| p.id == self.original[0].1)
                    })
                    .unwrap();
                assert_eq!(moved.space_id, owner_b);
                let pane = moved
                    .windows
                    .iter()
                    .flat_map(|w| &w.panes)
                    .find(|p| p.id == self.original[0].1)
                    .unwrap();
                assert_eq!(pane.child_pid, Some(self.original[0].2));
                assert_eq!(
                    self.app.windows[&self.windows[0]]
                        .mux
                        .remote_pane_id(self.app.windows[&self.windows[0]].mux.focused_id()),
                    Some(self.split_pane)
                );
                move_to_space_from_host(
                    self.app.windows.get_mut(&self.windows[0]).unwrap(),
                    "b",
                    true,
                );
                self.changed = Instant::now();
                self.phase = 4;
            }
            4 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                let owner_a = load_space(&spaces_dir(), "a").unwrap().id;
                let snapshot = attach_log::live_snapshot().unwrap();
                assert!(!snapshot.sessions.iter().any(|s| s.space_id == owner_a));
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                assert!(a.attach_pane_sessions.is_empty());
                assert!(a.mux.focused().child_pid().is_none());
                assert!(text(a).contains("Empty space"));
                capture(a, "a-empty");
                let b = self.app.windows.get_mut(&self.windows[1]).unwrap();
                assert_eq!(b.attach_pane_sessions.len(), 3);
                capture(b, "b-after-moves");
                let output =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/spaces-host-proof");
                std::fs::write(
                    output.join("snapshot.json"),
                    serde_json::to_vec_pretty(&snapshot).unwrap(),
                )
                .unwrap();
                let path = self.app.windows[&self.windows[0]]
                    .attach_layout_path
                    .clone()
                    .unwrap();
                verify_empty_space_startup(&mut self.app, event_loop, &path);
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                for _ in 0..2 {
                    handle_mux_command(
                        a,
                        MuxCommand::Split(prismattyc_mux::Axis::Horizontal),
                        keybind::Action::SplitRight,
                        "/bin/false",
                        &[],
                    );
                    session_prompt::dispatch_key(a, &Key::Named(NamedKey::Enter), false);
                    assert!(a.session_prompt.is_none(), "split naming failed");
                }
                handle_mux_command(
                    a,
                    MuxCommand::NewTab,
                    keybind::Action::NewTab,
                    "/bin/false",
                    &[],
                );
                session_prompt::dispatch_key(a, &Key::Named(NamedKey::Enter), false);
                assert!(a.session_prompt.is_none(), "tab naming failed");
                persist_attach_layout_from_live(a);
                test_support::wait_for_attach_write(a);
                self.changed = Instant::now();
                self.phase = 5;
            }
            5 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                assert_eq!(a.attach_pane_sessions.len(), 3);
                let owner = load_space(&spaces_dir(), "a").unwrap().id;
                let snapshot = attach_log::live_snapshot().unwrap();
                assert_eq!(
                    snapshot
                        .sessions
                        .iter()
                        .filter(|s| s.space_id == owner)
                        .count(),
                    3
                );
                assert!(a
                    .attach_pane_sessions
                    .keys()
                    .all(|pane| a.mux.remote_pane_id(*pane).is_some()));
                capture(a, "a-new-sessions");
                a.mux.select_tab(0).unwrap();
                persist_attach_selection(a);
                test_support::wait_for_attach_write(a);
                let selected = attach_tabs::load(a.attach_layout_path.as_ref().unwrap()).unwrap();
                assert_eq!(
                    selected.focused_session,
                    a.attach_pane_sessions.get(&a.mux.focused_id()).cloned()
                );
                let mut outdated = load_space(&spaces_dir(), "a").unwrap();
                outdated.sessions.clear();
                outdated.tabs.clear();
                prismattyc_mux::save_space(&spaces_dir(), "a", &outdated).unwrap();
                save_space_from_host(a, "a");
                let saved = load_space(&spaces_dir(), "a").unwrap();
                assert_eq!(saved.sessions.len(), 3);
                verify_polish_ui(a);
                a.mux.select_tab(a.mux.window_ids().len() - 1).unwrap();
                a.spacing.space_rail_pane_names = false;
                App::refit_geom(a, a.window.inner_size(), Some("hide pane names"));
                capture(a, "a-pane-names-off");
                apply_mux_tab_command(a, MuxTabCommand::CloseTab, "/bin/false", &[]).unwrap();
                persist_attach_layout_from_live(a);
                let _ = a.window.request_inner_size(PhysicalSize::new(420, 320));
                self.phase = 6;
                self.changed = Instant::now();
            }
            6 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                let a = self.app.windows.get_mut(&self.windows[0]).unwrap();
                assert_eq!(
                    a.attach_pane_sessions.len(),
                    2,
                    "a deliberately closed tab must stay detached"
                );
                a.spacing.space_rail_pane_names = true;
                App::refit_geom(a, a.window.inner_size(), Some("small window"));
                capture(a, "a-small-window");
                let session = a.attach_pane_sessions[&a.mux.focused_id()].clone();
                command(&[
                    "session",
                    "name",
                    "short-lived-worker",
                    "--session",
                    &session,
                ]);
                a.last_space_refresh = None;
                refresh_space_views_settled(a);
                assert!(
                    a.space_rail.live_pane_names["a"].contains(&"short-lived-worker".to_string())
                );
                a.mux.focused().send_bytes(b"exit\r".to_vec()).unwrap();
                self.phase = 7;
                self.changed = Instant::now();
            }
            7 if self.changed.elapsed() >= Duration::from_millis(1300) => {
                for host in self.app.windows.values_mut() {
                    host.last_space_refresh = None;
                    refresh_space_views_settled(host);
                    assert!(!host.space_rail.live_pane_names["a"]
                        .contains(&"short-lived-worker".to_string()));
                }
                capture(
                    self.app.windows.get_mut(&self.windows[0]).unwrap(),
                    "a-after-pane-exit",
                );
                let b_path = self.app.windows[&self.windows[1]]
                    .attach_layout_path
                    .clone();
                self.app.windows.remove(&self.windows[0]);
                self.app.last_register_try = None;
                self.app.retry_register_host_pid();
                let b = self.app.windows.get_mut(&self.windows[1]).unwrap();
                assert_ne!(b.attach_layout_path, b_path);
                assert_eq!(b.space_rail.current.as_deref(), Some("b"));
                test_support::wait_for_attach_write(b);
                let layout = attach_tabs::load(b.attach_layout_path.as_ref().unwrap()).unwrap();
                assert_eq!(layout.space.as_deref(), Some("b"));
                open_space_from_host(b, "a", SpaceOpenMode::Switch);
                self.phase = 8;
            }
            8 => {
                let host = self.app.windows.get_mut(&self.windows[1]).unwrap();
                assert_eq!(
                    host.space_rail.current.as_deref(),
                    Some("a"),
                    "host dispatch must switch the view"
                );
                let pid_path = &self.app.registered_host.as_ref().unwrap().0;
                let _ = std::fs::remove_file(pid_path.with_extension("render.json"));
                self.app.last_render_status = None;
                self.app.publish_render_status();
                test_support::wait_for_render_status(&self.app);
                let status =
                    prismattyc_mux::host_render_status::read(&host_mux_socket().unwrap()).unwrap();
                assert!(!status["windows"].as_array().unwrap().is_empty());
                self.complete = true;
                event_loop.exit();
                return;
            }
            _ => {}
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(30),
        ));
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn set_cache_modified(path: &Path, modified: SystemTime) {
    std::fs::File::open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

fn verify_polish_ui(host: &mut HostState) {
    let remote = host.mux.remote_pane_id(host.mux.focused_id()).unwrap();
    command(&[
        "pane-write",
        &remote.to_string(),
        "--text",
        "printf POLISH_RECEIPT",
        "--submit",
        "enter",
        "--json",
    ]);
    terminal_switcher::messages(host);
    let labels = host.terminal_targets.as_ref().unwrap();
    assert!(labels.iter().any(|e| e.label.contains("execution unknown")));
    let index = labels
        .iter()
        .position(|e| e.label.contains(&format!("pane {remote} —")))
        .unwrap();
    let overlay = chrome_overlay(host);
    assert!(
        matches!(overlay,a11y::OverlayKind::Choices { ref rows,.. } if rows.iter().any(|r|r.contains("execution unknown")))
    );
    capture(host, "agent-messages");
    dispatch_overlay_activate(host, index, "/bin/false", &[]);
    assert_eq!(host.mux.remote_pane_id(host.mux.focused_id()), Some(remote));
    space_panel::maintenance(host);
    let (title, rows) = space_panel::rows(host).unwrap();
    assert_eq!(title, "UPDATE AND RESTART");
    assert!(rows.iter().any(|r| r.name == "Restart MCP adapters"));
    let overlay = chrome_overlay(host);
    assert!(
        matches!(overlay,a11y::OverlayKind::Palette { ref rows,.. } if rows.iter().any(|r|r.contains("Restart MCP adapters")))
    );
    capture(host, "update-restart");
    host.space_panel = None;
    host.context_menu = None;
}

/// A layout written before the snapshot cache knows its session stays pending.
/// The next snapshot, taken after that write, applies and acks the same file.
#[test]
fn stale_snapshot_keeps_a_newer_layout_pending() {
    if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
        render_window_tests::run_in_private_display(
            "space_open_window_tests::stale_snapshot_keeps_a_newer_layout_pending",
        );
        return;
    }
    let binaries = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let pmux = binaries.join("pmux");
    let pmuxd = binaries.join("pmuxd");
    assert!(pmux.is_file() && pmuxd.is_file());
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().unwrap();
    let _daemon = Daemon(
        Command::new(pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(start.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(20));
    }
    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(["--no-splash", "/bin/cat"].into_iter().map(String::from)).unwrap();
    let app = App::new(
        cli,
        config::ConfigFile::default(),
        None,
        event_loop.create_proxy(),
    )
    .unwrap();
    let mut proof = StaleCacheProof {
        app,
        window: None,
        started: Instant::now(),
        done: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done);
    std::fs::write(
        std::env::var_os(render_window_tests::RESULT_ENV).unwrap(),
        b"complete",
    )
    .unwrap();
}

struct StaleCacheProof {
    app: App,
    window: Option<WindowId>,
    started: Instant,
    done: bool,
}

impl ApplicationHandler<UserAction> for StaleCacheProof {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let id = self.app.open_window(event_loop, false).unwrap();
        self.window = Some(id);
        let host = self.app.windows.get_mut(&id).unwrap();
        apply_rail_verdict(host, space_rail::RailVerdict::Create("a".into()));
        session_prompt::dispatch_key(host, &Key::Named(NamedKey::Enter), false);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.app.pump(event_loop, None);
        for host in self.app.windows.values_mut() {
            let _ = host.mux.drain_all();
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(20),
            "stale-cache proof timed out"
        );
        let id = self.window.expect("window");
        if self.app.windows[&id].space_opens.busy() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(30),
            ));
            return;
        }
        let host = self.app.windows.get_mut(&id).unwrap();
        assert_eq!(host.space_rail.current.as_deref(), Some("a"));
        assert!(host.cache_writer);
        let _ = space_session("a");
        let path = host.attach_layout_path.clone().unwrap();
        let ack = prismattyc_mux::host_ack_path_from_socket(&host_mux_socket().unwrap());
        let ack_mtime = std::fs::metadata(&ack)
            .and_then(|meta| meta.modified())
            .ok();
        host.snapshot_client = Some(Arc::new(snapshot_client::SnapshotClient::detached()));
        let client = Arc::clone(host.snapshot_client.as_ref().unwrap());
        client.publish_for_test(
            prismattyc_mux::Snapshot {
                sequence: 0,
                sessions: Vec::new(),
            },
            SystemTime::UNIX_EPOCH,
        );
        let mut layout = attach_tabs::load(&path).unwrap();
        assert!(!layout.tabs.is_empty());
        layout.tabs[0].title = "stale-cache-pending-title".into();
        std::thread::sleep(Duration::from_millis(20));
        prismattyc_mux::attach_tabs::save(&path, &layout).unwrap();
        let stamp_before = host.attach_cache_stamp;
        let pane_before = host.mux.remote_pane_id(host.mux.focused_id());
        poll_host_attach_tabs(host);
        assert_eq!(
            host.attach_cache_stamp, stamp_before,
            "an older cache must not consume the layout stamp"
        );
        assert!(
            !host.space_opens.blocks_persist(),
            "pending ownership must not fence later polls"
        );
        assert_eq!(
            std::fs::metadata(&ack)
                .and_then(|meta| meta.modified())
                .ok(),
            ack_mtime,
            "a pending layout is not acknowledged"
        );
        assert_eq!(host.mux.remote_pane_id(host.mux.focused_id()), pane_before);
        let file_mtime = cache_stamp(&path).unwrap().0;
        client.publish_for_test(
            attach_log::live_snapshot().unwrap(),
            file_mtime + Duration::from_secs(2),
        );
        poll_host_attach_tabs(host);
        assert_eq!(host.attach_cache_stamp, cache_stamp(&path));
        assert!(!host.space_opens.blocks_persist());
        assert_eq!(
            host.attach_layout
                .as_ref()
                .and_then(|file| file.tabs.first())
                .map(|tab| tab.title.as_str()),
            Some("stale-cache-pending-title"),
            "the same file is applied once a later snapshot allows it"
        );
        assert_ne!(
            std::fs::metadata(&ack)
                .and_then(|meta| meta.modified())
                .ok(),
            ack_mtime,
            "the same file is acknowledged once a later snapshot allows it"
        );
        self.done = true;
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

/// Startup attach copies live sessions into the window without going through
/// `bind_attach_pane`. A process-wide cache from before that copy must not
/// detach them. A snapshot taken after the copy may.
#[test]
fn stale_cache_does_not_detach_a_newer_attachment() {
    if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
        render_window_tests::run_in_private_display(
            "space_open_window_tests::stale_cache_does_not_detach_a_newer_attachment",
        );
        return;
    }
    let binaries = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let pmux = binaries.join("pmux");
    let pmuxd = binaries.join("pmuxd");
    assert!(pmux.is_file() && pmuxd.is_file());
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().unwrap();
    let _daemon = Daemon(
        Command::new(pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(start.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(20));
    }
    command(&["space", "create", "a"]);
    let session = loop {
        let found = load_space(&spaces_dir(), "a").ok().and_then(|space| {
            attach_log::live_snapshot().and_then(|snapshot| {
                snapshot
                    .sessions
                    .into_iter()
                    .find(|live| live.space_id == space.id)
            })
        });
        if let Some(found) = found {
            break found;
        }
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "space create did not publish a session"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    std::env::set_var("PMUX_SPACE", "a");
    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(
        [
            "--no-splash",
            "--attach-session",
            &session.id.to_string(),
            "--attach-title",
            session.name.as_str(),
        ]
        .into_iter()
        .map(String::from),
    )
    .unwrap();
    let mut app = App::new(
        cli,
        config::ConfigFile::default(),
        None,
        event_loop.create_proxy(),
    )
    .unwrap();
    app.snapshot_client = Some(Arc::new(snapshot_client::SnapshotClient::detached()));
    app.snapshot_client.as_ref().unwrap().publish_for_test(
        prismattyc_mux::Snapshot {
            sequence: 0,
            sessions: Vec::new(),
        },
        SystemTime::UNIX_EPOCH,
    );
    let mut proof = StaleDetachProof {
        app,
        window: None,
        started: Instant::now(),
        done: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done);
    std::fs::write(
        std::env::var_os(render_window_tests::RESULT_ENV).unwrap(),
        b"complete",
    )
    .unwrap();
}

struct StaleDetachProof {
    app: App,
    window: Option<WindowId>,
    started: Instant,
    done: bool,
}

impl ApplicationHandler<UserAction> for StaleDetachProof {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let id = self.app.open_window(event_loop, false).unwrap();
        self.window = Some(id);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.done {
            return;
        }
        self.app.pump(event_loop, None);
        for host in self.app.windows.values_mut() {
            let _ = host.mux.drain_all();
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(20),
            "stale-detach proof timed out"
        );
        let id = self.window.expect("window");
        let host = self.app.windows.get_mut(&id).unwrap();
        assert_eq!(host.space_rail.current.as_deref(), Some("a"));
        let sessions = host.attach_pane_sessions.clone();
        assert!(
            !sessions.is_empty(),
            "startup must attach the new Space session"
        );
        assert!(
            sessions
                .keys()
                .all(|pane| host.attach_bound_at.contains_key(pane)),
            "window initialization must stamp every attachment"
        );
        let pane_before = host.mux.remote_pane_id(host.mux.focused_id());
        host.snapshot_client = Some(Arc::new(snapshot_client::SnapshotClient::detached()));
        let client = Arc::clone(host.snapshot_client.as_ref().unwrap());
        client.publish_for_test(
            prismattyc_mux::Snapshot {
                sequence: 0,
                sessions: Vec::new(),
            },
            SystemTime::UNIX_EPOCH,
        );
        host.last_space_refresh = None;
        refresh_space_views_settled(host);
        assert_eq!(
            host.attach_pane_sessions, sessions,
            "a cache from before the attachment must not detach it"
        );
        assert_eq!(host.mux.remote_pane_id(host.mux.focused_id()), pane_before);
        client.publish_for_test(
            prismattyc_mux::Snapshot {
                sequence: 1,
                sessions: Vec::new(),
            },
            SystemTime::now() + Duration::from_secs(5),
        );
        host.last_space_refresh = None;
        refresh_space_views_settled(host);
        assert!(
            sessions
                .keys()
                .all(|pane| !host.attach_pane_sessions.contains_key(pane)),
            "a snapshot taken after the attachment may detach a session it lacks"
        );
        self.done = true;
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

/// Space panes must still be attached after `pmux space open` recreates them
/// on a fresh daemon. Burner sessions force new ids. Reconciliation used to
/// treat those ids as tabs the user closed and write an empty cache.
#[test]
fn space_reopen_after_daemon_restart_keeps_sessions() {
    launch_reopen_child(
        "space_open_window_tests::space_reopen_after_daemon_restart_keeps_sessions",
        true,
    );
}

/// The same reopen when the fresh daemon reissues the old session ids.
/// A dead placeholder whose id matches the new session used to stay
/// disconnected, because regroup already saw the name as present.
#[test]
fn space_reopen_after_daemon_restart_reused_ids_stay_connected() {
    launch_reopen_child(
        "space_open_window_tests::space_reopen_after_daemon_restart_reused_ids_stay_connected",
        false,
    );
}

fn launch_reopen_child(test_name: &str, recycle_ids: bool) {
    if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
        render_window_tests::run_in_private_display(test_name);
        return;
    }
    let binaries = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let pmux = binaries.join("pmux");
    let pmuxd = binaries.join("pmuxd");
    assert!(
        pmux.is_file() && pmuxd.is_file(),
        "build pmux and pmuxd first"
    );
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().unwrap();
    let daemon = Daemon(
        Command::new(&pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "private daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(
        ["--no-splash", "--panes", "2", "/bin/cat"]
            .into_iter()
            .map(String::from),
    )
    .unwrap();
    let config = config::ConfigFile {
        space_startup: Some("fresh".into()),
        space_autosave: Some(false),
        ..config::ConfigFile::default()
    };
    let app = App::new(cli, config, None, event_loop.create_proxy()).unwrap();
    let mut proof = RestartProof {
        app,
        daemon,
        pmuxd,
        socket,
        recycle_ids,
        done: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done, "restart fixture did not run");
}

struct RestartProof {
    app: App,
    daemon: Daemon,
    pmuxd: PathBuf,
    socket: PathBuf,
    recycle_ids: bool,
    done: bool,
}

impl ApplicationHandler<UserAction> for RestartProof {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.done {
            return;
        }
        let window = self.app.open_window(event_loop, false).unwrap();
        reopen_space_after_restart(
            &mut self.app,
            window,
            &mut self.daemon,
            &self.pmuxd,
            &self.socket,
            self.recycle_ids,
        );
        std::fs::write(
            std::env::var_os("PRISMATTYC_RENDER_TEST_RESULT").unwrap(),
            "complete",
        )
        .unwrap();
        self.done = true;
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn pump_host(app: &mut App, window: WindowId) {
    let host = app.windows.get_mut(&window).unwrap();
    let _ = host.mux.drain_all();
    poll_host_attach_tabs(host);
    host.last_space_refresh = None;
    refresh_space_views_settled(host);
}

/// Ack `space open` without refreshing. Refresh rewrites a dead binding's
/// id to its name while the new daemon has not recreated the session, and
/// that rewrite hides the reused-id case behind the different-id path.
fn pump_ack(app: &mut App, window: WindowId) {
    let host = app.windows.get_mut(&window).unwrap();
    let _ = host.mux.drain_all();
    poll_host_attach_tabs(host);
}

fn pump_command_with(app: &mut App, window: WindowId, args: &[&str], pump: fn(&mut App, WindowId)) {
    let pmux = pmux_bin();
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let handle = std::thread::spawn(move || {
        Command::new(pmux)
            .args(&owned)
            .output()
            .expect("spawn pmux")
    });
    let started = Instant::now();
    loop {
        if handle.is_finished() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "pmux {args:?} did not finish"
        );
        pump(app, window);
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = handle.join().expect("pmux thread");
    assert!(
        result.status.success(),
        "pmux {args:?}\n{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn pump_command(app: &mut App, window: WindowId, args: &[&str]) {
    pump_command_with(app, window, args, pump_host);
}

fn pump_command_ack(app: &mut App, window: WindowId, args: &[&str]) {
    pump_command_with(app, window, args, pump_ack);
}

/// The fresh daemon has already reissued the old ids, and every pane is
/// still the pre-restart placeholder bound to that same id.
fn assert_reused_ids_still_dead(app: &App, window: WindowId) {
    let host = app.windows.get(&window).unwrap();
    let space = load_space(&spaces_dir(), "reopen-space").unwrap();
    let snapshot = attach_log::live_snapshot().expect("daemon snapshot");
    let mut checked = 0;
    for (pane, bound) in &host.attach_pane_sessions {
        let Some(name) = host.mux.attach_name_of(*pane) else {
            continue;
        };
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == name && session.space_id == space.id)
        else {
            continue;
        };
        checked += 1;
        assert_eq!(
            bound,
            &live.id.to_string(),
            "{name} id changed before refresh; this no longer covers reused ids"
        );
        assert!(
            host.mux.is_placeholder(*pane),
            "{name} reconnected before refresh"
        );
    }
    assert!(checked >= 2, "reused-id setup lost the placeholder panes");
}

fn replace_daemon(daemon: &mut Daemon, pmuxd: &Path, socket: &Path) {
    let _ = daemon.0.kill();
    let _ = daemon.0.wait();
    let _ = std::fs::remove_file(socket);
    daemon.0 = Command::new(pmuxd)
        .arg("--socket")
        .arg(socket)
        .args(["--", "/bin/sh"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "replacement daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reopen_space_after_restart(
    app: &mut App,
    window: WindowId,
    daemon: &mut Daemon,
    pmuxd: &Path,
    socket: &Path,
    recycle_ids: bool,
) {
    if recycle_ids {
        for name in ["burner-1", "burner-2", "burner-3"] {
            pump_command(app, window, &["new", "--no-attach", name]);
        }
    }
    pump_command(
        app,
        window,
        &["space", "create", "reopen-space", "--no-attach"],
    );
    pump_command(
        app,
        window,
        &["space", "add", "reopen-space", "--name", "reopen-extra"],
    );
    pump_host(app, window);
    {
        let host = app.windows.get_mut(&window).unwrap();
        assert_eq!(host.space_rail.current.as_deref(), Some("reopen-space"));
        assert!(
            !host.attach_pane_sessions.is_empty(),
            "host never attached the space"
        );
    }
    let _ = daemon.0.kill();
    let _ = daemon.0.wait();
    // Each log reader blocks in subscribe, then retries the dead socket.
    // Every attached pane has to be a placeholder before the new daemon
    // starts; one survivor reconnects and hides the reopen race.
    let started = Instant::now();
    loop {
        let host = app.windows.get_mut(&window).unwrap();
        let _ = host.mux.drain_all();
        let pending = host.attach_pane_sessions.keys().any(|pane| {
            !host.mux.is_placeholder(*pane)
                && host
                    .mux
                    .pane(*pane)
                    .is_some_and(|runtime| runtime.child_alive)
        });
        if !host.attach_pane_sessions.is_empty() && !pending {
            let _ = host.mux.drain_all();
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "attach panes never all became placeholders after pmuxd died"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stale_ids: Vec<String> = app.windows[&window]
        .attach_pane_sessions
        .values()
        .cloned()
        .collect();
    assert!(
        app.windows[&window]
            .attach_pane_sessions
            .keys()
            .all(|pane| app.windows[&window].mux.is_placeholder(*pane)),
        "pre-restart panes must be placeholders"
    );
    replace_daemon(daemon, pmuxd, socket);
    if recycle_ids {
        pump_command(app, window, &["space", "open", "reopen-space"]);
    } else {
        pump_command_ack(app, window, &["space", "open", "reopen-space"]);
        assert_reused_ids_still_dead(app, window);
    }
    pump_host(app, window);

    let space = load_space(&spaces_dir(), "reopen-space").unwrap();
    let snapshot = attach_log::live_snapshot().expect("daemon snapshot");
    let live: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id == space.id)
        .collect();
    assert_eq!(live.len(), 2, "space open did not restore both sessions");
    if recycle_ids {
        assert!(
            live.iter()
                .any(|session| !stale_ids.contains(&session.id.to_string())),
            "restart did not recycle session ids: stale={stale_ids:?} live={live:?}"
        );
    } else {
        assert!(
            live.iter()
                .all(|session| stale_ids.contains(&session.id.to_string())),
            "restart did not reuse session ids: stale={stale_ids:?} live={live:?}"
        );
    }
    let host = app.windows.get_mut(&window).unwrap();
    let path = host.attach_layout_path.clone().unwrap();
    let cache = attach_tabs::load(&path).expect("attach cache");
    let keys: Vec<String> = cache
        .tabs
        .iter()
        .flat_map(|tab| tab.sessions.iter().cloned())
        .collect();
    for session in &live {
        let id = session.id.to_string();
        assert!(
            keys.iter().any(|key| key == &id || key == &session.name),
            "attach cache dropped {}: keys={keys:?}",
            session.name
        );
        let matches: Vec<_> = host
            .attach_pane_sessions
            .iter()
            .filter(|(pane, bound)| {
                *bound == &id || host.mux.attach_name_of(**pane) == Some(session.name.as_str())
            })
            .map(|(pane, bound)| (*pane, bound.clone()))
            .collect();
        assert!(!matches.is_empty(), "no pane for {}", session.name);
        for (pane, bound) in matches {
            assert_eq!(
                bound, id,
                "{} is bound to {bound}, not the live session",
                session.name
            );
            assert!(
                !host.mux.is_placeholder(pane),
                "{} is still a placeholder after reopen",
                session.name
            );
            assert!(
                host.mux
                    .pane(pane)
                    .is_some_and(|runtime| runtime.child_alive),
                "{} attach is not running",
                session.name
            );
        }
    }
}

/// Two retained Space views reuse pane ids. Rebinding one view must not
/// consume the other view's dead pane after a daemon restart reissues the
/// same session ids.
#[test]
fn space_reopen_after_daemon_restart_reused_ids_reconnect_each_view() {
    if std::env::var_os("PRISMATTYC_RENDER_TEST_CHILD").is_none() {
        render_window_tests::run_in_private_display(
            "space_open_window_tests::space_reopen_after_daemon_restart_reused_ids_reconnect_each_view",
        );
        return;
    }
    let binaries = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let pmux = binaries.join("pmux");
    let pmuxd = binaries.join("pmuxd");
    assert!(
        pmux.is_file() && pmuxd.is_file(),
        "build pmux and pmuxd first"
    );
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().unwrap();
    let daemon = Daemon(
        Command::new(&pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "private daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let event_loop = EventLoop::<UserAction>::with_user_event()
        .with_x11()
        .with_any_thread(true)
        .build()
        .unwrap();
    let cli = Cli::parse(
        ["--no-splash", "--panes", "1", "/bin/cat"]
            .into_iter()
            .map(String::from),
    )
    .unwrap();
    let config = config::ConfigFile {
        space_startup: Some("fresh".into()),
        space_autosave: Some(false),
        ..config::ConfigFile::default()
    };
    let app = App::new(cli, config, None, event_loop.create_proxy()).unwrap();
    let mut proof = TwoViewProof {
        app,
        daemon,
        pmuxd,
        socket,
        done: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done, "two-view restart fixture did not run");
}

struct TwoViewProof {
    app: App,
    daemon: Daemon,
    pmuxd: PathBuf,
    socket: PathBuf,
    done: bool,
}

impl ApplicationHandler<UserAction> for TwoViewProof {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.done {
            return;
        }
        let window = self.app.open_window(event_loop, false).unwrap();
        reopen_two_views_after_restart(
            &mut self.app,
            window,
            &mut self.daemon,
            &self.pmuxd,
            &self.socket,
        );
        std::fs::write(
            std::env::var_os("PRISMATTYC_RENDER_TEST_RESULT").unwrap(),
            "complete",
        )
        .unwrap();
        self.done = true;
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn attach_panes(mux: &mux::MuxRuntime) -> Vec<prismattyc_mux::PaneId> {
    mux.tab_panes()
        .into_iter()
        .flat_map(|(_, panes)| panes)
        .filter(|pane| mux.attach_session_of(*pane).is_some())
        .collect()
}

fn bound_ids(mux: &mux::MuxRuntime) -> Vec<String> {
    attach_panes(mux)
        .into_iter()
        .filter_map(|pane| mux.attach_session_of(pane).map(str::to_string))
        .collect()
}

fn attach_still_running(mux: &mux::MuxRuntime) -> bool {
    attach_panes(mux).into_iter().any(|pane| {
        !mux.is_placeholder(pane) && mux.pane(pane).is_some_and(|runtime| runtime.child_alive)
    })
}

fn retain_blank(app: &mut App, window: WindowId) {
    let host = app.windows.get_mut(&window).unwrap();
    let pane = host
        .mux
        .split_focused("/bin/cat", &[], prismattyc_mux::Axis::Horizontal, 0.5)
        .expect("retain a local terminal");
    host.mux.retain_local_terminal(pane);
    assert!(host.mux.is_retained_local_terminal(pane));
    assert!(
        host.mux.attach_session_of(pane).is_none(),
        "the retained terminal must not be an attach pane"
    );
}

fn assert_view_still_dead(app: &App, window: WindowId, space_name: &str, stale_ids: &[String]) {
    let host = app.windows.get(&window).unwrap();
    let space = load_space(&spaces_dir(), space_name).unwrap();
    assert_eq!(host.mux.space_id, space.id, "{space_name} is not showing");
    let snapshot = attach_log::live_snapshot().expect("daemon snapshot");
    let mut checked = 0;
    for (pane, bound) in &host.attach_pane_sessions {
        let Some(name) = host.mux.attach_name_of(*pane) else {
            continue;
        };
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == name && session.space_id == space.id)
        else {
            continue;
        };
        checked += 1;
        assert_eq!(
            bound,
            &live.id.to_string(),
            "{name} id changed before refresh; this no longer covers reused ids"
        );
        assert!(
            stale_ids.contains(bound),
            "{space_name} {name} was not reused: stale={stale_ids:?} live={}",
            live.id
        );
        assert!(
            host.mux.is_placeholder(*pane),
            "{name} reconnected before refresh"
        );
    }
    assert!(
        checked >= 1,
        "{space_name} lost its placeholder panes before refresh"
    );
}

fn assert_space_reattached(app: &App, window: WindowId, space_name: &str, stale_ids: &[String]) {
    let space = load_space(&spaces_dir(), space_name).unwrap();
    let snapshot = attach_log::live_snapshot().expect("daemon snapshot");
    let live: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id == space.id)
        .collect();
    assert!(!live.is_empty(), "{space_name} has no live sessions");
    assert!(
        live.iter()
            .all(|session| stale_ids.contains(&session.id.to_string())),
        "{space_name} did not reuse session ids: stale={stale_ids:?} live={live:?}"
    );
    let host = app.windows.get(&window).unwrap();
    assert_eq!(
        host.mux.space_id, space.id,
        "{space_name} is not the restored view"
    );
    for session in &live {
        let id = session.id.to_string();
        let matches: Vec<_> = host
            .attach_pane_sessions
            .iter()
            .filter(|(pane, bound)| {
                *bound == &id || host.mux.attach_name_of(**pane) == Some(session.name.as_str())
            })
            .map(|(pane, bound)| (*pane, bound.clone()))
            .collect();
        assert!(!matches.is_empty(), "no pane for {}", session.name);
        for (pane, bound) in matches {
            assert_eq!(
                bound, id,
                "{} is bound to {bound}, not the live session",
                session.name
            );
            assert!(
                !host.mux.is_placeholder(pane),
                "{} is still a placeholder after reopen",
                session.name
            );
            assert!(
                host.mux
                    .pane(pane)
                    .is_some_and(|runtime| runtime.child_alive),
                "{} attach is not running",
                session.name
            );
        }
    }
}

fn reopen_two_views_after_restart(
    app: &mut App,
    window: WindowId,
    daemon: &mut Daemon,
    pmuxd: &Path,
    socket: &Path,
) {
    pump_command(app, window, &["space", "create", "reopen-a", "--no-attach"]);
    pump_host(app, window);
    {
        let host = app.windows.get(&window).unwrap();
        assert_eq!(host.space_rail.current.as_deref(), Some("reopen-a"));
        assert!(
            !attach_panes(&host.mux).is_empty(),
            "reopen-a never attached"
        );
    }
    retain_blank(app, window);
    pump_command(app, window, &["space", "create", "reopen-b", "--no-attach"]);
    pump_host(app, window);
    {
        let host = app.windows.get(&window).unwrap();
        assert_eq!(host.space_rail.current.as_deref(), Some("reopen-b"));
        assert!(
            !attach_panes(&host.mux).is_empty(),
            "reopen-b never attached"
        );
    }
    retain_blank(app, window);

    let (overlap, stale_a, stale_b) = {
        let host = app.windows.get(&window).unwrap();
        let space_a = load_space(&spaces_dir(), "reopen-a").unwrap();
        let space_b = load_space(&spaces_dir(), "reopen-b").unwrap();
        assert_eq!(host.mux.space_id, space_b.id);
        assert_eq!(host.local_views.parked.len(), 1, "reopen-a was not parked");
        let parked = host.local_views.parked.values().next().unwrap();
        assert_eq!(parked.mux.space_id, space_a.id);
        assert_ne!(
            host.mux.instance(),
            parked.mux.instance(),
            "the two Spaces share one runtime, so pane ids cannot overlap"
        );
        assert!(local_views::has_local(&host.mux));
        assert!(local_views::has_local(&parked.mux));
        let active = attach_panes(&host.mux);
        let parked_panes = attach_panes(&parked.mux);
        let overlap: Vec<_> = active
            .iter()
            .copied()
            .filter(|pane| parked_panes.contains(pane))
            .collect();
        assert!(
            !overlap.is_empty(),
            "fixture panes do not overlap; a bare PaneId guard would not be exercised: active={active:?} parked={parked_panes:?}"
        );
        let stale_b = bound_ids(&host.mux);
        let stale_a = bound_ids(&parked.mux);
        assert!(!stale_a.is_empty() && !stale_b.is_empty());
        (overlap, stale_a, stale_b)
    };

    let _ = daemon.0.kill();
    let _ = daemon.0.wait();
    let started = Instant::now();
    loop {
        let host = app.windows.get_mut(&window).unwrap();
        let _ = host.mux.drain_all();
        let _ = local_views::drain(host, Instant::now() + Duration::from_secs(1));
        let active_pending = attach_still_running(&host.mux);
        let parked_pending = host
            .local_views
            .parked
            .values()
            .any(|view| attach_still_running(&view.mux));
        if host.local_views.parked.len() == 1
            && !attach_panes(&host.mux).is_empty()
            && !active_pending
            && !parked_pending
        {
            let _ = host.mux.drain_all();
            let _ = local_views::drain(host, Instant::now() + Duration::from_secs(1));
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "active and parked attach panes never all became placeholders"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    {
        let host = app.windows.get(&window).unwrap();
        assert!(
            attach_panes(&host.mux)
                .iter()
                .all(|pane| host.mux.is_placeholder(*pane)),
            "reopen-b panes must be placeholders"
        );
        let parked = &host.local_views.parked.values().next().unwrap().mux;
        assert!(
            attach_panes(parked)
                .iter()
                .all(|pane| parked.is_placeholder(*pane)),
            "reopen-a panes must be placeholders"
        );
    }

    replace_daemon(daemon, pmuxd, socket);
    // Open A first so the fresh daemon reissues ids in the original order.
    // Refresh only after each open has created that Space's sessions.
    pump_command_ack(app, window, &["space", "open", "reopen-a"]);
    assert_view_still_dead(app, window, "reopen-a", &stale_a);
    pump_host(app, window);
    assert_space_reattached(app, window, "reopen-a", &stale_a);
    {
        let host = app.windows.get(&window).unwrap();
        assert!(
            overlap
                .iter()
                .any(|pane| host.attach_pane_sessions.contains_key(pane)
                    && !host.mux.is_placeholder(*pane)),
            "reopen-a did not reconnect an overlapping pane {overlap:?}"
        );
    }

    pump_command_ack(app, window, &["space", "open", "reopen-b"]);
    assert_view_still_dead(app, window, "reopen-b", &stale_b);
    pump_host(app, window);
    assert_space_reattached(app, window, "reopen-b", &stale_b);
    {
        let host = app.windows.get(&window).unwrap();
        assert!(
            overlap
                .iter()
                .any(|pane| host.attach_pane_sessions.contains_key(pane)
                    && !host.mux.is_placeholder(*pane)
                    && host
                        .mux
                        .pane(*pane)
                        .is_some_and(|runtime| runtime.child_alive)),
            "reopen-b left an overlapping pane disconnected {overlap:?}"
        );
    }

    pump_command_ack(app, window, &["space", "open", "reopen-a"]);
    pump_host(app, window);
    assert_space_reattached(app, window, "reopen-a", &stale_a);
}
