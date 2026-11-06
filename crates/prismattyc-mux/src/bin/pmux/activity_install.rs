// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{Cli, MANAGED_MARKER};

#[path = "activity_install_receipt.rs"]
mod receipt;
use receipt::CreatedFields;

const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

fn command(cli: &Cli, executable: &Path, hook_event: Option<&str>) -> Result<String> {
    let mut args = vec![
        executable
            .to_str()
            .context("executable path must be UTF-8")?
            .to_owned(),
        "--socket".into(),
        cli.socket
            .to_str()
            .context("socket path must be UTF-8")?
            .to_owned(),
        "activity-emit".into(),
    ];
    if !cli.dynamic_seat {
        args.extend(["--seat".into(), cli.seat.as_str().to_owned()]);
    }
    args.push("--hook".into());
    if let Some(event) = hook_event {
        args.extend(["--hook-event".into(), event.to_owned()]);
    }
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
    fields: &mut CreatedFields,
) -> Result<Value> {
    let object = document
        .as_object_mut()
        .context("hook settings must be a JSON object")?;
    let mut created = fields.clone();
    if command.is_some() {
        created.hooks |= !object.contains_key("hooks");
        created.schema |= schema && !object.contains_key("schema_version");
    }
    if schema {
        match object.get("schema_version") {
            None if command.is_some() => {
                object.insert("schema_version".into(), json!(1));
            }
            None => {}
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
    hooks.retain(|event, entries| {
        let Some(entries) = entries.as_array_mut() else {
            return true;
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
        !created.events.contains(event) || !entries.is_empty()
    });
    if let Some(command) = command {
        let mut events = EVENTS.to_vec();
        if failure {
            events.push("PostToolUseFailure");
        }
        created
            .events
            .retain(|event| events.contains(&event.as_str()));
        for event in events {
            if !hooks.contains_key(event) {
                created.events.insert(event.to_owned());
            }
            let entries = hooks.entry(event).or_insert_with(|| json!([]));
            entries
                .as_array_mut()
                .context("hook event must be an array")?
                .push(json!({
                    "hooks": [{"type":"command", "command":command, "timeout":5}]
                }));
        }
    } else {
        let remove_hooks = created.hooks && hooks.is_empty();
        if remove_hooks {
            object.remove("hooks");
        }
        if created.schema && object.get("schema_version") == Some(&json!(1)) {
            object.remove("schema_version");
        }
        created = CreatedFields::default();
    }
    *fields = created;
    Ok(document)
}

fn transact(file: &Path, command: Option<&str>, schema: bool, failure: bool) -> Result<bool> {
    transact_document(file, |document| {
        let receipt_file = receipt::path(file)?;
        let before = receipt::load(&receipt_file, &document)?;
        let mut after = before.clone();
        let updated = reconcile(document.clone(), command, schema, failure, &mut after)?;
        if (updated != document || after != before)
            && (before != CreatedFields::default() || after != CreatedFields::default())
        {
            receipt::save(&receipt_file, &document, before, &updated, after)?;
        }
        Ok(updated)
    })
}

fn transact_document(file: &Path, transform: impl FnOnce(Value) -> Result<Value>) -> Result<bool> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).context("create hook settings directory")?;
    }
    let commit = prismattyc_mux::config::transact_file(file, Some(1024 * 1024), |raw| {
        let document = if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid hook settings JSON"))?
        };
        let updated = transform(document.clone())?;
        let contents = if updated == document {
            raw.to_owned()
        } else {
            serde_json::to_string_pretty(&updated)? + "\n"
        };
        Ok((contents, ()))
    })?;
    let changed = commit.changed;
    commit.report();
    Ok(changed)
}

pub(super) fn run(cli: &Cli, file: Option<&PathBuf>, remove: bool) -> Result<()> {
    if cli.all_harnesses {
        let home = std::env::var_os("HOME").context("HOME is required for --all-harnesses")?;
        let home = PathBuf::from(home);
        let executable = std::env::current_exe().context("resolve installed executable")?;
        let mut changed = false;
        let claude = command(cli, &executable, None)?;
        changed |= transact(
            &home.join(".claude/settings.json"),
            Some(&claude),
            true,
            cli.failure_hook,
        )?;
        let codex = command(cli, &executable, None)?;
        changed |= transact(
            &home.join(".codex/hooks.json"),
            Some(&codex),
            false,
            cli.failure_hook,
        )?;
        let cursor_file = home.join(".cursor/hooks.json");
        let cursor_events = [
            "sessionStart",
            "sessionEnd",
            "beforeSubmitPrompt",
            "preToolUse",
            "postToolUse",
            "stop",
        ];
        changed |= transact_document(&cursor_file, |cursor| {
            let mut cursor = reconcile_cursor(cursor, &cursor_events)?;
            for event in cursor_events {
                cursor = add_cursor_event(cursor, event, &command(cli, &executable, Some(event))?)?;
            }
            Ok(cursor)
        })?;
        let kiro_file = home.join(".kiro/hooks/pmux-activity.json");
        let kiro_events = [
            ("UserPromptSubmit", "UserPromptSubmit"),
            ("Stop", "Stop"),
            ("SessionStart", "SessionStart"),
            ("SessionEnd", "SessionEnd"),
            ("PreToolUse", "PreToolUse"),
            ("PostToolUse", "PostToolUse"),
        ];
        changed |= transact_document(&kiro_file, |kiro| {
            let mut kiro = reconcile_kiro(kiro, &[])?;
            for (trigger, event) in kiro_events {
                kiro = add_kiro_event(kiro, trigger, &command(cli, &executable, Some(event))?)?;
            }
            Ok(kiro)
        })?;
        println!("{}", if changed { "installed" } else { "unchanged" });
        return Ok(());
    }
    let file = file.context("hook settings file is required")?;
    if remove && !file.exists() {
        println!("unchanged");
        return Ok(());
    }
    let executable = std::env::current_exe().context("resolve installed executable")?;
    let command = (!remove)
        .then(|| command(cli, &executable, None))
        .transpose()?;
    let changed = transact(
        file,
        command.as_deref(),
        cli.schema_version,
        cli.failure_hook,
    )?;
    println!(
        "{}",
        if changed {
            if remove {
                "removed"
            } else {
                "installed"
            }
        } else {
            "unchanged"
        }
    );
    Ok(())
}

fn reconcile_cursor(mut document: Value, events: &[&str]) -> Result<Value> {
    let object = document
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    match object.get("version") {
        None => {
            object.insert("version".into(), json!(1));
        }
        Some(version) if version == &json!(1) => {}
        _ => bail!("Cursor hooks use an unsupported version"),
    }
    let hooks = object.entry("hooks").or_insert(json!({}));
    hooks
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    for entries in hooks.as_object_mut().unwrap().values_mut() {
        if let Some(entries) = entries.as_array_mut() {
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| cmd.ends_with(MANAGED_MARKER))
            });
        }
    }
    let hooks = document["hooks"].as_object_mut().unwrap();
    for event in events {
        hooks
            .entry(*event)
            .or_insert(json!([]))
            .as_array()
            .context("Cursor hook event must be an array")?;
    }
    Ok(document)
}

fn add_cursor_event(mut document: Value, event: &str, command: &str) -> Result<Value> {
    let entries = document["hooks"][event]
        .as_array_mut()
        .context("Cursor hook event must be an array")?;
    entries.push(json!({"command":command,"timeout":5}));
    Ok(document)
}

fn reconcile_kiro(mut document: Value, _commands: &[String]) -> Result<Value> {
    if !document.is_object() {
        bail!("Kiro hooks must be an object");
    }
    if !document.get("version").is_some_and(|v| v == "v1") {
        document["version"] = json!("v1");
    }
    let hooks = document
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert(json!([]));
    let hooks = hooks
        .as_array_mut()
        .context("Kiro hooks must be an array")?;
    hooks.retain(|entry| {
        !entry
            .pointer("/action/command")
            .and_then(Value::as_str)
            .is_some_and(|cmd| cmd.ends_with(MANAGED_MARKER))
    });
    Ok(document)
}

fn add_kiro_event(mut document: Value, trigger: &str, command: &str) -> Result<Value> {
    document["hooks"].as_array_mut().context("Kiro hooks must be an array")?.push(json!({"name":"pmux-activity","trigger":trigger,"action":{"type":"command","command":command},"timeout":5}));
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(label: &str) -> String {
        format!("{label} {MANAGED_MARKER}")
    }

    #[test]
    fn oversized_hook_settings_reject_before_utf8_decoding() {
        let directory = std::env::temp_dir().join(format!(
            "pmux-hook-size-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        let mut original = vec![b' '; 4 * 1024 * 1024];
        original[4 * 1024 * 1024 - 1] = 0xff;
        std::fs::write(&path, &original).unwrap();
        let cli = super::super::parse(
            Path::new("/tmp/feed.sock"),
            ["--seat", "worker-a", "--hook"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let error = run(&cli, Some(&path), false).unwrap_err();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(
            saved, original,
            "a rejected edit must preserve the original bytes"
        );
        assert!(
            format!("{error:#}").contains("text file exceeds 1048576 byte read limit"),
            "oversized settings must be rejected before decoding: {error:#}"
        );
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
        let command = command(&cli, Path::new("/tmp/pmux"), None).unwrap();
        let mut fields = CreatedFields::default();
        let once = reconcile(original.clone(), Some(&command), true, true, &mut fields).unwrap();
        let twice = reconcile(once.clone(), Some(&command), true, true, &mut fields).unwrap();
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
        let mut fields = CreatedFields::default();
        let first = reconcile(original, Some(&managed("first")), false, true, &mut fields).unwrap();
        let second = reconcile(first, Some(&managed("second")), false, false, &mut fields).unwrap();
        assert_eq!(second["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            second["hooks"]["Stop"][1]["hooks"][0]["command"],
            managed("second")
        );
        assert!(second["hooks"].get("PostToolUseFailure").is_none());
        let removed = reconcile(second, None, false, false, &mut fields).unwrap();
        assert_eq!(
            removed["hooks"]["Stop"],
            json!([{"hooks":[{"type":"command","command":"preserve"}]}])
        );
        assert_eq!(
            reconcile(removed.clone(), None, false, false, &mut fields).unwrap(),
            removed
        );
    }

    #[test]
    fn removal_restores_preexisting_empty_arrays_and_unrelated_settings() {
        let original = json!({
            "schema_version": 1,
            "env": {"EXAMPLE": "preserve-me"},
            "hooks": {
                "PreToolUse": [],
                "CustomEvent": [],
                "Stop": [{"hooks": [{"type": "command", "command": "user-handler"}]}]
            }
        });
        let mut fields = CreatedFields::default();
        let installed = reconcile(
            original.clone(),
            Some(&managed("first")),
            true,
            true,
            &mut fields,
        )
        .unwrap();
        let rebound = reconcile(
            installed,
            Some(&managed("second")),
            true,
            false,
            &mut fields,
        )
        .unwrap();
        assert!(rebound["hooks"].get("PostToolUseFailure").is_none());
        let removed = reconcile(rebound, None, false, false, &mut fields).unwrap();
        assert_eq!(removed, original);
        assert_eq!(
            reconcile(removed, None, false, false, &mut fields).unwrap(),
            original
        );
    }

    #[test]
    fn removal_restores_absent_hook_and_schema_fields() {
        let original = json!({"env": {"EXAMPLE": "preserve-me"}});
        let mut fields = CreatedFields::default();
        let installed = reconcile(
            original.clone(),
            Some(&managed("first")),
            true,
            true,
            &mut fields,
        )
        .unwrap();
        let reinstalled = reconcile(
            installed.clone(),
            Some(&managed("first")),
            true,
            true,
            &mut fields,
        )
        .unwrap();
        assert_eq!(reinstalled, installed);
        assert_eq!(
            reconcile(reinstalled, None, false, false, &mut fields).unwrap(),
            original
        );
    }

    #[test]
    fn reinstall_keeps_created_event_ownership_while_user_handlers_are_present() {
        let mut fields = CreatedFields::default();
        let mut installed = reconcile(
            json!({}),
            Some(&managed("first")),
            false,
            false,
            &mut fields,
        )
        .unwrap();
        installed["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "hooks": [{"type": "command", "command": "user-handler"}]
            }));
        let mut rebound = reconcile(
            installed,
            Some(&managed("second")),
            false,
            false,
            &mut fields,
        )
        .unwrap();
        rebound["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .retain(|entry| entry["hooks"][0]["command"] != "user-handler");
        assert_eq!(
            reconcile(rebound, None, false, false, &mut fields).unwrap(),
            json!({})
        );
    }

    #[test]
    fn legacy_managed_entries_preserve_unknown_container_origins() {
        let original = json!({"schema_version": 1, "hooks": {
            "CustomEvent": [], "Stop": [{"hooks": [{"command": managed("legacy")}]}]
        }});
        assert_eq!(
            reconcile(original, None, false, false, &mut CreatedFields::default()).unwrap(),
            json!({"schema_version": 1, "hooks": {"CustomEvent": [], "Stop": []}})
        );
        assert_eq!(
            reconcile(json!({}), None, true, false, &mut CreatedFields::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn recording_created_fields_keeps_the_command_unchanged() {
        let mut fields = CreatedFields::default();
        let installed = reconcile(
            json!({}),
            Some("fixture --managed-hook=pmux-activity-v1"),
            true,
            true,
            &mut fields,
        )
        .unwrap();
        assert_eq!(
            installed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "fixture --managed-hook=pmux-activity-v1"
        );
    }

    #[test]
    fn invalid_shapes_and_schema_are_rejected() {
        assert!(reconcile(
            json!([]),
            Some("cmd"),
            false,
            false,
            &mut CreatedFields::default()
        )
        .is_err());
        assert!(reconcile(
            json!({"hooks":[]}),
            Some("cmd"),
            false,
            false,
            &mut CreatedFields::default()
        )
        .is_err());
        assert!(reconcile(
            json!({"schema_version":2}),
            Some("cmd"),
            true,
            false,
            &mut CreatedFields::default()
        )
        .is_err());
        assert!(reconcile(
            json!({"hooks":{"Stop":{}}}),
            Some("cmd"),
            false,
            false,
            &mut CreatedFields::default()
        )
        .is_err());
    }

    #[test]
    fn cursor_and_kiro_reconcile_only_marked_handlers() {
        let events = ["sessionStart", "sessionEnd"];
        let cursor = json!({"hooks":{"sessionStart":[
            {"command":"keep"}, {"command":managed("old")}
        ]}});
        let cursor = reconcile_cursor(cursor, &events).unwrap();
        assert_eq!(cursor["version"], 1);
        let cursor = add_cursor_event(cursor, "sessionStart", &managed("new")).unwrap();
        assert_eq!(cursor["hooks"]["sessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(cursor["hooks"]["sessionEnd"], json!([]));

        let kiro = json!({"version":"v1","hooks":[
            {"name":"keep","trigger":"PromptSubmit","action":{"type":"command","command":"keep"}},
            {"name":"pmux","trigger":"PromptSubmit","action":{"type":"command","command":managed("old")}}
        ]});
        let kiro = reconcile_kiro(kiro, &[]).unwrap();
        let kiro = add_kiro_event(kiro, "UserPromptSubmit", &managed("new")).unwrap();
        assert_eq!(kiro["hooks"].as_array().unwrap().len(), 2);
        assert_eq!(kiro["hooks"][0]["action"]["command"], "keep");
        assert_eq!(kiro["hooks"][1]["trigger"], "UserPromptSubmit");
    }

    #[test]
    fn shared_harness_commands_resolve_agent_identity_at_runtime() {
        let mut cli = super::super::parse(
            Path::new("/tmp/feed.sock"),
            ["--seat", "worker-a", "--hook"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        cli.dynamic_seat = true;
        let command = command(&cli, Path::new("/tmp/pmux"), Some("sessionStart")).unwrap();
        assert!(!command.contains("--seat"));
        assert!(command.contains("--hook-event"));
    }
}
