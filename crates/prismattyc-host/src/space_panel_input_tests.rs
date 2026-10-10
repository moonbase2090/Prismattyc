//! Behavior of `space_panel::activate` and `space_panel::input_key_logical`.
//!
//! The private display keeps config writes, the daemon socket, and `xdg-open`
//! off the developer's seat. `PMUX` is switched to a logger after the window
//! exists so each spawned command is the argv the host actually built.

use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use winit::platform::x11::EventLoopBuilderExtX11;

const CHILD_ENV: &str = "PRISMATTYC_RENDER_TEST_CHILD";

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn activate_and_input_key_change_the_panel() {
    if std::env::var_os(CHILD_ENV).is_some() {
        exercise();
        std::fs::write(
            std::env::var_os(render_window_tests::RESULT_ENV).unwrap(),
            b"complete",
        )
        .unwrap();
        return;
    }
    render_window_tests::run_in_private_display(
        "space_panel::input_tests::activate_and_input_key_change_the_panel",
    );
}

fn exercise() {
    let pmux = fixture_bin("pmux");
    let pmuxd = fixture_bin("pmuxd");
    assert!(
        pmux.is_file() && pmuxd.is_file(),
        "build pmux and pmuxd at this checkout before the window fixture"
    );
    std::env::set_var("PMUX", &pmux);
    let socket = host_mux_socket().expect("private PMUX_SOCKET");
    let log_dir = runtime();
    let _daemon = Daemon(
        Command::new(&pmuxd)
            .arg("--socket")
            .arg(&socket)
            .args(["--", "/bin/sh"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log_dir.join("pmuxd.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_for_daemon();
    run_pmux(&["new", "--no-attach", "loose-seat"]);
    let loose = wait_session("loose-seat");
    assert!(
        loose.space_id.is_none(),
        "loose-seat joined a space: {:?}",
        loose.space_id
    );
    // Attach would exec prismattyc-host. The session is created either way.
    run_pmux(&["space", "create", "team-a", "--no-attach"]);
    let (owner, spaced) = wait_spaced("team-a");
    prepare_command_logger();

    struct Proof {
        app: App,
        owner: String,
        spaced_name: String,
        spaced_id: u64,
        loose_name: String,
        done: bool,
    }
    impl ApplicationHandler<UserAction> for Proof {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let id = self.app.open_window(event_loop, false).unwrap();
            let host = self.app.windows.get_mut(&id).unwrap();
            // The window is up. Later commands must hit the logger, not pmuxd.
            switch_to_command_logger();
            verify_input(host);
            verify_activate(
                host,
                &self.owner,
                &self.spaced_name,
                self.spaced_id,
                &self.loose_name,
            );
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
    let mut proof = Proof {
        app,
        owner,
        spaced_name: spaced.name,
        spaced_id: spaced.id,
        loose_name: loose.name,
        done: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.done, "the event loop must run the panel assertions");
}

fn verify_input(host: &mut HostState) {
    host.space_panel = None;
    host.dirty = false;
    assert!(!input_key_logical(host, &Key::Named(NamedKey::Escape)));
    assert!(host.space_panel.is_none());
    assert!(!host.dirty, "a missing panel does not repaint");

    show(host, "team-a", None, Choice::None);
    host.dirty = false;
    assert!(!input_key_logical(host, &Key::Named(NamedKey::Enter)));
    assert!(host.space_panel.as_ref().unwrap().input.is_none());
    assert!(!host.dirty, "a panel with no field does not repaint");

    show_input(host, "keep", false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Named(NamedKey::Escape)));
    assert!(host.space_panel.as_ref().unwrap().input.is_none());
    assert!(host.dirty);

    show_input(host, "abc", true, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Named(NamedKey::Backspace)));
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.value, "");
    assert!(!input.select_all);

    show_input(host, "abc", false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Named(NamedKey::Backspace)));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "ab"
    );

    show_input(host, "abc", false, Choice::Role("seat".into()));
    host.modifiers = ModifiersState::CONTROL;
    assert!(input_key_logical(host, &Key::Character("A".into())));
    assert!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .select_all
    );
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "abc"
    );

    host.modifiers = ModifiersState::CONTROL;
    let before = host
        .space_panel
        .as_ref()
        .unwrap()
        .input
        .as_ref()
        .unwrap()
        .value
        .clone();
    assert!(input_key_logical(host, &Key::Character("q".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        before,
        "ctrl-q is not select-all or paste"
    );

    host.modifiers = ModifiersState::ALT;
    assert!(input_key_logical(host, &Key::Character("b".into())));
    host.modifiers = ModifiersState::SUPER;
    assert!(input_key_logical(host, &Key::Character("c".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "abc"
    );

    host.modifiers = ModifiersState::empty();
    show_input(host, "ab", false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Character("c".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "abc"
    );

    show_input(host, "OLD", true, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Character("n".into())));
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.value, "n");
    assert!(!input.select_all);

    show_input(host, "ab", false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Character("\u{1}".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "ab",
        "a control character is not inserted"
    );

    show_input(host, "x".repeat(4096), false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Character("z".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value
            .len(),
        4096
    );

    set_clipboard("fresh");
    show_input(host, "OLD", true, Choice::Role("seat".into()));
    host.modifiers = ModifiersState::CONTROL;
    assert!(input_key_logical(host, &Key::Character("v".into())));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "fresh"
    );

    set_clipboard("0123456789\u{1}EXTRA");
    show_input(host, "a".repeat(4090), false, Choice::Role("seat".into()));
    host.modifiers = ModifiersState::CONTROL;
    assert!(input_key_logical(host, &Key::Character("v".into())));
    let pasted = &host
        .space_panel
        .as_ref()
        .unwrap()
        .input
        .as_ref()
        .unwrap()
        .value;
    assert_eq!(pasted.len(), 4096);
    assert!(pasted.ends_with("012345"), "paste stops at 4096: {pasted}");
    assert!(!pasted.contains('\u{1}'));
    assert!(!pasted.contains('E'));

    host.modifiers = ModifiersState::empty();
    show_input(host, "stay", false, Choice::Role("seat".into()));
    let (tx, rx) = std::sync::mpsc::channel();
    host.space_panel.as_mut().unwrap().pending = Some(rx);
    host.dirty = false;
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .value,
        "stay",
        "enter waits while a command is still running"
    );
    assert!(!host.dirty);
    drop(tx);

    show_input(host, "pilot", false, Choice::Role("seat".into()));
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    assert!(host.space_panel.as_ref().unwrap().input.is_none());
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .submitted
            .as_ref()
            .unwrap()
            .value,
        "pilot"
    );
    wait_argv("space role team-a seat pilot");

    show_input(host, "Design", true, Choice::LinkLabel);
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.label, "HTTP(S) link or absolute path");
    assert!(input.select_all);
    assert!(matches!(&input.choice, Choice::LinkTarget(label) if label == "Design"));
    host.space_panel
        .as_mut()
        .unwrap()
        .input
        .as_mut()
        .unwrap()
        .value = "https://example.com/doc".into();
    host.space_panel
        .as_mut()
        .unwrap()
        .input
        .as_mut()
        .unwrap()
        .select_all = false;
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    wait_argv("space link team-a Design https://example.com/doc");

    show_input(host, "weekly", false, Choice::SaveTemplate);
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    wait_argv("template save weekly --space team-a");

    show_input(host, "next-team", false, Choice::Template("base".into()));
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    match &host.space_panel.as_ref().unwrap().page {
        Page::Preview(template, name) => {
            assert_eq!(template, "base");
            assert_eq!(name, "next-team");
        }
        _ => panic!("template enter must open a preview"),
    }
    wait_argv("template preview base next-team --json");

    let log_before = argv_log();
    show_input(host, "ignored", false, Choice::None);
    assert!(input_key_logical(host, &Key::Named(NamedKey::Enter)));
    assert!(host.space_panel.as_ref().unwrap().input.is_none());
    assert_eq!(
        argv_log(),
        log_before,
        "enter on a non-editing choice does not spawn pmux"
    );
}

fn verify_activate(
    host: &mut HostState,
    owner: &str,
    spaced_name: &str,
    spaced_id: u64,
    loose_name: &str,
) {
    use_scratch_config();
    host.space_panel = None;
    host.dirty = false;
    activate(host, 0);
    assert!(host.space_panel.is_none());
    assert!(!host.dirty);

    show(host, "team-a", None, Choice::None);
    host.dirty = false;
    activate(host, 0);
    assert!(!host.dirty, "an inert row does not repaint");
    show(host, "team-a", None, Choice::Message);
    host.dirty = false;
    activate(host, 0);
    assert!(!host.dirty);
    show(host, "team-a", None, Choice::LinkTarget("label".into()));
    host.dirty = false;
    activate(host, 0);
    assert!(!host.dirty);
    show(host, "team-a", None, Choice::Settings);
    host.dirty = false;
    activate(host, 9);
    assert!(!host.dirty, "a missing row does not repaint");
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Team
    ));

    show(host, "team-a", None, Choice::Maintenance);
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Maintenance
    ));
    assert!(labels(host)
        .iter()
        .any(|label| label == "Check for updates"));
    assert_eq!(host.context_menu.as_ref().unwrap().selected, 0);
    let check = index_of(host, "Check for updates");
    activate(host, check);
    wait_argv("update --check");
    assert!(
        labels(host).iter().any(|label| label.contains("Loading")),
        "a running command replaces the rows: {:?}",
        labels(host)
    );

    show(host, "team-a", None, Choice::Settings);
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Settings
    ));
    assert!(labels(host).iter().any(|label| label.contains("Toasts:")));

    show(host, "team-a", None, Choice::Messages);
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Messages(_)
    ));

    host.context_menu = None;
    show_without_menu(host, "team-a", Choice::Session("seat".into()));
    activate(host, 0);
    assert!(host.context_menu.is_none());
    assert!(matches!(
        &host.space_panel.as_ref().unwrap().page,
        Page::Session(name) if name == "seat"
    ));

    show(host, "team-a", None, Choice::Role("seat".into()));
    activate(host, 0);
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.label, "Role");
    assert!(matches!(&input.choice, Choice::Role(name) if name == "seat"));

    show(host, "team-a", None, Choice::LinkLabel);
    activate(host, 0);
    assert_eq!(
        host.space_panel
            .as_ref()
            .unwrap()
            .input
            .as_ref()
            .unwrap()
            .label,
        "Link label"
    );

    show(host, "team-a", None, Choice::SaveTemplate);
    activate(host, 0);
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.value, "team-a");
    assert!(matches!(input.choice, Choice::SaveTemplate));

    show(host, "team-a", None, Choice::Template("base".into()));
    activate(host, 0);
    let input = host.space_panel.as_ref().unwrap().input.as_ref().unwrap();
    assert_eq!(input.value, "base-team");
    assert!(matches!(&input.choice, Choice::Template(name) if name == "base"));

    show(
        host,
        "team-a",
        None,
        Choice::Preference("spaces.autosave".into(), "true".into()),
    );
    activate(host, 0);
    assert!(
        config_body().contains("autosave = true"),
        "config: {}",
        config_body()
    );
    assert!(
        text_page(host).is_none(),
        "autosave save failed: {:?}",
        text_page(host)
    );

    show(
        host,
        "team-a",
        None,
        Choice::Preference("spaces.autosave".into(), "false".into()),
    );
    activate(host, 0);
    assert!(config_body().contains("autosave = false"));

    show(
        host,
        "team-a",
        None,
        Choice::Preference("space_autosave".into(), "true".into()),
    );
    activate(host, 0);
    assert!(config_body().contains("space_autosave = true"));

    show(
        host,
        "team-a",
        None,
        Choice::Preference("restore_blank_terminals".into(), "true".into()),
    );
    activate(host, 0);
    assert!(config_body().contains("restore_blank_terminals = true"));

    show(
        host,
        "team-a",
        None,
        Choice::Preference("start_at_login".into(), "true".into()),
    );
    activate(host, 0);
    assert!(config_body().contains("start_at_login = true"));

    show(
        host,
        "team-a",
        None,
        Choice::Preference("focus_border".into(), "violet".into()),
    );
    activate(host, 0);
    assert!(
        config_body().contains("focus_border = \"violet\"")
            || config_body().contains("focus_border = 'violet'"),
        "config: {}",
        config_body()
    );
    assert!(text_page(host).is_none());

    host.toasts = config::ToastLevel::All;
    show(
        host,
        "team-a",
        None,
        Choice::Preference("toasts".into(), "errors".into()),
    );
    activate(host, 0);
    assert_eq!(host.toasts, config::ToastLevel::Errors);
    assert!(
        config_body().contains("toasts = \"errors\"")
            || config_body().contains("toasts = 'errors'"),
        "config: {}",
        config_body()
    );

    host.link_click_mode = link_click::Mode::Plain;
    show(
        host,
        "team-a",
        None,
        Choice::Preference("link_click".into(), "modifier".into()),
    );
    activate(host, 0);
    assert_eq!(host.link_click_mode, link_click::Mode::Modifier);

    show(
        host,
        "team-a",
        None,
        Choice::Preference("link_click".into(), "plain".into()),
    );
    activate(host, 0);
    assert_eq!(host.link_click_mode, link_click::Mode::Plain);

    use_bad_config();
    show(
        host,
        "team-a",
        None,
        Choice::Preference("focus_border".into(), "bad".into()),
    );
    activate(host, 0);
    let text = text_page(host).expect("a failed preference save explains why");
    assert!(
        text.contains("Could not save preference"),
        "preference error text: {text}"
    );
    show(
        host,
        "team-a",
        None,
        Choice::Layout(config::LayoutMode::Sidebar),
    );
    activate(host, 0);
    let text = text_page(host).expect("a failed layout save explains why");
    assert!(
        text.contains("Could not save preference"),
        "layout error text: {text}"
    );
    use_scratch_config();

    host.spacing.layout = config::LayoutMode::Bars;
    show(
        host,
        "team-a",
        None,
        Choice::Layout(config::LayoutMode::Sidebar),
    );
    activate(host, 0);
    assert_eq!(host.spacing.layout, config::LayoutMode::Sidebar);
    assert!(text_page(host).is_none());

    show(host, "team-a", None, Choice::Back);
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Team
    ));
    wait_argv("space details team-a --json");

    show(host, "team-a", None, Choice::Templates);
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Templates
    ));
    wait_argv("template ls");

    show(
        host,
        "team-a",
        None,
        Choice::Attention {
            session: "seat".into(),
            pane: 3,
            revision: 9,
            resolve: true,
        },
    );
    activate(host, 0);
    wait_argv("attention resolve team-a seat 3 9");
    show(
        host,
        "team-a",
        None,
        Choice::Attention {
            session: "seat".into(),
            pane: 3,
            revision: 9,
            resolve: false,
        },
    );
    activate(host, 0);
    wait_argv("attention snooze team-a seat 3 9");

    show(host, "team-a", None, Choice::Reopen("seat".into()));
    activate(host, 0);
    wait_argv("session reopen seat --space team-a");

    show(
        host,
        "team-a",
        None,
        Choice::Create {
            template: "base".into(),
            name: "next".into(),
            launch: false,
        },
    );
    activate(host, 0);
    wait_argv("template create base next --prepare-only");
    assert!(
        !argv_log()
            .lines()
            .any(|line| line.contains("template create base next ") && line.contains("--launch")),
        "create without launch must not pass --launch:\n{}",
        argv_log()
    );

    show(
        host,
        "team-a",
        None,
        Choice::Create {
            template: "base".into(),
            name: "next-launch".into(),
            launch: true,
        },
    );
    activate(host, 0);
    wait_argv("template create base next-launch --prepare-only --launch");

    show(host, "team-a", None, Choice::Link("notaurl".into()));
    activate(host, 0);
    let text = text_page(host).expect("a rejected link explains why");
    assert!(text.contains("Could not open notaurl"), "{text}");

    show(
        host,
        "team-a",
        None,
        Choice::Link("https://example.com/team".into()),
    );
    activate(host, 0);
    wait_file_contains(&open_log(), "https://example.com/team");
    assert!(
        text_page(host).is_none(),
        "a spawned opener does not replace the page: {:?}",
        text_page(host)
    );

    host.last_space_open = Some(space_outcome::Report::for_test("team-a"));
    show(host, "team-a", None, Choice::Result);
    activate(host, 0);
    let text = text_page(host).expect("a stored result is shown");
    assert!(text.contains("team-a: applied"), "{text}");
    assert!(labels(host)
        .iter()
        .any(|label| label == "Retry opening this Space"));

    host.last_space_open = Some(space_outcome::Report::for_test("other-space"));
    show(host, "team-a", None, Choice::Result);
    activate(host, 0);
    assert!(
        labels(host).iter().any(|label| label.contains("Loading")),
        "a missing stored result asks the daemon: {:?}",
        labels(host)
    );
    assert!(host.space_panel.as_ref().unwrap().pending.is_some());
    assert!(
        text_page(host).is_none(),
        "a result for another space asks the daemon"
    );
    wait_argv("space result team-a");

    show(host, "team-a", None, Choice::Retry);
    activate(host, 0);
    assert!(host.context_menu.is_none(), "retry closes the menu");
    assert!(host.space_opens.busy(), "retry queues the space open");

    host.layout_dirty = false;
    host.space_team_focus = None;
    host.attach_pane_sessions.clear();
    let pane = host.mux.focused_id();
    host.attach_pane_sessions
        .insert(pane, spaced_id.to_string());
    show(
        host,
        "team-a",
        Some(owner.to_string()),
        Choice::View(spaced_name.into()),
    );
    activate(host, 0);
    assert!(host.context_menu.is_none());
    assert!(host.layout_dirty, "focusing a live pane dirties the layout");
    assert_eq!(host.mux.focused_id(), pane);
    assert!(host.space_team_focus.is_none());

    host.layout_dirty = false;
    host.space_team_focus = None;
    host.attach_pane_sessions.clear();
    show(
        host,
        "team-a",
        Some(owner.to_string()),
        Choice::View(spaced_name.into()),
    );
    activate(host, 0);
    assert_eq!(
        host.space_team_focus
            .as_ref()
            .map(|(id, session, _)| (id.clone(), *session)),
        Some((owner.to_string(), spaced_id))
    );
    assert!(host.context_menu.is_none());
    assert!(host.space_opens.busy());

    host.layout_dirty = false;
    host.space_team_focus = None;
    host.attach_pane_sessions.clear();
    show(host, "loose", None, Choice::View(loose_name.into()));
    host.context_menu.as_mut().unwrap().selected = 4;
    activate(host, 0);
    assert!(matches!(
        host.space_panel.as_ref().unwrap().page,
        Page::Team
    ));
    assert!(text_page(host).is_none());
    assert!(host.space_team_focus.is_none());
    assert!(!host.layout_dirty);
    assert_eq!(host.context_menu.as_ref().unwrap().selected, 0);
    assert!(host.dirty);

    host.layout_dirty = false;
    host.attach_pane_sessions.clear();
    show(
        host,
        "team-a",
        Some(owner.to_string()),
        Choice::View("missing-seat".into()),
    );
    activate(host, 0);
    let text = text_page(host).expect("a missing session is reported");
    assert!(text.contains("Session moved or stopped"), "{text}");

    let first = host.mux.focused_id();
    let second = host
        .mux
        .split_focused("/bin/sh", &[], prismattyc_mux::Axis::Horizontal, 0.5)
        .expect("split");
    assert!(host.mux.close_focused().expect("close split pane"));
    let survivor = host.mux.focused_id();
    let stale = if survivor == first { second } else { first };
    assert!(host
        .mux
        .tab_panes()
        .iter()
        .all(|(_, panes)| !panes.contains(&stale)));
    host.layout_dirty = false;
    host.attach_pane_sessions.clear();
    host.attach_pane_sessions
        .insert(stale, spaced_id.to_string());
    show(
        host,
        "team-a",
        Some(owner.to_string()),
        Choice::View(spaced_name.into()),
    );
    activate(host, 0);
    assert!(host.layout_dirty);
    assert!(host.context_menu.is_none());
    assert_eq!(
        host.mux.focused_id(),
        survivor,
        "a pane that is not on a tab is not focused"
    );
    assert!(host.space_team_focus.is_none());
}

fn show(host: &mut HostState, space: &str, owner: Option<String>, choice: Choice) {
    host.context_menu = Some(ContextMenu::new(ContextMenuKind::SpaceChip));
    host.context_menu.as_mut().unwrap().selected = 4;
    host.space_panel = Some(panel(space, owner, Some(choice)));
    host.dirty = false;
}

fn show_without_menu(host: &mut HostState, space: &str, choice: Choice) {
    host.context_menu = None;
    host.space_panel = Some(panel(space, None, Some(choice)));
    host.dirty = false;
}

fn show_input(host: &mut HostState, value: impl AsRef<str>, select_all: bool, choice: Choice) {
    let mut panel = panel("team-a", None, None);
    panel.input = Some(Input {
        label: "Field".into(),
        value: value.as_ref().to_string(),
        select_all,
        choice,
    });
    host.space_panel = Some(panel);
    host.dirty = false;
    host.modifiers = ModifiersState::empty();
}

fn panel(space: &str, owner: Option<String>, choice: Option<Choice>) -> Panel {
    Panel {
        space: space.into(),
        maintenance: false,
        owner,
        details: None,
        page: Page::Team,
        rows: choice
            .map(|choice| {
                vec![Row {
                    label: "row".into(),
                    detail: String::new(),
                    choice,
                }]
            })
            .unwrap_or_default(),
        input: None,
        submitted: None,
        pending: None,
        scroll: 0,
        columns: 80,
        chrome_layout: Some(config::LayoutMode::Bars),
    }
}

fn labels(host: &HostState) -> Vec<String> {
    host.space_panel
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .map(|row| row.label.clone())
        .collect()
}

fn index_of(host: &HostState, label: &str) -> usize {
    labels(host)
        .iter()
        .position(|row| row == label)
        .unwrap_or_else(|| panic!("missing row {label}: {:?}", labels(host)))
}

fn text_page(host: &HostState) -> Option<String> {
    match &host.space_panel.as_ref()?.page {
        Page::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn set_clipboard(text: &str) {
    arboard::Clipboard::new()
        .expect("clipboard")
        .set_text(text)
        .expect("set clipboard text");
}

fn use_scratch_config() {
    std::env::set_var("PRISMATTYC_CONFIG", scratch_config());
}

fn use_bad_config() {
    let dir = runtime().join("config-is-a-directory");
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("PRISMATTYC_CONFIG", dir);
}

fn scratch_config() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("prismattyc/config.toml")
}

fn config_body() -> String {
    std::fs::read_to_string(config::config_path()).unwrap_or_default()
}

fn runtime() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("private runtime dir"))
}

fn argv_log() -> String {
    std::fs::read_to_string(runtime().join("argv.log")).unwrap_or_default()
}

fn open_log() -> PathBuf {
    runtime().join("open.log")
}

fn wait_argv(needle: &str) {
    wait_file_contains(&runtime().join("argv.log"), needle);
}

fn wait_file_contains(path: &Path, needle: &str) {
    let start = Instant::now();
    loop {
        let body = std::fs::read_to_string(path).unwrap_or_default();
        if body.contains(needle) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "missing {needle} in {}:\n{body}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture_bin(name: &str) -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(name)
}

fn run_pmux(args: &[&str]) {
    let output = Command::new(fixture_bin("pmux"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "pmux {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_daemon() {
    let start = Instant::now();
    while attach_log::live_snapshot().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "private daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_session(name: &str) -> prismattyc_mux::SessionSnapshot {
    let start = Instant::now();
    loop {
        if let Some(snapshot) = attach_log::live_snapshot() {
            if let Some(session) = snapshot
                .sessions
                .into_iter()
                .find(|session| session.name == name)
            {
                return session;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "missing session {name}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_spaced(name: &str) -> (String, prismattyc_mux::SessionSnapshot) {
    let start = Instant::now();
    loop {
        if let Ok(space) = load_space(&spaces_dir(), name) {
            if let Some(id) = space.id.clone() {
                if let Some(snapshot) = attach_log::live_snapshot() {
                    if let Some(session) = snapshot
                        .sessions
                        .into_iter()
                        .find(|session| session.space_id.as_deref() == Some(id.as_str()))
                    {
                        return (id, session);
                    }
                }
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "space {name} has no live session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn prepare_command_logger() {
    let bin = runtime().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_exe(
        &bin.join("pmux"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PMUX_ARG_LOG\"\nexit 0\n",
    );
    write_exe(
        &bin.join("xdg-open"),
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$XDG_OPEN_LOG\"\nexit 0\n",
    );
    let _ = std::fs::File::create(runtime().join("argv.log")).and_then(|mut file| file.flush());
    let _ = std::fs::File::create(runtime().join("open.log"));
    std::env::set_var("PMUX_ARG_LOG", runtime().join("argv.log"));
    std::env::set_var("XDG_OPEN_LOG", runtime().join("open.log"));
}

fn switch_to_command_logger() {
    let bin = runtime().join("bin");
    std::env::set_var("PMUX", bin.join("pmux"));
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    std::env::set_var("PATH", path);
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}
