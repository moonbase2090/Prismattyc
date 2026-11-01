// SPDX-License-Identifier: MIT

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{Cli, MANAGED_MARKER};

const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

fn command(cli: &Cli, executable: &Path) -> Result<String> {
    let args = [
        executable
            .to_str()
            .context("executable path must be UTF-8")?,
        "--socket",
        cli.socket.to_str().context("socket path must be UTF-8")?,
        "activity-emit",
        "--seat",
        cli.seat.as_str(),
        "--hook",
    ];
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Result<Vec<_>>>()
        .map(|parts| format!("{} {MANAGED_MARKER}", parts.join(" ")))
}

#[cfg(unix)]
fn quote(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("hook arguments must not contain control characters");
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

#[cfg(windows)]
fn quote(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) || value.contains(['"', '%', '!']) {
        bail!("hook paths must not contain command expansion characters");
    }
    Ok(format!("\"{value}\""))
}

fn reconcile(
    mut document: Value,
    command: Option<&str>,
    schema: bool,
    failure: bool,
) -> Result<Value> {
    let object = document
        .as_object_mut()
        .context("hook settings must be a JSON object")?;
    if schema {
        match object.get("schema_version") {
            None => {
                object.insert("schema_version".into(), json!(1));
            }
            Some(value) if value == &json!(1) => {}
            _ => bail!("hook settings use a different schema version"),
        }
    }
    if !object.contains_key("hooks") && command.is_none() {
        return Ok(document);
    }
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("hooks must be a JSON object")?;
    // Remove only handlers carrying our marker, including events disabled by a later install.
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        entries.retain_mut(|entry| {
            let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|handler| {
                !handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.ends_with(MANAGED_MARKER))
            });
            before == handlers.len() || !handlers.is_empty()
        });
    }
    if let Some(command) = command {
        let mut events = EVENTS.to_vec();
        if failure {
            events.push("PostToolUseFailure");
        }
        for event in events {
            let entries = hooks.entry(event).or_insert_with(|| json!([]));
            entries
                .as_array_mut()
                .context("hook event must be an array")?
                .push(json!({
                    "hooks": [{"type":"command", "command":command, "timeout":5}]
                }));
        }
    }
    Ok(document)
}

pub(super) fn run(cli: &Cli, file: &Path, remove: bool) -> Result<()> {
    if remove && !file.exists() {
        println!("unchanged");
        return Ok(());
    }
    let executable = std::env::current_exe().context("resolve installed executable")?;
    let command = (!remove).then(|| command(cli, &executable)).transpose()?;
    let commit = prismattyc_mux::config::transact_file(file, |raw| {
        if raw.len() > 1024 * 1024 {
            bail!("hook settings exceed 1 MiB");
        }
        let document = if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid hook settings JSON"))?
        };
        let updated = reconcile(
            document.clone(),
            command.as_deref(),
            cli.schema_version,
            cli.failure_hook,
        )?;
        let contents = if updated == document {
            raw.to_owned()
        } else {
            serde_json::to_string_pretty(&updated)? + "\n"
        };
        Ok((contents, ()))
    })?;
    println!(
        "{}",
        if commit.changed {
            if remove {
                "removed"
            } else {
                "installed"
            }
        } else {
            "unchanged"
        }
    );
    commit.report();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(label: &str) -> String {
        format!("{label} {MANAGED_MARKER}")
    }

    #[test]
    fn repeated_install_preserves_unmanaged_settings_and_has_one_handler_per_event() {
        let original = json!({"schema_version":1,"env":{"EXAMPLE":"preserve-me"},
            "managed_hooks_path":"existing-managed-file", "hooks":{"SessionStart":[
            {"matcher":"startup", "hooks":[{"type":"command","command":"existing-command"}]}]}});
        let cli = super::super::parse(
            Path::new("/tmp/feed.sock"),
            ["--seat", "worker-a", "--hook"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let command = command(&cli, Path::new("/tmp/pmux")).unwrap();
        let once = reconcile(original.clone(), Some(&command), true, true).unwrap();
        let twice = reconcile(once.clone(), Some(&command), true, true).unwrap();
        assert_eq!(twice, once);
        assert_eq!(twice["env"]["EXAMPLE"], "preserve-me");
        assert_eq!(twice["managed_hooks_path"], "existing-managed-file");
        assert_eq!(
            twice["hooks"]["SessionStart"][0],
            original["hooks"]["SessionStart"][0]
        );
        assert_eq!(twice["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(
            twice["hooks"]["PostToolUseFailure"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn changed_binding_and_removal_preserve_other_handlers() {
        let original =
            json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"preserve"}]}]}});
        let first = reconcile(original, Some(&managed("first")), false, true).unwrap();
        let second = reconcile(first, Some(&managed("second")), false, false).unwrap();
        assert_eq!(second["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            second["hooks"]["Stop"][1]["hooks"][0]["command"],
            managed("second")
        );
        assert!(second["hooks"]["PostToolUseFailure"]
            .as_array()
            .unwrap()
            .is_empty());
        let removed = reconcile(second, None, false, false).unwrap();
        assert_eq!(
            removed["hooks"]["Stop"],
            json!([{"hooks":[{"type":"command","command":"preserve"}]}])
        );
        assert_eq!(
            reconcile(removed.clone(), None, false, false).unwrap(),
            removed
        );
    }

    #[test]
    fn invalid_shapes_and_schema_are_rejected() {
        assert!(reconcile(json!([]), Some("cmd"), false, false).is_err());
        assert!(reconcile(json!({"hooks":[]}), Some("cmd"), false, false).is_err());
        assert!(reconcile(json!({"schema_version":2}), Some("cmd"), true, false).is_err());
        assert!(reconcile(json!({"hooks":{"Stop":{}}}), Some("cmd"), false, false).is_err());
    }
}
