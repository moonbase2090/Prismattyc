//! Escape through a real window. A private X display hosts the real event
//! loop; the X server's test extension (via `xdotool`) types the key, so the
//! event travels winit -> `App::window_event` -> the overlay handlers, with the
//! modifier state the platform reports. Unit tests cannot build winit key events.

use super::*;
use std::process::{Command, Stdio};
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

const CHILD_ENV: &str = "PRISMATTYC_RENDER_TEST_CHILD";
const TEST: &str = "escape_live_tests::escape_closes_overlays_in_a_real_window";
const PER_STEP: Duration = Duration::from_secs(4);

struct Step {
    name: &'static str,
    keys: &'static str,
    open: fn(&mut HostState),
    is_open: fn(&HostState) -> bool,
}

fn steps() -> Vec<Step> {
    let mut steps = Vec::new();
    for keys in ["Escape", "ctrl+Escape", "alt+Escape", "super+Escape"] {
        steps.push(Step {
            name: "command palette",
            keys,
            open: |host| open_command_palette(host, keybind::Action::CommandPalette),
            is_open: |host| host.palette.is_some(),
        });
        steps.push(Step {
            name: "space picker",
            keys,
            open: |host| {
                host.space_picker = Some(palette::SpacePicker::new(palette::SpacePickerKind::Open));
            },
            is_open: |host| host.space_picker.is_some(),
        });
    }
    steps.push(Step {
        name: "theme picker",
        keys: "Escape",
        open: open_theme_picker,
        is_open: |host| host.theme_picker.is_some(),
    });
    steps.push(Step {
        name: "find",
        keys: "Escape",
        open: open_find_prompt,
        is_open: |host| host.find.active,
    });
    steps.push(Step {
        name: "transparency",
        keys: "Escape",
        open: |host| {
            assert!(open_transparency(host));
        },
        is_open: |host| host.transparency.is_some(),
    });
    steps
}

fn x11_window_id(window: &Window) -> String {
    match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::Xlib(handle) => handle.window.to_string(),
        RawWindowHandle::Xcb(handle) => handle.window.get().to_string(),
        other => panic!("not an X11 window: {other:?}"),
    }
}

fn xdotool(args: &[&str]) -> bool {
    Command::new("xdotool")
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("xdotool is installed on the test image")
        .success()
}

#[test]
fn escape_closes_overlays_in_a_real_window() {
    if std::env::var_os(CHILD_ENV).is_none() {
        render_window_tests::run_in_private_display(TEST);
        return;
    }
    struct Proof {
        app: App,
        id: Option<WindowId>,
        steps: Vec<Step>,
        index: usize,
        sent_at: Option<Instant>,
        completed: bool,
    }
    impl ApplicationHandler<UserAction> for Proof {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            self.id = Some(self.app.open_window(event_loop, false).unwrap());
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
            self.app.window_event(event_loop, id, event);
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.app.about_to_wait(event_loop);
            event_loop.set_control_flow(ControlFlow::Poll);
            let Some(id) = self.id else {
                return;
            };
            let Some(host) = self.app.windows.get_mut(&id) else {
                return;
            };
            let step = &self.steps[self.index];
            match self.sent_at {
                None => {
                    // Open the overlay, give the X input focus to the window,
                    // then type the key for real.
                    (step.open)(host);
                    assert!((step.is_open)(host), "{} did not open", step.name);
                    let window = x11_window_id(&host.window);
                    if xdotool(&["windowfocus", &window]) && xdotool(&["key", step.keys]) {
                        self.sent_at = Some(Instant::now());
                    }
                }
                Some(sent) => {
                    if !(step.is_open)(host) {
                        eprintln!("escape live: {} closed by {}", step.name, step.keys);
                        self.index += 1;
                        self.sent_at = None;
                        if self.index == self.steps.len() {
                            self.completed = true;
                            self.app.windows.clear();
                            event_loop.exit();
                        }
                    } else if sent.elapsed() > PER_STEP {
                        panic!("{} stayed open after {}", step.name, step.keys);
                    }
                }
            }
        }
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
    let mut proof = Proof {
        app,
        id: None,
        steps: steps(),
        index: 0,
        sent_at: None,
        completed: false,
    };
    event_loop.run_app(&mut proof).unwrap();
    assert!(proof.completed);
    std::fs::write(
        std::env::var_os(render_window_tests::RESULT_ENV).unwrap(),
        b"complete",
    )
    .unwrap();
}
