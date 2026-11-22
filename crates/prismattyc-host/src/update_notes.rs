//! Short update notices: the version, a few one-line highlights from the
//! release notes, and a link to the full notes. A notice never carries the
//! whole changelog. The macOS alert shows a few highlights; the one-line
//! notice on Linux and Windows shows the top one.

const RELEASES: &str = "https://github.com/moonbase2090/Prismattyc/releases";
/// Highlights a notice shows at most.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAX_HIGHLIGHTS: usize = 3;
/// Characters one highlight keeps, including the closing ellipsis.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAX_HIGHLIGHT_CHARS: usize = 80;

/// The release page with the full notes for `version`, or the releases list
/// when the version is not a plain release number.
pub fn release_notes_url(version: &str) -> String {
    let version = version.strip_prefix('v').unwrap_or(version);
    let plain = version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        && !version.contains("..");
    if plain {
        format!("{RELEASES}/tag/v{version}")
    } else {
        RELEASES.to_string()
    }
}

/// The first few changes of a release, one line each, and how many more the
/// full notes list.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Highlights {
    pub items: Vec<String>,
    pub more: usize,
}

/// Pick the highlights from changelog Markdown: the first top-level bullets,
/// in the order the notes list them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn release_highlights(release_notes: &str) -> Highlights {
    highlights(release_notes, MAX_HIGHLIGHTS, MAX_HIGHLIGHT_CHARS)
}

/// [`release_highlights`] with a smaller budget, for one-line notices.
pub fn highlights(release_notes: &str, count: usize, max_chars: usize) -> Highlights {
    // Top-level bullets only: nested ones are detail of the bullet above.
    let bullets: Vec<String> = release_notes
        .lines()
        .filter_map(|line| line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")))
        .map(|bullet| one_line(bullet, max_chars))
        .filter(|line| !line.is_empty())
        .collect();
    Highlights {
        more: bullets.len().saturating_sub(count),
        items: bullets.into_iter().take(count).collect(),
    }
}

/// The body of the update alert: the highlights as bullets and where the
/// rest is. Never the changelog itself.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn update_notice_text(release_notes: &str) -> String {
    let Highlights { items, more } = release_highlights(release_notes);
    if items.is_empty() {
        return "See the release notes for what changed.".into();
    }
    let mut lines: Vec<String> = items.iter().map(|item| format!("• {item}")).collect();
    match more {
        0 => {}
        1 => lines.push("And 1 more change in the release notes.".into()),
        n => lines.push(format!("And {n} more changes in the release notes.")),
    }
    lines.join("\n")
}

/// One bullet as a single short line: Markdown and parenthetical asides
/// dropped, then its first clause, cut at a word when still too long.
fn one_line(bullet: &str, max_chars: usize) -> String {
    let plain = without_asides(&plain_text(bullet));
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let clause = [": ", "; ", ". "]
        .iter()
        .filter_map(|stop| plain.find(stop))
        .min()
        .map_or(plain.as_str(), |end| &plain[..end]);
    let clause = clause.trim_end_matches(['.', ',', ':', ';', ' ']);
    if clause.chars().count() <= max_chars {
        return clause.to_string();
    }
    // Keep whole words that fit before the ellipsis.
    let budget = max_chars.saturating_sub(1);
    let mut kept = String::new();
    for word in clause.split(' ') {
        let len = kept.chars().count() + usize::from(!kept.is_empty()) + word.chars().count();
        if len > budget {
            break;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(word);
    }
    if kept.is_empty() {
        kept = clause.chars().take(budget).collect();
    }
    format!("{}…", kept.trim_end_matches([',', ':', ';', '.']))
}

/// Drop code, emphasis and link markup, keeping the words.
fn plain_text(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut chars = markdown.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' | '*' => {}
            // A link keeps its text; the target in `(...)` is skipped.
            ']' if chars.peek() == Some(&'(') => {
                for skipped in chars.by_ref() {
                    if skipped == ')' {
                        break;
                    }
                }
            }
            '[' | ']' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Remove `(...)` asides, nested ones included, with the space before them.
fn without_asides(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => {
                if depth == 0 {
                    let kept = out.trim_end().len();
                    out.truncate(kept);
                }
                depth += 1;
            }
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES_0_3_73: &str = include_str!("update_notes_0_3_73.md");

    #[test]
    fn a_real_changelog_gives_three_short_highlights_and_a_count() {
        let highlights = release_highlights(NOTES_0_3_73);
        assert_eq!(
            highlights.items,
            [
                "Experimental native terminal grids and uploaded images for direct local PTY…",
                "pmux update is now the one update command",
                "Every dialog, picker and modal menu now dims the app behind it and blurs it…",
            ]
        );
        assert_eq!(highlights.more, 6, "9 bullets, 3 shown");
    }

    #[test]
    fn the_alert_text_is_the_highlights_never_the_changelog() {
        let text = update_notice_text(NOTES_0_3_73);
        assert_eq!(
            text,
            "• Experimental native terminal grids and uploaded images for direct local PTY…\n\
             • pmux update is now the one update command\n\
             • Every dialog, picker and modal menu now dims the app behind it and blurs it…\n\
             And 6 more changes in the release notes."
        );
        for line in text.lines() {
            assert!(line.chars().count() <= MAX_HIGHLIGHT_CHARS + 2, "{line}");
        }
        for section in ["Added", "Changed", "Fixed", "##", "`", "**"] {
            assert!(!text.contains(section), "{section} leaked into {text}");
        }
    }

    #[test]
    fn a_highlight_is_the_first_clause_without_markup_or_asides() {
        let notes = "### Fixed\n\n\
            - **Restart** keeps `tmux` panes (and their [layouts](https://example.invalid)); more detail.\n\
            - Second item. Another sentence.\n\
            - Third: with detail.\n";
        assert_eq!(
            release_highlights(notes).items,
            ["Restart keeps tmux panes", "Second item", "Third"]
        );
    }

    #[test]
    fn a_long_clause_is_cut_at_a_word_with_an_ellipsis() {
        let notes = format!("- {}\n", "word ".repeat(40));
        let item = &release_highlights(&notes).items[0];
        assert!(item.ends_with("word…"), "{item}");
        assert!(item.chars().count() <= MAX_HIGHLIGHT_CHARS, "{item}");
        assert!(item.chars().count() > MAX_HIGHLIGHT_CHARS - 6, "{item}");
    }

    #[test]
    fn nested_bullets_and_paragraphs_are_not_highlights() {
        let notes =
            "Intro paragraph.\n\n### Added\n\n- Top.\n  - Nested detail.\n\nTrailing text.\n";
        assert_eq!(
            release_highlights(notes),
            Highlights {
                items: vec!["Top".into()],
                more: 0
            }
        );
        assert_eq!(update_notice_text(notes), "• Top");
    }

    #[test]
    fn a_one_line_notice_gets_one_shorter_highlight() {
        assert_eq!(
            highlights(NOTES_0_3_73, 1, 48),
            Highlights {
                items: vec!["Experimental native terminal grids and uploaded…".into()],
                more: 8
            }
        );
    }

    #[test]
    fn one_hidden_change_is_singular() {
        let text = update_notice_text("- One.\n- Two.\n- Three.\n- Four.\n");
        assert!(
            text.ends_with("\nAnd 1 more change in the release notes."),
            "{text}"
        );
    }

    #[test]
    fn notes_without_bullets_point_at_the_release_notes() {
        for notes in [
            "",
            "  \n",
            "## [0.3.74] - 2026-11-30\n\nA paragraph only.\n",
        ] {
            assert_eq!(
                update_notice_text(notes),
                "See the release notes for what changed."
            );
        }
    }

    #[test]
    fn the_notes_link_names_the_release_tag() {
        let tag = "https://github.com/moonbase2090/Prismattyc/releases/tag/v0.3.73";
        assert_eq!(release_notes_url("0.3.73"), tag);
        assert_eq!(release_notes_url("v0.3.73"), tag);
        assert_eq!(
            release_notes_url("0.4.0-rc.1"),
            "https://github.com/moonbase2090/Prismattyc/releases/tag/v0.4.0-rc.1"
        );
        for odd in ["", "0.3.73/../x", "0.3.73 x", "0.3.73?q"] {
            assert_eq!(
                release_notes_url(odd),
                "https://github.com/moonbase2090/Prismattyc/releases"
            );
        }
    }
}
