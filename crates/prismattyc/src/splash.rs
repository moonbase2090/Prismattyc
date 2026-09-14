//! Launch splash screen (Kiro-style): ASCII art, version, a rotating tip,
//! and a single-key menu. Shown only on a bare interactive `prism` launch
//! (no explicit PROGRAM, stdin/stdout are TTYs); `--no-splash` or
//! `PRISMATTYC_NO_SPLASH=1` opts out. Runs in the host terminal before the PTY
//! is spawned, so it never touches the child or the mux.

use std::env;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use prismattyc_core::splash::{
    art_frame, day_of_year, in_hold, tip_index, Topic, ART, ART_MARGIN_LEFT, FRAME_MS,
    HOLD_FRAME_MS, LINK, REPO_URL, TAGLINE, TIPS, WHATS_NEW,
};

/// What the interactive loop decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Proceed to spawn the child program.
    Launch,
    /// Exit before spawning anything.
    Quit,
}

/// Menu choice derived from one keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Launch,
    Quit,
    Show(Topic),
}

/// Map a keypress to a menu action. Unmapped keys return None (repaint).
/// Raw mode disables ISIG, so Ctrl+C / Ctrl+D arrive as plain key events
/// and must be mapped to Quit explicitly.
#[must_use]
pub fn key_action(key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        return Some(Action::Quit);
    }
    match key.code {
        KeyCode::Enter | KeyCode::Esc => Some(Action::Launch),
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('1') => Some(Action::Show(Topic::WhatsNew)),
        KeyCode::Char('2') => Some(Action::Show(Topic::Docs)),
        KeyCode::Char('3') => Some(Action::Show(Topic::Changelog)),
        KeyCode::Char('4') => Some(Action::Show(Topic::Walkthrough)),
        _ => None,
    }
}

/// 24-bit ANSI foreground, or "" when color is off (NO_COLOR / piped).
fn fg(r: u8, g: u8, b: u8, color: bool) -> String {
    if color {
        format!("\x1b[38;2;{r};{g};{b}m")
    } else {
        String::new()
    }
}

fn reset(color: bool) -> &'static str {
    if color {
        "\x1b[0m"
    } else {
        ""
    }
}

/// OSC 8 clickable hyperlink, tinted link-blue when color is on; plain
/// text otherwise (NO_COLOR / piped).
fn link(text: &str, url: &str, color: bool) -> String {
    if color {
        format!(
            "\x1b]8;;{url}\x1b\\{}{text}{}\x1b]8;;\x1b\\",
            fg(LINK[0], LINK[1], LINK[2], true),
            reset(true),
        )
    } else {
        text.to_string()
    }
}

/// Render the art at `elapsed_ms` into the attract-mode animation
/// (`prismattyc_core::splash::art_frame`): solid blocks in brand ink, outline
/// strokes banded across the spectrum, plus the beam sweep, reflection
/// passes, and lens flare. Without color the plain static art is returned.
fn render_art(color: bool, elapsed_ms: u64) -> String {
    let mut out = String::new();
    if !color {
        let pad = " ".repeat(ART_MARGIN_LEFT);
        for line in ART {
            out.push_str("  ");
            out.push_str(&pad);
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }
    for row in art_frame(elapsed_ms) {
        out.push_str("  ");
        let mut current: Option<[u8; 3]> = None;
        for (ch, rgb) in row {
            if ch == ' ' {
                out.push(' ');
                continue;
            }
            if current != Some(rgb) {
                out.push_str(&fg(rgb[0], rgb[1], rgb[2], true));
                current = Some(rgb);
            }
            out.push(ch);
        }
        out.push_str(reset(true));
        out.push('\n');
    }
    out
}

/// Number of terminal lines a page occupies (one per newline).
fn page_lines(page: &str) -> usize {
    page.matches('\n').count()
}

/// Repaint only the art rows of the main page in place. The cursor sits at
/// the end of the page; the art starts on line 1 (after the leading blank
/// line). Save/restore the cursor around the move so the caller's position
/// survives.
fn art_repaint(main_page_lines: usize, elapsed_ms: u64) -> String {
    let up = main_page_lines.saturating_sub(1);
    format!(
        "\x1b7\x1b[{up}A\r{}\x1b8",
        render_art(true, elapsed_ms).replace('\n', "\r\n")
    )
}

/// Frame period: full rate while something moves, slower during a hold
/// where only the flare twinkles.
fn frame_period(elapsed_ms: u64) -> Duration {
    Duration::from_millis(if in_hold(elapsed_ms) {
        HOLD_FRAME_MS
    } else {
        FRAME_MS
    })
}

/// The animation repaints by moving the cursor up to the art; that only
/// works when the whole page is on screen (no scrolling happened).
fn page_fits_terminal(main_page_lines: usize) -> bool {
    match crossterm::terminal::size() {
        Ok((_, rows)) => (rows as usize) > main_page_lines,
        Err(_) => false,
    }
}

/// Indent for text lines: two spaces plus the art's left margin, so text
/// aligns with the first letter of the word art.
fn indent() -> String {
    format!("  {}", " ".repeat(ART_MARGIN_LEFT))
}

/// The main splash page.
#[must_use]
pub fn render_main(version: &str, tip: usize, color: bool, elapsed_ms: u64) -> String {
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let accent = fg(80, 200, 255, color);
    let rst = reset(color);
    let ind = indent();
    let mut out = String::from("\n");
    out.push_str(&render_art(color, elapsed_ms));
    out.push_str(&format!(
        "\n{ind}{bold}Prismattyc{rst} {dim}v{version}{}{rst}  —  {TAGLINE}\n\n",
        prismattyc_core::git_suffix()
    ));
    out.push_str(&format!(
        "{ind}{accent}tip:{rst} {dim}{}{rst}\n\n",
        TIPS[tip % TIPS.len()]
    ));
    out.push_str(&format!("{ind}{bold}1.{rst}  What's new in v{version}\n"));
    out.push_str(&format!("{ind}{bold}2.{rst}  Docs & tips\n"));
    out.push_str(&format!("{ind}{bold}3.{rst}  Changelog\n"));
    out.push_str(&format!("{ind}{bold}4.{rst}  Walkthrough\n\n"));
    out.push_str(&format!(
        "{ind}{accent}⏎{rst}  Start session      {accent}q{rst}  Quit\n"
    ));
    out
}

/// A sub-page body.
#[must_use]
pub fn render_topic(topic: Topic, version: &str, color: bool) -> String {
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let rst = reset(color);
    let ind = indent();
    let mut out = String::from("\n");
    match topic {
        Topic::WhatsNew => {
            out.push_str(&format!("{ind}{bold}What's new in v{version}{rst}\n\n"));
            for item in WHATS_NEW {
                out.push_str(&format!("{ind}  • {item}\n"));
            }
        }
        Topic::Docs => {
            out.push_str(&format!("{ind}{bold}Docs & tips{rst}\n\n"));
            for tip in TIPS {
                out.push_str(&format!("{ind}  • {tip}\n"));
            }
            out.push_str(&format!(
                "\n{ind}  {dim}prismattyc --help · pmux --help · {rst}{}\n",
                link(REPO_URL, REPO_URL, color),
            ));
        }
        Topic::Changelog => {
            out.push_str(&format!("{ind}{bold}Changelog{rst}\n\n"));
            out.push_str(&format!(
                "{ind}  {}{dim} · {rst}{}\n",
                link(
                    &format!("{REPO_URL}/releases"),
                    &format!("{REPO_URL}/releases"),
                    color
                ),
                link(
                    &format!("{REPO_URL}/blob/main/CHANGELOG.md"),
                    &format!("{REPO_URL}/blob/main/CHANGELOG.md"),
                    color,
                ),
            ));
        }
        Topic::Walkthrough => {
            out.push_str(&format!("{ind}{bold}Walkthrough{rst}\n\n"));
            out.push_str(&format!(
                "{ind}  Level 0 opens in prismattyc-host. The pane stays usable.\n"
            ));
        }
    }
    out.push_str(&format!("\n{ind}{dim}any key to go back{rst}\n"));
    out
}

/// True when the splash should appear for this launch: bare `prism` (child
/// is the default shell), both std streams are TTYs, and no opt-out.
#[must_use]
pub fn should_show(explicit_program: bool, no_splash_flag: bool) -> bool {
    use std::io::IsTerminal;
    if explicit_program || no_splash_flag || env_flag_enabled("PRISMATTYC_NO_SPLASH") {
        return false;
    }
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn env_flag_enabled(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn read_key() -> Result<Option<KeyEvent>> {
    if let Event::Key(key) = event::read()? {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }
        return Ok(Some(key));
    }
    Ok(None)
}

/// Restore the cooked terminal on drop, so a panic mid-splash cannot leave
/// the host TTY in raw mode.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Raw mode maps `\n` to "down" without a carriage return; translate so
/// pages render at column 0.
fn write_page(stdout: &mut impl Write, page: &str) -> Result<()> {
    write!(stdout, "{}", page.replace('\n', "\r\n"))?;
    stdout.flush()?;
    Ok(())
}

/// Run the interactive splash. Caller has already checked [`should_show`].
///
/// With color on, the art animates (beam sweep, then the reflection loop)
/// between keypresses: the loop polls for input one frame at a time and
/// repaints the art rows in place. Any mapped key acts immediately.
pub fn run(version: &str) -> Result<Outcome> {
    let color = env::var_os("NO_COLOR").is_none();
    let tip = tip_index(day_of_year());
    let mut stdout = io::stdout();
    let _raw = RawModeGuard::enter()?;
    let started = Instant::now();
    let elapsed_ms = || started.elapsed().as_millis() as u64;
    let result = (|| -> Result<Outcome> {
        let page = render_main(version, tip, color, elapsed_ms());
        let lines = page_lines(&page);
        write_page(&mut stdout, &page)?;
        let animate = color && page_fits_terminal(lines);
        loop {
            let now = elapsed_ms();
            let wait = if animate {
                frame_period(now)
            } else {
                Duration::from_secs(60)
            };
            if !event::poll(wait)? {
                if animate {
                    write!(stdout, "{}", art_repaint(lines, elapsed_ms()))?;
                    stdout.flush()?;
                }
                continue;
            }
            let Some(key) = read_key()? else {
                continue;
            };
            match key_action(key) {
                Some(Action::Launch) => {
                    // Remove the splash from the primary screen before the
                    // child redraws it. Otherwise full-screen TUIs that use
                    // partial repainting can expose these old cells.
                    write!(stdout, "\x1b[2J\x1b[H\x1b[0m")?;
                    stdout.flush()?;
                    return Ok(Outcome::Launch);
                }
                Some(Action::Quit) => {
                    write!(stdout, "\x1b[2K\r")?;
                    return Ok(Outcome::Quit);
                }
                Some(Action::Show(topic)) => {
                    write_page(&mut stdout, &render_topic(topic, version, color))?;
                    // Sub-page: any key returns to the main page; Quit exits.
                    loop {
                        match read_key()? {
                            None => {}
                            Some(key) => {
                                if key_action(key) == Some(Action::Quit) {
                                    return Ok(Outcome::Quit);
                                }
                                break;
                            }
                        }
                    }
                    write_page(&mut stdout, &render_main(version, tip, color, elapsed_ms()))?;
                }
                None => {}
            }
        }
    })();
    // Leave the screen tidy for the child program about to start.
    write!(stdout, "\r\n")?;
    stdout.flush()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::splash::{FLARE_GLYPH, FLARE_PEAK_GLYPH, INK, SETTLED_MS, SPECTRUM};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn key_action_maps_menu_keys() {
        assert_eq!(key_action(key(KeyCode::Enter)), Some(Action::Launch));
        assert_eq!(key_action(key(KeyCode::Esc)), Some(Action::Launch));
        assert_eq!(key_action(key(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(
            key_action(key(KeyCode::Char('1'))),
            Some(Action::Show(Topic::WhatsNew))
        );
        assert_eq!(
            key_action(key(KeyCode::Char('2'))),
            Some(Action::Show(Topic::Docs))
        );
        assert_eq!(
            key_action(key(KeyCode::Char('3'))),
            Some(Action::Show(Topic::Changelog))
        );
        assert_eq!(
            key_action(key(KeyCode::Char('4'))),
            Some(Action::Show(Topic::Walkthrough))
        );
        assert_eq!(key_action(key(KeyCode::Char('x'))), None);
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit_but_plain_letters_do_not() {
        for ch in ['c', 'd'] {
            let event = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            assert_eq!(key_action(event), Some(Action::Quit));
            assert_eq!(key_action(key(KeyCode::Char(ch))), None);
        }
    }

    #[test]
    fn main_page_has_art_version_tip_and_menu() {
        let page = render_main("1.2.3", 0, false, SETTLED_MS);
        assert!(page.contains("██████╗"));
        assert!(page.contains("v1.2.3"));
        assert!(page.contains(&format!("v1.2.3{}", prismattyc_core::git_suffix())));
        assert!(page.contains(TIPS[0]));
        assert!(page.contains("What's new in v1.2.3"));
        assert!(page.contains("Docs & tips"));
        assert!(page.contains("Changelog"));
        assert!(page.contains("Walkthrough"));
        assert!(page.contains("Start session"));
        assert!(page.contains("Quit"));
    }

    #[test]
    fn colorless_output_has_no_ansi() {
        let page = render_main("0.0.0", 1, false, 0);
        assert!(!page.contains("\x1b["));
        let topic = render_topic(Topic::WhatsNew, "0.0.0", false);
        assert!(!topic.contains("\x1b["));
    }

    #[test]
    fn colored_output_has_gradient_and_reset() {
        let page = render_main("0.0.0", 0, true, SETTLED_MS);
        assert!(page.contains("\x1b[38;2;"));
        assert!(page.contains("\x1b[0m"));
    }

    #[test]
    fn art_uses_brand_spectrum_and_ink() {
        let art = render_art(true, SETTLED_MS);
        // Ink fill on the solid blocks.
        assert!(art.contains(&fg(INK[0], INK[1], INK[2], true)));
        // Every spectrum swatch appears as an outline color.
        for rgb in SPECTRUM {
            assert!(
                art.contains(&fg(rgb[0], rgb[1], rgb[2], true)),
                "missing spectrum color {rgb:?}"
            );
        }
    }

    #[test]
    fn art_animates_and_repaints_in_place() {
        // Frame 0 is dark: no strokes yet, no colour codes.
        let dark = render_art(true, 0);
        assert!(!dark.contains('█'));
        // Settled frame carries the flare on the top row.
        let settled = render_art(true, SETTLED_MS);
        let top = settled.lines().next().unwrap();
        assert!(top.contains(FLARE_GLYPH) || top.contains(FLARE_PEAK_GLYPH));
        // Colorless art is static and plain.
        assert_eq!(render_art(false, 0), render_art(false, SETTLED_MS));
        assert!(!render_art(false, 0).contains("\x1b["));
        // Repaint moves up to line 1, paints, and restores the cursor.
        let page = render_main("1.0", 0, true, 0);
        let lines = page_lines(&page);
        let repaint = art_repaint(lines, SETTLED_MS);
        assert!(repaint.starts_with(&format!("\x1b7\x1b[{}A\r", lines - 1)));
        assert!(repaint.ends_with("\x1b8"));
        assert_eq!(repaint.matches("\r\n").count(), ART.len());
    }

    #[test]
    fn topics_render_content() {
        assert!(render_topic(Topic::WhatsNew, "9.9", false).contains("What's new in v9.9"));
        assert!(render_topic(Topic::Docs, "9.9", false).contains(REPO_URL));
        assert!(render_topic(Topic::Changelog, "9.9", false).contains("releases"));
    }

    #[test]
    fn colored_topics_render_clickable_link_blue_urls() {
        let docs = render_topic(Topic::Docs, "9.9", true);
        assert!(docs.contains(&format!("\x1b]8;;{REPO_URL}\x1b\\")));
        assert!(docs.contains(&fg(LINK[0], LINK[1], LINK[2], true)));
        // Colorless output stays plain text (and OSC 8-free).
        let plain = render_topic(Topic::Changelog, "9.9", false);
        assert!(!plain.contains("\x1b]8;"));
        assert!(!plain.contains("\x1b["));
    }

    #[test]
    fn splash_suppressed_for_explicit_program_or_flag() {
        assert!(!should_show(true, false));
        assert!(!should_show(true, true));
        // (TTY-dependent cases are covered by the flag/env short-circuits.)
    }
}
