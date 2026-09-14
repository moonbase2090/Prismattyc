//! In-window launch splash for bare `prismattyc-host` launches — the same
//! content as the `prismattyc` CLI splash (shared via `prismattyc_core::splash`),
//! rasterized into the window instead of printed to a terminal. Shown on
//! the first window only; `--no-splash`, `PRISMATTYC_NO_SPLASH=1`, and config
//! `splash = false` opt out. Attach launches skip it (attaching drops you
//! into live sessions, the same way an explicit PROGRAM skips the CLI splash).

use std::time::{Duration, Instant};

use prismattyc_core::git_suffix;
use prismattyc_core::splash::ART;
use prismattyc_core::splash::{
    art_frame, art_frame_width, art_static, day_of_year, in_hold, tip_index, Topic,
    ART_MARGIN_LEFT, FRAME_MS, HOLD_FRAME_MS, INK, LINK, REPO_URL, TAGLINE, TIPS, WHATS_NEW,
};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Which page the splash is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Topic(Topic),
}

/// What a keypress decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Dismiss the splash and start using the session.
    Dismiss,
    /// Exit prismattyc-host before the session is used.
    Quit,
    /// Open a sub-page.
    Show(Topic),
    /// Return from a sub-page to the main page.
    Back,
}

/// Launch-splash overlay state.
#[derive(Debug, Clone, Copy)]
pub struct Splash {
    pub page: Page,
    pub tip: usize,
    /// Animation clock origin: the art plays from the moment the splash
    /// appeared (`prismattyc_core::splash::art_frame`).
    pub shown_at: Instant,
    /// Elapsed ms at the last frame that marked the window dirty. Frames
    /// are quantized so the animation costs a bounded number of repaints.
    pub last_tick_ms: u64,
    /// Config `splash_animation` (default true). Off: static art, no timer.
    pub animated: bool,
    /// True when walkthrough.json exists. Set when the splash is shown.
    pub resume: bool,
}

impl Splash {
    #[must_use]
    pub fn new(animated: bool) -> Self {
        Self {
            page: Page::Main,
            tip: tip_index(day_of_year()),
            shown_at: Instant::now(),
            last_tick_ms: 0,
            animated,
            resume: false,
        }
    }

    /// Milliseconds since the splash appeared.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.shown_at.elapsed().as_millis() as u64
    }

    /// The animation clock for the current frame: `Some(elapsed)` on an
    /// animated main page, `None` when the art is static or a topic page
    /// is up (the rasterizer then draws plain glyphs, no rays).
    #[must_use]
    pub fn frame_clock(&self) -> Option<u64> {
        (self.animated && self.page == Page::Main).then(|| self.elapsed_ms())
    }

    /// Advance the frame clock. True when a new frame is due and the window
    /// should repaint. Only an animated main page ticks (topic pages have
    /// no art; a static splash never needs a timer).
    pub fn tick(&mut self) -> bool {
        if !self.animated || self.page != Page::Main {
            return false;
        }
        let now = self.elapsed_ms();
        if now.saturating_sub(self.last_tick_ms) >= frame_period_ms(self.last_tick_ms) {
            self.last_tick_ms = now;
            return true;
        }
        false
    }

    /// When the next frame is due, for the event loop's `WaitUntil`. None
    /// on topic pages or a static splash (nothing moves).
    #[must_use]
    pub fn next_frame_at(&self) -> Option<Instant> {
        if !self.animated || self.page != Page::Main {
            return None;
        }
        let next = self.last_tick_ms + frame_period_ms(self.last_tick_ms);
        Some(self.shown_at + Duration::from_millis(next))
    }
}

/// Columns of breathing room on each side of the art frame.
const WINDOW_SIDE_PAD_COLS: usize = 6;
/// Rows of breathing room above and below the main page.
const WINDOW_VERTICAL_PAD_ROWS: usize = 3;

/// Initial window size, in cells, for a launch that shows the splash: at
/// least `base`, and wide/tall enough for the art frame and the main page
/// with padding.
#[must_use]
pub fn window_cells(base_cols: usize, base_rows: usize) -> (usize, usize) {
    let cols = art_frame_width() + 2 * WINDOW_SIDE_PAD_COLS;
    let rows = layout(Page::Main, "0.0.0", 0, None).len() + 2 * WINDOW_VERTICAL_PAD_ROWS;
    (cols.max(base_cols), rows.max(base_rows))
}

/// Frame period: full rate while the beam or a reflection moves, slower
/// during a hold where only the flare twinkles.
#[must_use]
pub fn frame_period_ms(elapsed_ms: u64) -> u64 {
    if in_hold(elapsed_ms) {
        HOLD_FRAME_MS
    } else {
        FRAME_MS
    }
}

impl Default for Splash {
    fn default() -> Self {
        Self::new(true)
    }
}

/// True when the splash should appear on the first window.
///
/// CLI `--no-splash` and `PRISMATTYC_NO_SPLASH` hide the splash even when
/// config `splash` is true. Config `splash = false` hides it when CLI and
/// env do not suppress. An explicit PROGRAM or `--attach-session` always
/// skips the splash.
#[must_use]
pub fn should_show(
    no_splash_flag: bool,
    explicit_target: bool,
    env_no_splash: bool,
    config_splash: bool,
) -> bool {
    !no_splash_flag && !explicit_target && !env_no_splash && config_splash
}

/// Map a keypress given the current page. Mirrors the CLI keymap: on the
/// main page Enter/Esc dismiss, q quits, 1/2/3 open topics, Ctrl+C/Ctrl+D
/// quit; on a topic page any key goes back and the quit keys still quit.
/// Unmapped main-page keys return None (ignored, not forwarded).
#[must_use]
pub fn key_action(page: Page, key: &Key, modifiers: ModifiersState) -> Option<Action> {
    if modifiers.control_key() {
        if let Key::Character(s) = key {
            if s.eq_ignore_ascii_case("c") || s.eq_ignore_ascii_case("d") {
                return Some(Action::Quit);
            }
        }
    }
    match page {
        Page::Topic(_) => Some(match key {
            Key::Character(s) if s.eq_ignore_ascii_case("q") => Action::Quit,
            _ => Action::Back,
        }),
        Page::Main => match key {
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape) => Some(Action::Dismiss),
            Key::Character(s) => match s.as_str() {
                "q" | "Q" => Some(Action::Quit),
                "1" => Some(Action::Show(Topic::WhatsNew)),
                "2" => Some(Action::Show(Topic::Docs)),
                "3" => Some(Action::Show(Topic::Changelog)),
                "4" => Some(Action::Show(Topic::Walkthrough)),
                _ => None,
            },
            _ => None,
        },
    }
}

/// One styled run of text on a splash line.
pub type Span = (String, [u8; 3]);

const DIM: [u8; 3] = [0x80, 0x80, 0x80];
const ACCENT: [u8; 3] = [0x50, 0xc8, 0xff];

/// Build the visible lines for a page as styled spans. Art rows come from
/// the attract-mode animation at `Some(elapsed_ms)`, or the static art for
/// `None`, and expand to per-glyph spans so the rasterizer can paint each
/// cell's colour; empty lines are empty vecs. Pure, so tests pin the content.
#[must_use]
pub fn layout(page: Page, version: &str, tip: usize, elapsed_ms: Option<u64>) -> Vec<Vec<Span>> {
    layout_with_resume(page, version, tip, elapsed_ms, false)
}

/// Same as [`layout`], with splash item 4 as Resume when progress exists.
#[must_use]
pub fn layout_with_resume(
    page: Page,
    version: &str,
    tip: usize,
    elapsed_ms: Option<u64>,
    resume: bool,
) -> Vec<Vec<Span>> {
    let mut lines: Vec<Vec<Span>> = Vec::new();
    match page {
        Page::Main => {
            let art = match elapsed_ms {
                Some(ms) => art_frame(ms),
                None => art_static(),
            };
            for row in art {
                // Spaces carry INK and are never painted; they only advance.
                lines.push(
                    row.into_iter()
                        .map(|(ch, rgb)| (ch.to_string(), rgb))
                        .collect(),
                );
            }
            lines.push(vec![]);
            lines.push(vec![
                ("Prismattyc ".into(), INK),
                (format!("v{version}{}", git_suffix()), DIM),
                ("  —  ".into(), DIM),
                (TAGLINE.into(), INK),
            ]);
            lines.push(vec![]);
            lines.push(vec![
                ("tip: ".into(), ACCENT),
                (TIPS[tip % TIPS.len()].into(), DIM),
            ]);
            lines.push(vec![]);
            lines.push(vec![
                ("1.  ".into(), ACCENT),
                (format!("What's new in v{version}"), INK),
            ]);
            lines.push(vec![("2.  ".into(), ACCENT), ("Docs & tips".into(), INK)]);
            lines.push(vec![("3.  ".into(), ACCENT), ("Changelog".into(), INK)]);
            let topic4 = if resume {
                "Resume walkthrough"
            } else {
                "Walkthrough"
            };
            lines.push(vec![("4.  ".into(), ACCENT), (topic4.into(), INK)]);
            lines.push(vec![]);
            lines.push(vec![
                ("⏎".into(), ACCENT),
                ("  Start session      ".into(), INK),
                ("q".into(), ACCENT),
                ("  Quit".into(), INK),
            ]);
        }
        Page::Topic(Topic::WhatsNew) => {
            lines.push(vec![(format!("What's new in v{version}"), INK)]);
            lines.push(vec![]);
            for item in WHATS_NEW {
                lines.push(vec![("• ".into(), ACCENT), ((*item).into(), INK)]);
            }
        }
        Page::Topic(Topic::Docs) => {
            lines.push(vec![("Docs & tips".into(), INK)]);
            lines.push(vec![]);
            for tip in TIPS {
                lines.push(vec![("• ".into(), ACCENT), ((*tip).into(), INK)]);
            }
            lines.push(vec![]);
            lines.push(vec![
                ("prismattyc --help · pmux --help · ".into(), DIM),
                (REPO_URL.into(), LINK),
            ]);
        }
        Page::Topic(Topic::Changelog) => {
            lines.push(vec![("Changelog".into(), INK)]);
            lines.push(vec![]);
            lines.push(vec![
                (format!("{REPO_URL}/releases"), LINK),
                (" · ".into(), DIM),
                (format!("{REPO_URL}/blob/main/CHANGELOG.md"), LINK),
            ]);
        }
        Page::Topic(Topic::Walkthrough) => {
            lines.push(vec![("Walkthrough".into(), INK)]);
            lines.push(vec![]);
            lines.push(vec![(
                "Level 0 opens in the windowed host. The pane stays usable.".into(),
                INK,
            )]);
        }
    }
    if let Page::Topic(_) = page {
        lines.push(vec![]);
        lines.push(vec![("any key to go back".into(), DIM)]);
    }
    // The art rows carry a left margin for the top-left flare; indent every
    // other line by the same amount so text stays aligned with the first
    // letter (and topic pages match the main page).
    let art_rows = if page == Page::Main { ART.len() } else { 0 };
    let pad = " ".repeat(ART_MARGIN_LEFT);
    for line in lines.iter_mut().skip(art_rows) {
        if !line.is_empty() {
            line.insert(0, (pad.clone(), INK));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::splash::{FLARE_GLYPH, FLARE_PEAK_GLYPH, SETTLED_MS, SPECTRUM};

    fn char_key(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn main_page_keymap_matches_cli() {
        let no_mods = ModifiersState::empty();
        assert_eq!(
            key_action(Page::Main, &Key::Named(NamedKey::Enter), no_mods),
            Some(Action::Dismiss)
        );
        assert_eq!(
            key_action(Page::Main, &Key::Named(NamedKey::Escape), no_mods),
            Some(Action::Dismiss)
        );
        assert_eq!(
            key_action(Page::Main, &char_key("q"), no_mods),
            Some(Action::Quit)
        );
        assert_eq!(
            key_action(Page::Main, &char_key("1"), no_mods),
            Some(Action::Show(Topic::WhatsNew))
        );
        assert_eq!(
            key_action(Page::Main, &char_key("2"), no_mods),
            Some(Action::Show(Topic::Docs))
        );
        assert_eq!(
            key_action(Page::Main, &char_key("3"), no_mods),
            Some(Action::Show(Topic::Changelog))
        );
        assert_eq!(
            key_action(Page::Main, &char_key("4"), no_mods),
            Some(Action::Show(Topic::Walkthrough))
        );
        assert_eq!(key_action(Page::Main, &char_key("x"), no_mods), None);
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit() {
        let ctrl = ModifiersState::CONTROL;
        for key in ["c", "d"] {
            assert_eq!(
                key_action(Page::Main, &char_key(key), ctrl),
                Some(Action::Quit)
            );
            assert_eq!(
                key_action(Page::Topic(Topic::Docs), &char_key(key), ctrl),
                Some(Action::Quit)
            );
        }
        // Plain c/d are not quit.
        assert_eq!(
            key_action(Page::Main, &char_key("c"), ModifiersState::empty()),
            None
        );
    }

    #[test]
    fn topic_page_any_key_backs_out_and_q_quits() {
        let no_mods = ModifiersState::empty();
        let page = Page::Topic(Topic::WhatsNew);
        assert_eq!(
            key_action(page, &char_key("x"), no_mods),
            Some(Action::Back)
        );
        assert_eq!(
            key_action(page, &Key::Named(NamedKey::Enter), no_mods),
            Some(Action::Back)
        );
        assert_eq!(
            key_action(page, &char_key("q"), no_mods),
            Some(Action::Quit)
        );
    }

    #[test]
    fn suppression_matches_cli_env_and_config() {
        // Default: show on a bare launch.
        assert!(should_show(false, false, false, true));
        // CLI `--no-splash` hides even when config splash is on.
        assert!(!should_show(true, false, false, true));
        // Explicit PROGRAM / attach skips the splash.
        assert!(!should_show(false, true, false, true));
        // `PRISMATTYC_NO_SPLASH` hides even when config splash is on.
        assert!(!should_show(false, false, true, true));
        // Config `splash = false` hides when CLI and env do not suppress.
        assert!(!should_show(false, false, false, false));
        // Config on does not override CLI or env suppressors.
        assert!(!should_show(true, false, false, true));
        assert!(!should_show(false, false, true, true));
    }

    #[test]
    fn main_layout_has_art_version_tip_and_menu() {
        let lines = layout(Page::Main, "1.2.3", 0, Some(SETTLED_MS));
        let text: String = lines
            .iter()
            .flat_map(|line| line.iter().map(|(t, _)| t.as_str()))
            .collect();
        assert!(text.contains("██████╗"));
        assert!(text.contains("v1.2.3"));
        assert!(
            text.contains(&format!("v1.2.3{}", prismattyc_core::git_suffix())),
            "header must include git stamp when baked: {text:?}"
        );
        assert!(text.contains(TIPS[0]));
        assert!(text.contains("What's new in v1.2.3"));
        assert!(text.contains("Docs & tips"));
        assert!(text.contains("Changelog"));
        assert!(text.contains("Walkthrough"));
        assert!(!text.contains("Resume walkthrough"));
        let resume = layout_with_resume(Page::Main, "1.2.3", 0, Some(SETTLED_MS), true);
        let resume_text: String = resume
            .iter()
            .flat_map(|line| line.iter().map(|(t, _)| t.as_str()))
            .collect();
        assert!(resume_text.contains("Resume walkthrough"));
        assert!(text.contains("Start session"));
        assert!(text.contains("Quit"));
        // Art spans carry the brand palette: ink fill, spectrum outlines.
        let art = &lines[0];
        assert!(art.iter().any(|(t, rgb)| t == "█" && *rgb == INK));
        for swatch in SPECTRUM {
            assert!(
                lines[..ART.len()]
                    .iter()
                    .flatten()
                    .any(|(_, rgb)| rgb == swatch),
                "missing spectrum color {swatch:?}"
            );
        }
    }

    #[test]
    fn art_animates_with_time() {
        // Frame 0 is dark; the settled frame shows the flare on the top row.
        let dark = layout(Page::Main, "1.0", 0, Some(0));
        assert!(dark[..ART.len()].iter().flatten().all(|(t, _)| t == " "));
        let settled = layout(Page::Main, "1.0", 0, Some(SETTLED_MS));
        assert!(settled[0]
            .iter()
            .any(|(t, _)| { t == &FLARE_GLYPH.to_string() || t == &FLARE_PEAK_GLYPH.to_string() }));
    }

    #[test]
    fn text_lines_align_with_the_first_letter() {
        let lines = layout(Page::Main, "1.0", 0, Some(SETTLED_MS));
        let pad = " ".repeat(ART_MARGIN_LEFT);
        for line in &lines[ART.len()..] {
            if !line.is_empty() {
                assert_eq!(line[0].0, pad);
            }
        }
        let topic = layout(Page::Topic(Topic::Docs), "1.0", 0, None);
        assert_eq!(topic[0][0].0, pad);
    }

    #[test]
    fn splash_window_fits_the_art_frame() {
        let (cols, rows) = window_cells(80, 24);
        assert!(cols >= art_frame_width() + 2 * WINDOW_SIDE_PAD_COLS);
        assert!(rows >= 24);
        // A larger base wins.
        assert_eq!(window_cells(300, 90), (300, 90));
    }

    #[test]
    fn static_splash_never_ticks_and_lays_out_plain_art() {
        let mut splash = Splash::new(false);
        assert!(!splash.tick());
        assert!(splash.next_frame_at().is_none());
        assert!(splash.frame_clock().is_none());
        let lines = layout(Page::Main, "1.0", 0, splash.frame_clock());
        let text: String = lines[0].iter().map(|(t, _)| t.as_str()).collect();
        assert!(text.contains("██████╗"));
        assert!(!text.contains(FLARE_GLYPH) && !text.contains(FLARE_PEAK_GLYPH));
        // Same width as an animated frame, so text below stays aligned.
        assert_eq!(
            lines[0].len(),
            layout(Page::Main, "1.0", 0, Some(0))[0].len()
        );
        // An animated splash reports a clock on the main page only.
        let mut live = Splash::new(true);
        assert!(live.frame_clock().is_some());
        live.page = Page::Topic(Topic::Docs);
        assert!(live.frame_clock().is_none());
    }

    #[test]
    fn tick_quantizes_frames_and_only_on_main_page() {
        let mut splash = Splash::new(true);
        splash.last_tick_ms = 0;
        // Immediately after a tick nothing is due; the deadline is one
        // frame out.
        let next = splash.next_frame_at().expect("main page animates");
        assert!(next > splash.shown_at);
        assert!(next <= splash.shown_at + Duration::from_millis(frame_period_ms(0)));
        splash.page = Page::Topic(Topic::Docs);
        assert!(!splash.tick());
        assert!(splash.next_frame_at().is_none());
        // Hold frames are slower than motion frames.
        assert!(frame_period_ms(SETTLED_MS) > frame_period_ms(0));
    }

    #[test]
    fn topic_layouts_render_content() {
        let whats_new = layout(Page::Topic(Topic::WhatsNew), "9.9", 0, None);
        let text: String = whats_new
            .iter()
            .flat_map(|line| line.iter().map(|(t, _)| t.as_str()))
            .collect();
        assert!(text.contains("What's new in v9.9"));
        assert!(text.contains("any key to go back"));

        let docs = layout(Page::Topic(Topic::Docs), "9.9", 0, None);
        let text: String = docs
            .iter()
            .flat_map(|line| line.iter().map(|(t, _)| t.as_str()))
            .collect();
        assert!(text.contains(REPO_URL));
        // URLs are link-blue, not dim.
        assert!(
            docs.iter()
                .flatten()
                .any(|(t, rgb)| t.contains(REPO_URL) && *rgb == LINK),
            "repo URL not link-colored"
        );

        let changelog = layout(Page::Topic(Topic::Changelog), "9.9", 0, None);
        let text: String = changelog
            .iter()
            .flat_map(|line| line.iter().map(|(t, _)| t.as_str()))
            .collect();
        assert!(text.contains("releases"));
    }
}
