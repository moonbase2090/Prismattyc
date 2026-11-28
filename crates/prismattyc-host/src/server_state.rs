//! The pmux server's state as the app sees it, and the calm card shown in a
//! window while the server is down.
//!
//! A background probe checks the server once a second. While it is stopped,
//! starting, or failed to start, a window that has nothing else to show gets
//! one card instead of an error screen per attached pane. Enter starts the
//! server; the card's other button opens a local shell in a new tab.
use super::*;
use graphite::{Face, Rect};
use std::sync::atomic::AtomicU8;

/// What the app knows about the pmux server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServerState {
    Running,
    /// The app asked the server to start and is waiting for it.
    Starting,
    /// The server stopped answering at `since`.
    Stopped {
        since: SystemTime,
    },
    /// The last start failed with this message.
    Failed(String),
}

/// A brief gap (a restart, an update) does not show the card.
pub(crate) const STOPPED_GRACE: Duration = Duration::from_secs(2);

/// The state after a liveness sample. Pure, so the rules are tested.
pub(crate) fn next(state: &ServerState, live: bool, now: SystemTime) -> ServerState {
    match (state, live) {
        (_, true) => ServerState::Running,
        (ServerState::Running, false) => ServerState::Stopped { since: now },
        // Starting waits for its start to finish; Stopped keeps its time and
        // Failed keeps its message until the next start or a live sample.
        (other, false) => other.clone(),
    }
}

/// Whether a window shows the card. `local_live` is true when the window
/// has a running local shell; `dismissed` when the user chose a local shell
/// from the card. `busy` covers the app's own update and restart flows,
/// which stop and start the server on purpose.
pub(crate) fn shows_card(
    state: &ServerState,
    now: SystemTime,
    local_live: bool,
    dismissed: bool,
    busy: bool,
) -> bool {
    if busy || dismissed {
        return false;
    }
    match state {
        ServerState::Running => false,
        ServerState::Starting => !local_live,
        ServerState::Failed(_) => true,
        ServerState::Stopped { since } => {
            !local_live
                && now
                    .duration_since(*since)
                    .is_ok_and(|gap| gap >= STOPPED_GRACE)
        }
    }
}

/// 0 unknown, 1 live, 2 down: the probe thread's last sample.
pub(crate) struct Probe {
    sample: Arc<AtomicU8>,
}

impl Probe {
    /// Probe `socket` once a second on a background thread, and wake the
    /// event loop when the answer changes.
    pub(crate) fn spawn(socket: PathBuf, wake: mux::Wake) -> Self {
        let sample = Arc::new(AtomicU8::new(0));
        let shared = Arc::clone(&sample);
        let _ = std::thread::Builder::new()
            .name("server-probe".into())
            .spawn(move || loop {
                let live = matches!(
                    prismattyc_mux::probe_socket_liveness(&socket),
                    prismattyc_mux::SocketLiveness::Live
                );
                let value = if live { 1 } else { 2 };
                // While the server is down, wake every sample so the card
                // appears after the grace and its age stays current.
                if shared.swap(value, Ordering::Relaxed) != value || !live {
                    wake();
                }
                std::thread::sleep(Duration::from_secs(1));
            });
        Self { sample }
    }

    /// The last sample, or `None` before the first one.
    pub(crate) fn live(&self) -> Option<bool> {
        match self.sample.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }
}

/// What a key or click on the card asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardAction {
    Start,
    LocalShell,
    /// Save every session, then stop the server (confirmed by the caller).
    Stop,
}

struct CardCopy {
    dot: fn(&graphite::Tokens) -> graphite::Rgb,
    title: &'static str,
    body: [String; 2],
    primary: Option<&'static str>,
    footer: String,
}

fn ago(since: SystemTime, now: SystemTime) -> String {
    let minutes = now
        .duration_since(since)
        .map_or(0, |gap| gap.as_secs() / 60);
    match minutes {
        0 => "Stopped just now".into(),
        1 => "Stopped 1 minute ago".into(),
        n if n < 120 => format!("Stopped {n} minutes ago"),
        n => format!("Stopped {} hours ago", n / 60),
    }
}

fn copy(state: &ServerState, now: SystemTime) -> CardCopy {
    match state {
        ServerState::Running | ServerState::Stopped { .. } => CardCopy {
            dot: |tok| tok.idle,
            title: "The pmux server is stopped",
            body: [
                "Your Spaces and saved sessions are kept. Start the".into(),
                "server to open them again, or work in a local shell.".into(),
            ],
            primary: Some("Start the pmux server"),
            footer: match state {
                ServerState::Stopped { since } => ago(*since, now),
                _ => String::new(),
            },
        },
        ServerState::Starting => CardCopy {
            dot: |tok| tok.unseen,
            title: "Starting the pmux server…",
            body: [
                "Your sessions open again as the server comes up.".into(),
                String::new(),
            ],
            primary: None,
            footer: "Starting".into(),
        },
        ServerState::Failed(message) => CardCopy {
            dot: |tok| tok.attention,
            title: "The pmux server did not start",
            body: [
                message.lines().next().unwrap_or_default().to_string(),
                "Your saved sessions are unchanged.".into(),
            ],
            primary: Some("Try again"),
            footer: "Run pmux doctor in a local shell for details".into(),
        },
    }
}

/// The card's labels, for accessibility and render status.
pub(crate) fn rows(state: &ServerState) -> (String, Vec<String>) {
    let copy = copy(state, SystemTime::now());
    let mut rows = Vec::new();
    if let Some(primary) = copy.primary {
        rows.push(primary.to_string());
    }
    rows.push("Open a local shell".into());
    (copy.title.to_string(), rows)
}

/// Paint the card centered in the window. Returns the two button rects
/// (primary, local shell) for the pointer; a missing primary is empty.
pub(crate) fn paint(
    host: &HostState,
    state: &ServerState,
    buffer: &mut [u32],
    width: usize,
    height: usize,
) -> [Rect; 2] {
    let tok = graphite::theme_tokens(&host.theme);
    let chrome = host.mux.geom().chrome;
    let s = |d: f32| d * chrome.scale_milli as f32 / 1000.0;
    let copy = copy(state, SystemTime::now());
    // Dead panes behind the card show nothing, not their error screens.
    let geom = host.mux.geom();
    for (_, _, rect) in host.mux.panes_and_rects() {
        let (x, y, w, h) = geom.pane_slot_px(rect);
        graphite::fill_round_rect(buffer, width, Rect::new(x, y, w, h), 0.0, tok.ground, 255);
    }
    let card_w = (s(520.0) as usize).min(width.saturating_sub(s(32.0) as usize));
    let card_h = s(236.0) as usize;
    let card = Rect::new(
        width.saturating_sub(card_w) / 2,
        height.saturating_sub(card_h) / 2,
        card_w,
        card_h,
    );
    crate::graphite_overlays::modal_panel(
        buffer,
        width,
        card,
        &tok,
        raster::OverlaySurface::default(),
    );
    let left = card.x as f32 + s(28.0);
    let title_y = card.y as f32 + s(40.0);
    let dot = s(10.0) as usize;
    graphite::fill_round_rect(
        buffer,
        width,
        Rect::new(left as usize, title_y as usize - dot / 2, dot, dot),
        dot as f32 / 2.0,
        (copy.dot)(&tok),
        255,
    );
    graphite::draw_text(
        buffer,
        width,
        left + s(20.0),
        title_y,
        Face::SemiBold,
        s(16.0),
        copy.title,
        tok.text_strong,
        card.x,
        card.right(),
    );
    for (index, line) in copy.body.iter().enumerate() {
        let line = graphite::ellipsize(Face::Regular, s(13.0), line, card.w as f32 - s(56.0));
        graphite::draw_text(
            buffer,
            width,
            left,
            title_y + s(34.0) + s(20.0) * index as f32,
            Face::Regular,
            s(13.0),
            &line,
            tok.text,
            card.x,
            card.right(),
        );
    }
    let button_h = s(34.0) as usize;
    let button_y = card.y + s(128.0) as usize;
    let mut x = left as usize;
    let mut buttons = [Rect::new(0, 0, 0, 0); 2];
    if let Some(primary) = copy.primary {
        let label_w = graphite::text_width(Face::SemiBold, s(13.0), primary).ceil() as usize;
        let w = label_w + s(64.0) as usize;
        let rect = Rect::new(x, button_y, w, button_h);
        let accent = graphite::accent(&tok, focus_border_rgb(host.focus_border));
        graphite::fill_round_rect(buffer, width, rect, s(8.0), accent, 255);
        let ink = if tok.variant == graphite_core::ThemeVariant::Light {
            [255, 255, 255]
        } else {
            tok.ground
        };
        let cy = (button_y + button_h / 2) as f32;
        graphite::draw_text(
            buffer,
            width,
            x as f32 + s(14.0),
            cy,
            Face::SemiBold,
            s(13.0),
            primary,
            ink,
            rect.x,
            rect.right(),
        );
        graphite::draw_text(
            buffer,
            width,
            (rect.right() as f32) - s(46.0),
            cy,
            Face::Mono,
            s(11.0),
            "Enter",
            ink,
            rect.x,
            rect.right(),
        );
        buttons[0] = rect;
        x = rect.right() + s(12.0) as usize;
    }
    let shell = "Open a local shell";
    let chord = host
        .keymap
        .spellings(keybind::Action::NewTab)
        .into_iter()
        .next()
        .unwrap_or_default();
    let label_w = graphite::text_width(Face::Regular, s(13.0), shell).ceil() as usize;
    let chord_w = graphite::text_width(Face::Mono, s(11.0), &chord).ceil() as usize;
    let rect = Rect::new(x, button_y, label_w + chord_w + s(44.0) as usize, button_h);
    graphite::outlined_round_rect(buffer, width, rect, s(8.0), tok.field_line, tok.panel);
    let cy = (button_y + button_h / 2) as f32;
    graphite::draw_text(
        buffer,
        width,
        rect.x as f32 + s(14.0),
        cy,
        Face::Regular,
        s(13.0),
        shell,
        tok.text,
        rect.x,
        rect.right(),
    );
    graphite::draw_text(
        buffer,
        width,
        (rect.right() - chord_w) as f32 - s(14.0),
        cy,
        Face::Mono,
        s(11.0),
        &chord,
        tok.key_text,
        rect.x,
        rect.right(),
    );
    buttons[1] = rect;
    graphite::fill_round_rect(
        buffer,
        width,
        Rect::new(
            card.x + s(20.0) as usize,
            card.y + s(186.0) as usize,
            card.w.saturating_sub(s(40.0) as usize),
            1,
        ),
        0.0,
        tok.hairline,
        255,
    );
    graphite::draw_text(
        buffer,
        width,
        left,
        card.y as f32 + s(208.0),
        Face::Regular,
        s(12.0),
        &copy.footer,
        tok.muted,
        card.x,
        card.right(),
    );
    buttons
}

/// What a key does while the card is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardKey {
    /// Not the card's key: the new-tab key, the palette and the rest work.
    Pass,
    /// Enter while starting: it must not reach a dead pane.
    Swallow,
    Act(CardAction),
}

/// Enter starts the server when the card offers it.
pub(crate) fn key(state: &ServerState, key: &Key) -> CardKey {
    match key {
        Key::Named(NamedKey::Enter)
            if matches!(state, ServerState::Stopped { .. } | ServerState::Failed(_)) =>
        {
            CardKey::Act(CardAction::Start)
        }
        Key::Named(NamedKey::Enter) => CardKey::Swallow,
        _ => CardKey::Pass,
    }
}

/// Which card button is under the pointer.
pub(crate) fn hit(buttons: &[Rect; 2], x: usize, y: usize) -> Option<CardAction> {
    let inside = |rect: &Rect| {
        rect.w > 0 && x >= rect.x && x < rect.right() && y >= rect.y && y < rect.y + rect.h
    };
    if inside(&buttons[0]) {
        Some(CardAction::Start)
    } else if inside(&buttons[1]) {
        Some(CardAction::LocalShell)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn samples_move_between_running_and_stopped_and_keep_failures() {
        let stopped = next(&ServerState::Running, false, at(10));
        assert_eq!(stopped, ServerState::Stopped { since: at(10) });
        // A second down sample keeps the original time.
        assert_eq!(next(&stopped, false, at(20)), stopped);
        assert_eq!(next(&stopped, true, at(21)), ServerState::Running);
        assert_eq!(
            next(&ServerState::Starting, false, at(5)),
            ServerState::Starting
        );
        let failed = ServerState::Failed("socket directory is not writable".into());
        assert_eq!(next(&failed, false, at(5)), failed);
        assert_eq!(next(&failed, true, at(5)), ServerState::Running);
    }

    #[test]
    fn the_card_waits_out_short_gaps_and_steps_aside_for_local_work() {
        let stopped = ServerState::Stopped { since: at(100) };
        assert!(
            !shows_card(&stopped, at(101), false, false, false),
            "inside the grace"
        );
        assert!(shows_card(&stopped, at(103), false, false, false));
        assert!(
            !shows_card(&stopped, at(103), true, false, false),
            "a live local shell"
        );
        assert!(
            !shows_card(&stopped, at(103), false, true, false),
            "dismissed"
        );
        assert!(
            !shows_card(&stopped, at(103), false, false, true),
            "the app is restarting it"
        );
        assert!(!shows_card(
            &ServerState::Running,
            at(103),
            false,
            false,
            false
        ));
        let failed = ServerState::Failed("no".into());
        assert!(
            shows_card(&failed, at(0), true, false, false),
            "a failed start shows anyway"
        );
        assert!(!shows_card(&failed, at(0), true, true, false));
        assert!(shows_card(
            &ServerState::Starting,
            at(0),
            false,
            false,
            false
        ));
    }

    #[test]
    fn enter_starts_only_when_the_card_offers_a_start() {
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(
            key(&ServerState::Stopped { since: at(0) }, &enter),
            CardKey::Act(CardAction::Start)
        );
        assert_eq!(
            key(&ServerState::Failed("x".into()), &enter),
            CardKey::Act(CardAction::Start)
        );
        assert_eq!(key(&ServerState::Starting, &enter), CardKey::Swallow);
        assert_eq!(
            key(
                &ServerState::Stopped { since: at(0) },
                &Key::Character("t".into())
            ),
            CardKey::Pass
        );
        assert_eq!(rows(&ServerState::Starting).1, vec!["Open a local shell"]);
        assert_eq!(
            rows(&ServerState::Failed("x".into())).1,
            vec!["Try again", "Open a local shell"]
        );
    }

    #[test]
    fn buttons_hit_only_inside_their_rects() {
        let buttons = [Rect::new(10, 10, 100, 30), Rect::new(120, 10, 100, 30)];
        assert_eq!(hit(&buttons, 50, 20), Some(CardAction::Start));
        assert_eq!(hit(&buttons, 150, 20), Some(CardAction::LocalShell));
        assert_eq!(hit(&buttons, 115, 20), None);
        let no_primary = [Rect::new(0, 0, 0, 0), Rect::new(120, 10, 100, 30)];
        assert_eq!(hit(&no_primary, 0, 0), None);
    }
}
