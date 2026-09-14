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
}
