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
    let app = App::new(
        cli,
        config::ConfigFile::default(),
        None,
        event_loop.create_proxy(),
    )
    .unwrap();
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
    refresh_space_views(&mut follower);
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
    refresh_space_views(&mut follower);
    assert_eq!(
        follower.space_rail.live_pane_names["renamed-a"],
        vec!["a-1"]
    );
    capture(&mut follower, "a-renamed-follower");
    command(&["space", "rm", "a"]);
    command(&["space", "rename", "renamed-a", "a"]);
    for host in [app.windows.get_mut(&source).unwrap(), &mut follower] {
        host.last_space_refresh = None;
        refresh_space_views(host);
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
        self.app.pump(event_loop);
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
                    refresh_space_views(host);
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
                self.app.pump(event_loop);
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
                prismattyc_mux::attach_tabs::save(&path, &foreign).unwrap();
                poll_host_attach_tabs(a);
                assert_eq!(
                    a.mux.remote_pane_id(a.mux.focused_id()),
                    Some(self.original[0].1),
                    "foreign view reached the source window"
                );
                assert!(a.space_opens.blocks_persist());
                prismattyc_mux::attach_tabs::save(&path, &original).unwrap();
                poll_host_attach_tabs(a);
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
                refresh_space_views(a);
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
                    refresh_space_views(host);
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
