// SPDX-License-Identifier: MIT
//! Fresh-document, validated commits. The dialog only submits complete intents.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::config::ConfigFile;

#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub path: Vec<String>,
    pub expected: Option<toml::Value>,
    pub value: Option<toml::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Destination {
    pub original_id: Option<String>,
    pub id: String,
    pub ssh: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Edit {
    pub changes: Vec<Change>,
    pub remote: Option<(Vec<Destination>, Vec<Destination>)>,
}

pub(crate) struct Receipt {
    pub document: DocumentMut,
    pub config: ConfigFile,
    pub durability_warning: Option<String>,
}

pub(crate) fn value_at<'a>(root: &'a toml::Value, path: &[String]) -> Option<&'a toml::Value> {
    path.iter()
        .try_fold(root, |value, key| value.as_table()?.get(key))
}

pub(crate) fn destinations(config: &ConfigFile) -> Vec<Destination> {
    config
        .remote
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|entry| Destination {
            original_id: Some(entry.id.clone()),
            id: entry.id.clone(),
            ssh: entry.ssh.clone(),
            label: entry.label.clone(),
        })
        .collect()
}

pub(crate) fn commit(path: &Path, edit: Edit) -> Result<Receipt> {
    let result = prismattyc_mux::transact_config(path, |document| {
        let before = crate::config::parse(&document.to_string(), path)?;
        let current: toml::Value = document.to_string().parse()?;
        // Check every touched value before applying any part of a compound edit.
        for change in &edit.changes {
            anyhow::ensure!(
                value_at(&current, &change.path) == change.expected.as_ref(),
                "{} changed in another editor; reload before saving",
                change.path.join(".")
            );
        }
        if let Some((expected, _)) = &edit.remote {
            anyhow::ensure!(
                destinations(&before) == *expected,
                "remote destinations changed in another editor; reload before saving"
            );
        }
        for change in edit.changes {
            set_value(document, &change.path, change.value)?;
        }
        if let Some((_, records)) = edit.remote {
            patch_destinations(document, records)?;
        }
        let config = crate::config::parse(&document.to_string(), path)?;
        Ok((document.clone(), config))
    })?;
    Ok(Receipt {
        document: result.value.0,
        config: result.value.1,
        durability_warning: result.durability_warning,
    })
}

/// Set or remove one known path, retaining decorations and removed comments.
pub(crate) fn set_value(
    document: &mut DocumentMut,
    path: &[String],
    value: Option<toml::Value>,
) -> Result<()> {
    anyhow::ensure!(
        crate::config_schema::field(path).is_some(),
        "unknown settings target {}",
        path.join(".")
    );
    let mut comments = Vec::new();
    patch(document.as_table_mut(), path, value, &mut comments)?;
    if !comments.is_empty() {
        let trailing = document.trailing().as_str().unwrap_or_default();
        document.set_trailing(format!("{trailing}\n{}\n", comments.join("\n")));
    }
    Ok(())
}

pub(crate) fn item_value(item: Item) -> Result<toml::Value> {
    let value = item
        .into_value()
        .map_err(|_| anyhow::anyhow!("setting is not a TOML value"))?;
    let mut parsed: toml::Table = format!("value = {value}").parse()?;
    parsed.remove("value").context("setting value")
}

fn patch(
    table: &mut dyn TableLike,
    path: &[String],
    value: Option<toml::Value>,
    comments: &mut Vec<String>,
) -> Result<()> {
    let (key, rest) = path.split_first().context("empty config field path")?;
    if !rest.is_empty() {
        if !table.contains_key(key) {
            if value.is_none() {
                return Ok(());
            }
            let mut nested = toml_edit::Table::new();
            nested.set_implicit(true);
            table.insert(key, Item::Table(nested));
        }
        let nested = table
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .context("setting parent is not a table")?;
        return patch(nested, rest, value, comments);
    }
    if let Some(value) = value {
        let mut value = to_edit_value(value)?;
        if let Some(old) = table.get(key).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
            // Keep decoration attached to surviving ordered-list entries.
            if let (Some(old), Some(new)) = (old.as_array(), value.as_array_mut()) {
                let mut used = std::collections::HashSet::new();
                for new in new.iter_mut() {
                    if let Some((index, old)) = old.iter().enumerate().find(|(index, old)| {
                        !used.contains(index)
                            && match (old.as_str(), new.as_str()) {
                                (Some(old), Some(new)) => old == new,
                                _ => old.to_string().trim() == new.to_string().trim(),
                            }
                    }) {
                        used.insert(index);
                        *new = old.clone();
                    }
                }
                // Comments on edited or removed entries remain in the document.
                for (_, old) in old
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !used.contains(index))
                {
                    collect_comments(old.decor().prefix().and_then(|raw| raw.as_str()), comments);
                    collect_comments(old.decor().suffix().and_then(|raw| raw.as_str()), comments);
                }
            }
        }
        if let Some(item) = table.get_mut(key) {
            *item = Item::Value(value);
        } else {
            table.insert(key, Item::Value(value));
        }
    } else {
        if let Some(key) = table.key(key) {
            collect_comments(
                key.leaf_decor().prefix().and_then(|raw| raw.as_str()),
                comments,
            );
            collect_comments(
                key.leaf_decor().suffix().and_then(|raw| raw.as_str()),
                comments,
            );
        }
        if let Some(old) = table.remove(key).and_then(|item| item.into_value().ok()) {
            collect_value_comments(&old, comments);
        }
    }
    Ok(())
}

fn collect_value_comments(value: &toml_edit::Value, comments: &mut Vec<String>) {
    collect_comments(
        value.decor().prefix().and_then(|raw| raw.as_str()),
        comments,
    );
    collect_comments(
        value.decor().suffix().and_then(|raw| raw.as_str()),
        comments,
    );
    if let Some(array) = value.as_array() {
        for value in array {
            collect_value_comments(value, comments);
        }
    }
    if let Some(table) = value.as_inline_table() {
        for (_, value) in table {
            collect_value_comments(value, comments);
        }
    }
}

fn collect_comments(raw: Option<&str>, out: &mut Vec<String>) {
    if let Some(raw) = raw {
        for line in raw
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('#'))
        {
            out.push(line.to_owned());
        }
    }
}

fn to_edit_value(value: toml::Value) -> Result<toml_edit::Value> {
    let text = toml::to_string(&std::collections::BTreeMap::from([("value", value)]))?;
    text.parse::<DocumentMut>()?
        .remove("value")
        .context("serialized setting value")?
        .into_value()
        .map_err(|_| anyhow::anyhow!("setting value is not scalar or array"))
}

fn patch_destinations(document: &mut DocumentMut, records: Vec<Destination>) -> Result<()> {
    let mut existing = std::collections::HashMap::new();
    if let Some(tables) = document.get("remote").and_then(Item::as_array_of_tables) {
        for table in tables {
            if let Some(id) = table.get("id").and_then(Item::as_str) {
                existing.insert(id.to_string(), table.clone());
            }
        }
    } else if let Some(values) = document.get("remote").and_then(Item::as_array) {
        for value in values {
            if let Some(table) = value.as_inline_table() {
                if let Some(id) = table.get("id").and_then(toml_edit::Value::as_str) {
                    existing.insert(id.to_string(), table.clone().into_table());
                }
            }
        }
    }
    let mut tables = toml_edit::ArrayOfTables::new();
    for record in records {
        let mut table = if let Some(original) = record.original_id {
            existing
                .remove(&original)
                .context("destination identity changed")?
        } else {
            toml_edit::Table::new()
        };
        let mut comments = Vec::new();
        patch(
            &mut table,
            &["id".into()],
            Some(record.id.into()),
            &mut comments,
        )?;
        patch(
            &mut table,
            &["ssh".into()],
            Some(record.ssh.into()),
            &mut comments,
        )?;
        patch(
            &mut table,
            &["label".into()],
            record.label.map(Into::into),
            &mut comments,
        )?;
        if !comments.is_empty() {
            let prefix = table
                .decor()
                .prefix()
                .and_then(|raw| raw.as_str())
                .unwrap_or_default()
                .to_owned();
            table
                .decor_mut()
                .set_prefix(format!("{prefix}\n{}\n", comments.join("\n")));
        }
        tables.push(table);
    }
    if tables.is_empty() {
        document["remote"] = toml_edit::value(toml_edit::Array::new());
    } else {
        document["remote"] = Item::ArrayOfTables(tables);
    }
    // Parsed table positions otherwise restore the old array order on display.
    // Reindex descendants together so unknown sub-tables follow their record.
    fn position(table: &mut toml_edit::Table, next: &mut usize) {
        table.set_position(*next);
        *next += 1;
        for (_, item) in table.iter_mut() {
            match item {
                Item::Table(table) => position(table, next),
                Item::ArrayOfTables(tables) => {
                    for table in tables.iter_mut() {
                        position(table, next);
                    }
                }
                _ => {}
            }
        }
    }
    position(document.as_table_mut(), &mut 0);
    Ok(())
}
