// SPDX-License-Identifier: MIT

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::MANAGED_MARKER;

const MAX_RECEIPT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CreatedFields {
    pub events: BTreeSet<String>,
    pub hooks: bool,
    pub schema: bool,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    fingerprint: String,
    created: CreatedFields,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u8,
    before: Snapshot,
    after: Snapshot,
}

pub(super) fn path(profile: &Path) -> Result<PathBuf> {
    let profile = profile
        .canonicalize()
        .unwrap_or_else(|_| profile.to_owned());
    let mut filename = std::ffi::OsString::from(".");
    filename.push(profile.file_name().context("hook profile filename")?);
    filename.push(".pmux-activity-origin");
    Ok(profile.with_file_name(filename))
}

fn kind(value: Option<&Value>) -> &'static str {
    match value {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn fingerprint(document: &Value, created: &CreatedFields) -> String {
    let mut handlers = Vec::new();
    if let Some(hooks) = document.get("hooks").and_then(Value::as_object) {
        for (event, entries) in hooks {
            for entry in entries.as_array().into_iter().flatten() {
                for handler in entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(command) = handler
                        .get("command")
                        .and_then(Value::as_str)
                        .filter(|command| command.ends_with(MANAGED_MARKER))
                    {
                        handlers.push((event.as_str(), command));
                    }
                }
            }
        }
    }
    handlers.sort_unstable();
    let events: Vec<_> = created
        .events
        .iter()
        .map(|event| {
            (
                event,
                kind(
                    document
                        .get("hooks")
                        .and_then(Value::as_object)
                        .and_then(|hooks| hooks.get(event)),
                ),
            )
        })
        .collect();
    let shape = (
        kind(document.get("hooks")),
        (
            document.get("schema_version").is_some(),
            document.get("schema_version"),
        ),
        events,
    );
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(handlers, shape)).expect("profile fingerprint serializes")
        )
    )
}

pub(super) fn load(file: &Path, document: &Value) -> Result<CreatedFields> {
    let input = match std::fs::File::open(file) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CreatedFields::default())
        }
        Err(error) => return Err(error).context("open hook origin receipt"),
    };
    let mut bytes = Vec::new();
    input.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT_BYTES,
        "hook origin receipt exceeds {MAX_RECEIPT_BYTES} byte read limit"
    );
    let receipt: Receipt = serde_json::from_slice(&bytes).context("invalid hook origin receipt")?;
    ensure!(
        receipt.version == 1,
        "unsupported hook origin receipt version"
    );
    for snapshot in [receipt.after, receipt.before] {
        if snapshot.fingerprint == fingerprint(document, &snapshot.created) {
            return Ok(snapshot.created);
        }
    }
    Ok(CreatedFields::default())
}

pub(super) fn save(
    file: &Path,
    before: &Value,
    before_fields: CreatedFields,
    after: &Value,
    after_fields: CreatedFields,
) -> Result<()> {
    // The profile lock is held by the caller. Persist both states before the
    // profile replacement, so a failed or completed replacement can recover.
    let receipt = Receipt {
        version: 1,
        before: Snapshot {
            fingerprint: fingerprint(before, &before_fields),
            created: before_fields,
        },
        after: Snapshot {
            fingerprint: fingerprint(after, &after_fields),
            created: after_fields,
        },
    };
    let contents = serde_json::to_string_pretty(&receipt)? + "\n";
    ensure!(
        contents.len() as u64 <= MAX_RECEIPT_BYTES,
        "hook origin receipt is too large"
    );
    let commit = prismattyc_mux::config::transact_file(file, Some(MAX_RECEIPT_BYTES), |_| {
        Ok((contents, ()))
    })?;
    commit.report();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hook-origin-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn profile(command: &str) -> Value {
        json!({"hooks": {"Stop": [{"hooks": [{"command": format!("{command} {MANAGED_MARKER}")}]}]}})
    }

    #[test]
    fn staged_receipt_distinguishes_shape_changes_with_unchanged_commands() {
        let scratch = Scratch::new();
        let file = scratch.0.join("origin");
        let before = profile("same");
        let mut after = before.clone();
        after["schema_version"] = json!(1);
        save(
            &file,
            &before,
            CreatedFields::default(),
            &after,
            CreatedFields {
                schema: true,
                ..CreatedFields::default()
            },
        )
        .unwrap();
        assert_eq!(load(&file, &before).unwrap(), CreatedFields::default());
        assert_eq!(
            load(&file, &after).unwrap(),
            CreatedFields {
                schema: true,
                ..CreatedFields::default()
            }
        );
    }

    #[test]
    fn staged_receipt_recognizes_both_commands_and_ignores_unmanaged_edits() {
        let scratch = Scratch::new();
        let file = scratch.0.join("origin");
        let before = profile("old");
        let mut after = profile("new");
        let owned = CreatedFields {
            hooks: true,
            events: BTreeSet::from(["Stop".into()]),
            ..CreatedFields::default()
        };
        save(&file, &before, owned.clone(), &after, owned.clone()).unwrap();
        assert_eq!(load(&file, &before).unwrap(), owned);
        assert_eq!(load(&file, &after).unwrap(), owned);
        after["env"] = json!({"USER_SETTING": "keep"});
        after["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks": [{"command": "user"}]}));
        assert_eq!(load(&file, &after).unwrap(), owned);
        assert_eq!(
            load(&file, &profile("different")).unwrap(),
            CreatedFields::default()
        );
    }

    #[test]
    fn oversized_receipt_is_rejected_before_decoding() {
        let scratch = Scratch::new();
        let file = scratch.0.join("origin");
        std::fs::write(&file, vec![0xff; MAX_RECEIPT_BYTES as usize + 1]).unwrap();
        let error = load(&file, &profile("same")).unwrap_err();
        assert!(error.to_string().contains("byte read limit"), "{error:#}");
    }

    #[test]
    fn profile_conflict_keeps_external_bytes_and_the_previous_receipt_state() {
        let scratch = Scratch::new();
        let file = scratch.0.join("settings.json");
        let origin_file = path(&file).unwrap();
        let before = profile("old");
        let after = profile("new");
        let mut external = before.clone();
        external["env"] = json!({"USER_SETTING": "external"});
        let external_bytes = serde_json::to_string(&external).unwrap();
        std::fs::write(&file, serde_json::to_string(&before).unwrap()).unwrap();
        let owned = CreatedFields {
            hooks: true,
            events: BTreeSet::from(["Stop".into()]),
            ..CreatedFields::default()
        };
        let attempt = super::super::transact_document(&file, |document| {
            save(
                &origin_file,
                &document,
                owned.clone(),
                &after,
                owned.clone(),
            )?;
            std::fs::write(&file, &external_bytes)?;
            Ok(after)
        });
        assert!(attempt.is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), external_bytes);
        assert_eq!(load(&origin_file, &external).unwrap(), owned);
    }
}
