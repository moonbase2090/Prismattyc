//! Prepare changelog Markdown for the macOS update dialog.

/// Drop the duplicate version heading, then render a small Markdown subset.
///
/// Changelog sections start with `## [0.3.34] - date`. The dialog already says
/// `Prismattyc {version} is available`, so that heading is removed. Remaining
/// `##` / `###` headings, `-` / `*` bullets, and `**` / `*` emphasis become
/// plain text the alert can show without raw Markdown markers.
pub fn prepare_update_release_notes(version: &str, release_notes: &str) -> String {
    let stripped = strip_version_heading(version, release_notes);
    if stripped.is_empty() {
        return String::new();
    }
    let rendered = render_release_notes_markdown(&stripped);
    const MAX_CHARS: usize = 8_000;
    if rendered.chars().count() > MAX_CHARS {
        format!("{}…", rendered.chars().take(MAX_CHARS).collect::<String>())
    } else {
        rendered
    }
}

fn strip_version_heading(version: &str, release_notes: &str) -> String {
    let version = version.strip_prefix('v').unwrap_or(version);
    let notes = release_notes.trim();
    if notes.is_empty() {
        return String::new();
    }

    let mut lines = notes.lines().peekable();
    while matches!(lines.peek(), Some(line) if line.trim().is_empty()) {
        lines.next();
    }

    if let Some(first) = lines.peek().copied() {
        if line_is_version_heading(first, version) {
            lines.next();
            while matches!(lines.peek(), Some(line) if line.trim().is_empty()) {
                lines.next();
            }
        }
    }

    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

fn line_is_version_heading(line: &str, version: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix("##") else {
        return false;
    };
    // Require a space (or end) after ## so ### headings are not treated as version lines.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let rest = rest.trim_start();
    let token = if let Some(inner) = rest.strip_prefix('[') {
        inner.split(']').next().unwrap_or("").trim()
    } else {
        rest.split_whitespace().next().unwrap_or("")
    };
    let token = token.strip_prefix('v').unwrap_or(token);
    token == version
}

/// Render a small Markdown subset used in changelog release notes.
pub fn render_release_notes_markdown(markdown: &str) -> String {
    let mut out = String::new();
    let mut previous_blank = true;
    for line in markdown.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if !out.is_empty() && !previous_blank {
                out.push('\n');
                previous_blank = true;
            }
            continue;
        }

        let body = if let Some(rest) = strip_atx_heading(trimmed) {
            rest.to_string()
        } else if let Some(rest) = trimmed.strip_prefix("- ") {
            format!("• {}", apply_inline_emphasis(rest))
        } else if let Some(rest) = trimmed.strip_prefix("* ") {
            format!("• {}", apply_inline_emphasis(rest))
        } else {
            apply_inline_emphasis(trimmed)
        };

        if !out.is_empty() && !previous_blank {
            out.push('\n');
        }
        out.push_str(&body);
        previous_blank = false;
    }
    out.trim().to_string()
}

fn strip_atx_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('#')?;
    let mut hashes = 1usize;
    let mut rest = rest;
    while let Some(next) = rest.strip_prefix('#') {
        hashes += 1;
        rest = next;
        if hashes > 6 {
            return None;
        }
    }
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

fn apply_inline_emphasis(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, "**") {
                out.extend(chars[i + 2..end].iter().copied());
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' {
            if let Some(end) = find_closing(&chars, i + 1, "*") {
                out.extend(chars[i + 1..end].iter().copied());
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_closing(chars: &[char], start: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    let mut i = start;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_bracketed_version_heading_and_renders_body() {
        let notes = "## [0.3.34] - 2026-10-10\n\n### Fixed\n\n- Sidebar rename works.\n";
        let prepared = prepare_update_release_notes("0.3.34", notes);
        assert!(!prepared.contains("##"), "{prepared}");
        assert!(!prepared.contains("###"), "{prepared}");
        assert!(!prepared.contains("0.3.34"), "{prepared}");
        assert!(prepared.contains("Fixed"), "{prepared}");
        assert!(prepared.contains("• Sidebar rename works."), "{prepared}");
    }

    #[test]
    fn drops_plain_version_heading() {
        let notes = "## 0.3.34\n\n### Added\n\n- One thing.\n";
        let prepared = prepare_update_release_notes("v0.3.34", notes);
        assert!(!prepared.contains("0.3.34"), "{prepared}");
        assert!(prepared.starts_with("Added"), "{prepared}");
        assert!(prepared.contains("• One thing."), "{prepared}");
    }

    #[test]
    fn keeps_body_when_heading_version_differs() {
        let notes = "## [0.3.33] - 2026-10-09\n\n### Fixed\n\n- Older note.\n";
        let prepared = prepare_update_release_notes("0.3.34", notes);
        assert!(prepared.contains("0.3.33"), "{prepared}");
        assert!(prepared.contains("• Older note."), "{prepared}");
    }

    #[test]
    fn empty_notes_stay_empty() {
        assert_eq!(prepare_update_release_notes("0.3.34", "  \n"), "");
    }

    #[test]
    fn truncates_very_long_notes() {
        let body = format!("### Fixed\n\n- {}\n", "x".repeat(8_050));
        let prepared = prepare_update_release_notes("0.3.34", &body);
        assert!(prepared.ends_with('…'), "{prepared}");
        assert_eq!(prepared.chars().count(), 8_001);
    }

    #[test]
    fn renders_emphasis_without_markers() {
        let rendered =
            render_release_notes_markdown("### Fixed\n\n- Uses **bold** and *italic*.\n");
        assert_eq!(rendered, "Fixed\n• Uses bold and italic.");
    }
}
