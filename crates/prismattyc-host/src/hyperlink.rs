//! Resolve explicit OSC 8 hyperlinks and auto-detected HTTP(S) runs.
//!
//! OSC 8 wins when a cell carries an explicit target. Unsafe explicit targets
//! remain non-clickable and never fall through to displayed-text detection.

use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;

use prismattyc_core::{Screen, MAX_HYPERLINK_URI_BYTES};
use winit::keyboard::ModifiersState;

/// Trailing characters stripped from a detected run (spike SGR-safe set).
const TRAILING_PUNCT: &[char] = &[',', '.', ';', ':', '!', '?', ')', ']', '`'];

/// Cap a single detected URL so a full-viewport wrap cannot grow without bound.
const MAX_URL_CHARS: usize = MAX_HYPERLINK_URI_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedUrl {
    pub url: String,
    /// Inclusive viewport cell of the first URL character.
    pub start: (usize, usize),
    /// Inclusive viewport cell of the last URL character (after punct strip).
    pub end: (usize, usize),
}

impl DetectedUrl {
    fn contains(&self, columns: usize, row: usize, col: usize) -> bool {
        if columns == 0 {
            return false;
        }
        let idx = |r: usize, c: usize| r.saturating_mul(columns).saturating_add(c);
        let at = idx(row, col);
        at >= idx(self.start.0, self.start.1) && at <= idx(self.end.0, self.end.1)
    }
}

/// Linux: Ctrl (no Shift). macOS: Cmd/Super (no Shift). Shift stays host select.
pub fn is_open_url_click(modifiers: ModifiersState) -> bool {
    if modifiers.shift_key() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        modifiers.super_key() && !modifiers.control_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        modifiers.control_key() && !modifiers.super_key()
    }
}

/// Hit + open-modifier owns the gesture. Otherwise ADR-0001 / ADR-0003 apply.
pub fn click_owns_url(open_gesture: bool, hit: bool) -> bool {
    open_gesture && hit
}

/// URL covering viewport cell `(row, col)` under `scroll`, if any.
pub fn url_at(screen: &Screen, scroll: usize, row: usize, col: usize) -> Option<String> {
    let scroll = scroll.min(screen.max_view_scroll());
    if let Some(uri) = screen.hyperlink_uri_at_view(scroll, row, col) {
        return is_allowed_http_url(uri).then(|| uri.to_owned());
    }
    let columns = screen.columns();
    detect_urls(screen, scroll)
        .into_iter()
        .find(|found| found.contains(columns, row, col))
        .map(|found| found.url)
}

pub fn detect_urls(screen: &Screen, scroll: usize) -> Vec<DetectedUrl> {
    let rows = screen.rows();
    let columns = screen.columns();
    if rows == 0 || columns == 0 {
        return Vec::new();
    }
    let scroll = scroll.min(screen.max_view_scroll());
    let mut chars = Vec::with_capacity(rows.saturating_mul(columns));
    let mut pos = Vec::with_capacity(rows.saturating_mul(columns));
    for row in 0..rows {
        for col in 0..columns {
            let cell = screen.view_cell(scroll, row, col);
            chars.push(cell.character);
            pos.push((row, col));
        }
    }

    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let Some(scheme_len) = scheme_prefix_len(&chars[i..]) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme_len;
        while end < chars.len() && end - i < MAX_URL_CHARS && is_url_char(chars[end]) {
            end += 1;
        }
        let mut trimmed = end;
        while trimmed > i + scheme_len && TRAILING_PUNCT.contains(&chars[trimmed - 1]) {
            trimmed -= 1;
        }
        if trimmed <= i + scheme_len {
            i += 1;
            continue;
        }
        let url: String = chars[i..trimmed].iter().collect();
        if is_allowed_http_url(&url) {
            found.push(DetectedUrl {
                url,
                start: pos[i],
                end: pos[trimmed - 1],
            });
            i = trimmed;
        } else {
            i += 1;
        }
    }
    found
}

fn scheme_prefix_len(chars: &[char]) -> Option<usize> {
    if starts_ignore_ascii_case(chars, &['h', 't', 't', 'p', 's', ':', '/', '/']) {
        Some(8)
    } else if starts_ignore_ascii_case(chars, &['h', 't', 't', 'p', ':', '/', '/']) {
        Some(7)
    } else {
        None
    }
}

fn starts_ignore_ascii_case(chars: &[char], prefix: &[char]) -> bool {
    if chars.len() < prefix.len() {
        return false;
    }
    chars
        .iter()
        .zip(prefix)
        .all(|(got, want)| got.eq_ignore_ascii_case(want))
}

fn is_url_char(ch: char) -> bool {
    matches!(
        ch,
        'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '-'
            | '.'
            | '_'
            | '~'
            | ':'
            | '/'
            | '?'
            | '#'
            | '['
            | ']'
            | '@'
            | '!'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | ';'
            | '='
            | '%'
    )
}

/// Allow `http` and `https` only. Reject `javascript:`, `file:`, `data:`.
pub fn is_allowed_http_url(url: &str) -> bool {
    if url.len() > MAX_URL_CHARS || url.is_empty() || url.contains('\0') {
        return false;
    }
    if url.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let rest = if url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://") {
        &url[8..]
    } else if url.len() >= 7 && url[..7].eq_ignore_ascii_case("http://") {
        &url[7..]
    } else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b > b' ' && b < 0x7f)
}

/// Program + single URL argument. Never `sh -c`.
pub fn open_argv(url: &str) -> Option<(&'static str, String)> {
    if !is_allowed_http_url(url) {
        return None;
    }
    Some((open_program(), url.to_string()))
}

pub fn open_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Spawn the platform opener detached. Failures are silent to the caller (`false`).
pub fn spawn_open(url: &str) -> bool {
    let Some((program, arg)) = open_argv(url) else {
        return false;
    };
    match Command::new(program)
        .arg(&arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// BEL on the host process (stderr). Windowed visual bell is not claimed.
pub fn ring_host_bell() {
    let mut out = std::io::stderr();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::Screen;
    use prismattyc_emulator::Emulator;

    fn fill(screen: &mut Screen, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                screen.line_feed();
                screen.carriage_return();
            } else {
                screen.put_char(ch);
            }
        }
    }

    fn mods(ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
        let mut value = ModifiersState::empty();
        value.set(ModifiersState::CONTROL, ctrl);
        value.set(ModifiersState::SHIFT, shift);
        value.set(ModifiersState::SUPER, logo);
        value
    }

    #[test]
    fn detects_plain_https_and_hits_cells() {
        let mut screen = Screen::new(40, 1, 0);
        fill(&mut screen, "see https://example.com now");
        let url = "https://example.com";
        let start = 4usize;
        for col in start..start + url.len() {
            assert_eq!(
                url_at(&screen, 0, 0, col).as_deref(),
                Some(url),
                "col {col}"
            );
        }
        assert_eq!(url_at(&screen, 0, 0, 0), None, "text before URL is a miss");
        assert_eq!(
            url_at(&screen, 0, 0, start + url.len()),
            None,
            "space after URL is a miss"
        );
    }

    #[test]
    fn explicit_osc8_target_opens_from_non_url_label() {
        let mut emulator = Emulator::new(20, 1, 0);
        let _ = emulator.feed(b"\x1b]8;id=docs;https://example.com/manual\x1b\\docs\x1b]8;;\x1b\\");
        for col in 0..4 {
            assert_eq!(
                url_at(emulator.screen(), 0, 0, col).as_deref(),
                Some("https://example.com/manual")
            );
        }
        assert_eq!(url_at(emulator.screen(), 0, 0, 4), None);
    }

    #[test]
    fn unsafe_explicit_target_does_not_fall_through_to_visible_text() {
        let mut emulator = Emulator::new(32, 1, 0);
        let _ =
            emulator.feed(b"\x1b]8;;javascript:alert(1)\x1b\\https://safe.example\x1b]8;;\x1b\\");
        assert_eq!(url_at(emulator.screen(), 0, 0, 0), None);
    }

    #[test]
    fn wrap_across_row_edge_is_one_url() {
        let mut screen = Screen::new(20, 2, 0);
        fill(&mut screen, "https://example.com/x");
        assert_eq!(screen.view_cell(0, 0, 19).character, '/');
        assert_eq!(screen.view_cell(0, 1, 0).character, 'x');
        let url = "https://example.com/x";
        assert_eq!(url_at(&screen, 0, 0, 0).as_deref(), Some(url));
        assert_eq!(url_at(&screen, 0, 0, 19).as_deref(), Some(url));
        assert_eq!(url_at(&screen, 0, 1, 0).as_deref(), Some(url));
        assert_eq!(url_at(&screen, 0, 1, 1), None);
    }

    #[test]
    fn newline_does_not_join_the_next_row() {
        let mut screen = Screen::new(40, 2, 0);
        fill(&mut screen, "https://example.com\n/not-joined");
        assert_eq!(
            url_at(&screen, 0, 0, 0).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(url_at(&screen, 0, 1, 0), None);
    }

    #[test]
    fn strips_trailing_sgr_safe_punctuation() {
        let mut screen = Screen::new(48, 1, 0);
        fill(&mut screen, "https://example.com.,;:!?)`] ");
        let url = "https://example.com";
        assert_eq!(url_at(&screen, 0, 0, 0).as_deref(), Some(url));
        assert_eq!(
            url_at(&screen, 0, 0, url.len() - 1).as_deref(),
            Some(url),
            "last URL cell is a hit"
        );
        for col in url.len()..url.len() + TRAILING_PUNCT.len() {
            assert_eq!(
                url_at(&screen, 0, 0, col),
                None,
                "trailing punct col {col} is a miss"
            );
        }
    }

    #[test]
    fn rejects_javascript_file_and_data_schemes() {
        assert!(!is_allowed_http_url("javascript:alert(1)"));
        assert!(!is_allowed_http_url("file:///etc/passwd"));
        assert!(!is_allowed_http_url("data:text/html,hi"));
        assert!(open_argv("javascript:alert(1)").is_none());
        assert!(open_argv("file:///tmp").is_none());
        assert!(open_argv("data:text/html,hi").is_none());

        let mut screen = Screen::new(40, 1, 0);
        fill(&mut screen, "javascript:alert(1) file://x data:y");
        assert!(detect_urls(&screen, 0).is_empty());
        assert_eq!(url_at(&screen, 0, 0, 0), None);
    }

    #[test]
    fn allows_http_and_https_only() {
        assert!(is_allowed_http_url("https://example.com"));
        assert!(is_allowed_http_url("http://example.com/a"));
        assert!(is_allowed_http_url("HTTPS://Example.COM"));
        assert!(!is_allowed_http_url("https://"));
        assert!(!is_allowed_http_url("http://"));
        assert!(!is_allowed_http_url("ftp://example.com"));
        let (program, arg) =
            open_argv("https://example.com/some/app/installations/new").expect("allowlisted");
        assert_eq!(program, open_program());
        assert!(!program.contains(' '));
        assert_ne!(program, "sh");
        assert_eq!(arg, "https://example.com/some/app/installations/new");
    }

    #[test]
    fn hit_vs_miss_vs_selection() {
        let mut screen = Screen::new(32, 1, 0);
        fill(&mut screen, "https://ex.com plus");
        let hit = url_at(&screen, 0, 0, 2).is_some();
        let miss = url_at(&screen, 0, 0, 20).is_none();
        assert!(hit);
        assert!(miss);

        assert!(
            click_owns_url(true, hit),
            "Ctrl/Cmd+click on a URL opens, not select"
        );
        assert!(
            !click_owns_url(true, false),
            "open-gesture miss defers to ADR-0001/0003"
        );
        assert!(
            !click_owns_url(false, hit),
            "plain left-click on a URL stays selection"
        );
        assert!(!click_owns_url(false, false));
    }

    #[test]
    fn open_url_click_is_primary_modifier_without_shift() {
        #[cfg(target_os = "macos")]
        {
            assert!(is_open_url_click(mods(false, false, true)));
            assert!(!is_open_url_click(mods(true, false, false)));
            assert!(!is_open_url_click(mods(false, true, true)));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(is_open_url_click(mods(true, false, false)));
            assert!(!is_open_url_click(mods(false, false, true)));
            assert!(!is_open_url_click(mods(true, true, false)));
        }
        assert!(!is_open_url_click(mods(false, false, false)));
        assert!(!is_open_url_click(mods(false, true, false)));
    }

    #[test]
    fn view_scroll_window_finds_url_in_history() {
        let mut screen = Screen::new(40, 2, 10);
        fill(&mut screen, "https://scrolled.example/\nnext\nlive");
        assert_eq!(screen.max_view_scroll(), 1);
        assert_eq!(
            url_at(&screen, 0, 0, 0),
            None,
            "live view does not show the scrolled URL"
        );
        assert_eq!(
            url_at(&screen, 1, 0, 0).as_deref(),
            Some("https://scrolled.example/")
        );
    }
}
