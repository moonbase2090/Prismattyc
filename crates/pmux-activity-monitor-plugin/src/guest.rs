// SPDX-License-Identifier: MIT

use std::cell::RefCell;
use std::collections::BTreeMap;

use lunatui_term::{Event, KeyCode, KeyEvent, Modifiers};
use pmux_plugin_api::guest::pmux::{
    activity::{manage, read},
    plugin::{host, lifecycle},
    view::frame,
};
use pmux_plugin_api::monitor::GraphStyle;
use pmux_plugin_api::{activity, modifiers as bits, API_VERSION};

use crate::{
    app::Monitor,
    capture,
    model::{Host, Link},
    ui::Palette,
};

const POLL_INTERVAL_MS: u16 = 250;

struct View {
    reference: lifecycle::ViewReference,
    monitor: Monitor,
    handles: Vec<u64>,
    last_poll: Option<u64>,
    frame_bytes: u32,
    readable: bool,
    clock: frame::Clock,
    first_tick: Option<u64>,
    next_host: usize,
}

impl View {
    fn new(context: lifecycle::Context) -> Result<Self, String> {
        let lifecycle::Role::View(reference) = context.role else {
            return Err("monitor requires a view".into());
        };
        let preferences = &reference.preferences;
        let palette = match preferences.palette {
            frame::Palette::Light => Palette::LIGHT,
            frame::Palette::Dark => Palette::DARK,
            frame::Palette::Auto if preferences.dark_background == Some(false) => Palette::LIGHT,
            frame::Palette::Auto => Palette::DARK,
        };
        let clock = host::now();
        let mut monitor = Monitor::new(vec!["local".into()], palette, clock.wall_ms);
        monitor.ui.graph_style = GraphStyle::from_view(&reference.contribution)?;
        monitor.ui.motion = preferences.motion && !preferences.snapshot;
        let readable = context
            .capabilities
            .iter()
            .any(|capability| capability.name == "pmux:activity/read" && capability.available);
        if !readable {
            monitor.hosts[0].link = Link::PermissionDenied;
        }
        Ok(Self {
            reference,
            monitor,
            handles: Vec::new(),
            last_poll: None,
            frame_bytes: context.limits.frame_bytes,
            readable,
            clock,
            first_tick: None,
            next_host: 0,
        })
    }

    fn poll(&mut self, clock: &frame::Clock) {
        self.monitor.now_ms = clock.wall_ms;
        if !self.readable
            || self.last_poll.is_some_and(|last| {
                clock.monotonic_ms.saturating_sub(last) < u64::from(POLL_INTERVAL_MS)
            })
        {
            return;
        }
        self.last_poll = Some(clock.monotonic_ms);
        let endpoints = match read::endpoints() {
            Ok(endpoints) => endpoints,
            Err(_) => {
                for host in &mut self.monitor.hosts {
                    host.disconnect(clock.wall_ms);
                }
                return;
            }
        };
        if endpoints
            .iter()
            .map(|endpoint| endpoint.handle)
            .collect::<Vec<_>>()
            != self.handles
        {
            self.next_host = 0;
            let selected = self.monitor.ui.selected.as_ref().and_then(|(index, seat)| {
                self.handles
                    .get(*index)
                    .map(|handle| (*handle, seat.clone()))
            });
            let mut old = self
                .handles
                .drain(..)
                .zip(std::mem::take(&mut self.monitor.hosts))
                .collect::<BTreeMap<_, _>>();
            for endpoint in endpoints {
                let mut host = old
                    .remove(&endpoint.handle)
                    .unwrap_or_else(|| Host::new(&endpoint.label));
                host.label = endpoint.label;
                self.handles.push(endpoint.handle);
                self.monitor.hosts.push(host);
            }
            self.monitor.ui.selected = selected.and_then(|(handle, seat)| {
                self.handles
                    .iter()
                    .position(|candidate| *candidate == handle)
                    .map(|index| (index, seat))
            });
        }
        let read_budget = (pmux_plugin_api::MAX_ACTIVITY_READS_PER_SECOND
            / (1000 / POLL_INTERVAL_MS))
            .saturating_sub(1);
        let mut remaining = read_budget.min(pmux_plugin_api::MAX_HOST_IMPORTS_PER_CALL - 1) / 2;
        for _ in 0..self.handles.len() {
            if remaining == 0 {
                break;
            }
            let index = self.next_host;
            self.next_host = (index + 1) % self.handles.len();
            let handle = self.handles[index];
            let host = &mut self.monitor.hosts[index];
            // Two pages cover the daemon's 512-entry ring. Further activity is
            // picked up on the next tick instead of monopolizing the worker.
            for _ in 0..2 {
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                let cursor = host.epoch.as_ref().map(|epoch| read::Cursor {
                    epoch: epoch.clone(),
                    sequence: host.sequence,
                });
                match activity::poll(handle, cursor.as_ref(), 256) {
                    Ok(page) => {
                        let more = page.has_more;
                        host.apply(page, clock.monotonic_ms);
                        if !more {
                            break;
                        }
                    }
                    Err(error) => {
                        match error {
                            read::Error::RateLimited => {}
                            read::Error::Connecting => host.link = Link::Connecting,
                            read::Error::FeedDisabled => host.link = Link::Disabled,
                            read::Error::PermissionDenied | read::Error::Revoked => {
                                host.link = Link::PermissionDenied
                            }
                            read::Error::IncompatibleSchema => host.link = Link::Unsupported,
                            _ => host.disconnect(clock.wall_ms),
                        }
                        break;
                    }
                }
            }
        }
    }

    fn event(&mut self, event: frame::Event) -> Result<lifecycle::Reply, String> {
        match event {
            frame::Event::Close => return Ok(lifecycle::Reply::Closed),
            frame::Event::Resize(viewport) => self.reference.viewport = viewport,
            frame::Event::Tick(clock) => {
                self.first_tick.get_or_insert(clock.monotonic_ms);
                self.poll(&clock);
                self.clock = clock;
            }
            frame::Event::Key(key) => {
                self.monitor.now_ms = self.clock.wall_ms;
                if self.monitor.event(&Event::Key(key_event(key))) {
                    return Ok(lifecycle::Reply::Closed);
                }
                for request in self.monitor.ui.take_cleanup_requests() {
                    let result = self
                        .handles
                        .get(request.host)
                        .copied()
                        .ok_or_else(|| "selected activity endpoint disappeared".to_owned())
                        .and_then(|endpoint| manage::remove_stopped(endpoint, request.session_id));
                    self.monitor
                        .ui
                        .resolve_cleanup(request, result, &mut self.monitor.hosts);
                }
            }
            frame::Event::Mouse(mouse) => {
                self.monitor.now_ms = self.clock.wall_ms;
                self.monitor.event(&Event::Mouse(mouse_event(mouse)));
            }
        }
        if self.reference.preferences.snapshot
            && self
                .monitor
                .hosts
                .iter()
                .any(|host| host.link == Link::Connecting || host.more)
            && self
                .clock
                .monotonic_ms
                .saturating_sub(self.first_tick.unwrap_or(self.clock.monotonic_ms))
                < 3000
        {
            return Ok(lifecycle::Reply::Idle);
        }
        if self.reference.preferences.snapshot
            && self
                .monitor
                .hosts
                .iter()
                .all(|host| matches!(host.link, Link::Connecting | Link::Retrying { .. }))
        {
            return Err("activity feed unavailable".into());
        }
        let content = capture::render(
            &mut self.monitor,
            &self.reference.viewport,
            self.clock.monotonic_ms,
            self.frame_bytes,
        )?;
        Ok(lifecycle::Reply::Frame(lifecycle::ViewFrame {
            next_tick_ms: if self.monitor.animating(self.clock.monotonic_ms) {
                33
            } else {
                POLL_INTERVAL_MS
            },
            id: self.reference.id,
            content,
        }))
    }
}

fn key_event(key: frame::Key) -> KeyEvent {
    let mut modifiers = Modifiers::new();
    for (bit, apply) in [
        (bits::SHIFT, Modifiers::shift as fn(Modifiers) -> Modifiers),
        (bits::ALT, Modifiers::alt),
        (bits::CONTROL, Modifiers::control),
        (bits::SUPER, Modifiers::super_key),
        (bits::HYPER, Modifiers::hyper),
        (bits::META, Modifiers::meta),
        (bits::CAPS_LOCK, Modifiers::caps_lock),
        (bits::NUM_LOCK, Modifiers::num_lock),
    ] {
        if key.modifiers & bit != 0 {
            modifiers = apply(modifiers);
        }
    }
    let code = match key.code {
        frame::KeyCode::Character(character) => KeyCode::Char(character),
        frame::KeyCode::Function(number) => KeyCode::F(number),
        frame::KeyCode::Named(named) => match named {
            frame::NamedKey::Escape => KeyCode::Esc,
            frame::NamedKey::Enter => KeyCode::Enter,
            frame::NamedKey::Tab => KeyCode::Tab,
            frame::NamedKey::BackTab => {
                modifiers = modifiers.shift();
                KeyCode::Tab
            }
            frame::NamedKey::Backspace => KeyCode::Backspace,
            frame::NamedKey::Delete => KeyCode::Delete,
            frame::NamedKey::Insert => KeyCode::Insert,
            frame::NamedKey::Up => KeyCode::Up,
            frame::NamedKey::Down => KeyCode::Down,
            frame::NamedKey::Left => KeyCode::Left,
            frame::NamedKey::Right => KeyCode::Right,
            frame::NamedKey::Home => KeyCode::Home,
            frame::NamedKey::End => KeyCode::End,
            frame::NamedKey::PageUp => KeyCode::PageUp,
            frame::NamedKey::PageDown => KeyCode::PageDown,
        },
    };
    KeyEvent::press(code).modifiers(modifiers)
}

fn mouse_event(mouse: frame::Mouse) -> lunatui_term::MouseEvent {
    use lunatui_term::{MouseButton, MouseKind, Scroll};
    let button = |button| match button {
        frame::MouseButton::Left => MouseButton::Left,
        frame::MouseButton::Middle => MouseButton::Middle,
        frame::MouseButton::Right => MouseButton::Right,
    };
    let kind = match mouse.action {
        frame::MouseAction::Press(pressed) => MouseKind::Down(button(pressed)),
        frame::MouseAction::Release(released) => MouseKind::Up(button(released)),
        frame::MouseAction::Drag(held) => MouseKind::Drag(button(held)),
        frame::MouseAction::Moved => MouseKind::Move,
        frame::MouseAction::Wheel(frame::Wheel::Up) => MouseKind::Scroll(Scroll::Up),
        frame::MouseAction::Wheel(frame::Wheel::Down) => MouseKind::Scroll(Scroll::Down),
        frame::MouseAction::Wheel(frame::Wheel::Left) => MouseKind::Scroll(Scroll::Left),
        frame::MouseAction::Wheel(frame::Wheel::Right) => MouseKind::Scroll(Scroll::Right),
    };
    let mut modifiers = Modifiers::new();
    for (bit, apply) in [
        (
            pmux_plugin_api::modifiers::SHIFT,
            Modifiers::shift as fn(Modifiers) -> Modifiers,
        ),
        (pmux_plugin_api::modifiers::ALT, Modifiers::alt),
        (pmux_plugin_api::modifiers::CONTROL, Modifiers::control),
    ] {
        if mouse.modifiers & bit != 0 {
            modifiers = apply(modifiers);
        }
    }
    lunatui_term::MouseEvent::new(kind, mouse.column, mouse.row).modifiers(modifiers)
}

thread_local! { static VIEW: RefCell<Option<View>> = const { RefCell::new(None) }; }
struct Plugin;
impl pmux_plugin_api::guest::Guest for Plugin {
    fn handle(message: lifecycle::Message) -> Result<lifecycle::Reply, String> {
        VIEW.with(|view| match message {
            lifecycle::Message::Start(context) => {
                if context.api_version != API_VERSION {
                    return Err("incompatible monitor API".into());
                }
                *view.borrow_mut() = Some(View::new(context)?);
                Ok(lifecycle::Reply::Ready(API_VERSION.into()))
            }
            lifecycle::Message::View(event) => view
                .borrow_mut()
                .as_mut()
                .ok_or("monitor was not started")?
                .event(event.event),
            lifecycle::Message::Stop => {
                *view.borrow_mut() = None;
                Ok(lifecycle::Reply::Idle)
            }
            lifecycle::Message::Tool(_) => Err("monitor has no command contributions".into()),
        })
    }
}
pmux_plugin_api::guest::export_plugin!(Plugin);
