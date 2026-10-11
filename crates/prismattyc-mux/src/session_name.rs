//! Human-readable session names shared by the CLI and host naming popup.

/// Suggest the first unused address under a Space name.
#[must_use]
pub fn suggest(space: &str, occupied: &[String]) -> String {
    let mut prefix = String::new();
    for ch in space.chars() {
        if ch.is_ascii_alphanumeric() {
            prefix.push(ch.to_ascii_lowercase());
        } else if !prefix.is_empty() && !prefix.ends_with('-') {
            prefix.push('-');
        }
        if prefix.len() >= 40 {
            break;
        }
    }
    let prefix = prefix.trim_end_matches('-');
    let prefix = if prefix.is_empty() { "session" } else { prefix };
    for index in 1_u64.. {
        let name = format!("{prefix}-{index}");
        if !occupied.contains(&name) {
            return name;
        }
    }
    unreachable!("session name space exhausted")
}

/// Mailbox address for a visible pane label.
///
/// Letters, digits, spaces, and other characters stay in the label. This
/// keeps only lowercase ASCII letters and digits, with a hyphen where a
/// run of other characters was. A label that cannot make a two-character
/// address becomes `session`.
#[must_use]
pub fn agent_id_from_label(label: &str) -> String {
    let mut out = String::new();
    for ch in label.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 64 {
            break;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.len() >= 2 {
        trimmed.to_string()
    } else {
        "session".to_string()
    }
}

/// First free mailbox address derived from `base`.
///
/// `taken` reports addresses already used by another session.
pub fn unique_agent_id(
    base: &str,
    mut taken: impl FnMut(&str) -> bool,
) -> Result<String, &'static str> {
    let base = if crate::mailbox::AgentId::new(base).is_ok() {
        base
    } else {
        "session"
    };
    if !taken(base) {
        return Ok(base.to_string());
    }
    for index in 2..10_000 {
        let candidate = format!("{base}-{index}");
        if candidate.len() > 64 {
            break;
        }
        if !taken(&candidate) {
            return Ok(candidate);
        }
    }
    Err("no free mailbox address")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suggestions_are_mail_addresses_and_skip_existing_names() {
        assert_eq!(
            suggest("Work", &["work-1".into(), "work-3".into()]),
            "work-2"
        );
        for name in ["Main", "Project / Work", "", "日本語", &"Long".repeat(25)] {
            crate::mailbox::AgentId::new(suggest(name, &[])).unwrap();
        }
    }

    #[test]
    fn pane_labels_keep_case_and_mail_addresses_are_derived() {
        assert_eq!(agent_id_from_label("Hello World"), "hello-world");
        assert_eq!(agent_id_from_label("  Build  "), "build");
        assert_eq!(agent_id_from_label("Café Menu"), "caf-menu");
        assert_eq!(agent_id_from_label("日本語"), "session");
        assert_eq!(agent_id_from_label("A"), "session");
        assert_eq!(agent_id_from_label("work-1"), "work-1");
        let mut used = vec!["hello-world".to_string()];
        let first = unique_agent_id("hello-world", |id| used.iter().any(|own| own == id)).unwrap();
        assert_eq!(first, "hello-world-2");
        used.push(first);
        assert_eq!(
            unique_agent_id("hello-world", |id| used.iter().any(|own| own == id)).unwrap(),
            "hello-world-3"
        );
        assert!(crate::mailbox::AgentId::new(agent_id_from_label("Hello World")).is_ok());
    }
}
