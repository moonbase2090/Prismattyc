// SPDX-License-Identifier: MIT
//! The accepted serde model supplies structural discovery and tolerant loading.
//! Unknown names stay in the original TOML document, never in executable state.

use std::sync::OnceLock;

use serde_json::{Map, Value};

pub(crate) fn schema() -> &'static Value {
    static SCHEMA: OnceLock<schemars::Schema> = OnceLock::new();
    SCHEMA
        .get_or_init(|| schemars::schema_for!(crate::config::ConfigFile))
        .as_value()
}

pub(crate) fn resolve(mut node: &Value) -> &Value {
    while let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        node = schema()
            .pointer(reference.strip_prefix('#').expect("local schema reference"))
            .expect("derived schema reference exists");
    }
    node
}

pub(crate) fn member<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    let node = resolve(node);
    node.get(name).or_else(|| {
        ["anyOf", "oneOf", "allOf"].iter().find_map(|union| {
            node.get(union)
                .and_then(Value::as_array)?
                .iter()
                .find_map(|alternative| member(alternative, name))
        })
    })
}

pub(crate) fn properties(node: &Value) -> Option<&Map<String, Value>> {
    member(node, "properties").and_then(Value::as_object)
}

/// Locate an editable known field. Unknown future actions remain inert.
pub(crate) fn field(path: &[String]) -> Option<&'static Value> {
    let mut node = schema();
    for (index, segment) in path.iter().enumerate() {
        if index == 1 && path[0] == "keys" {
            crate::keybind::Action::from_name(segment)?;
            node = member(node, "additionalProperties")?;
        } else {
            node = properties(node)?.get(segment)?;
        }
    }
    Some(node)
}

/// Copy recognized fields only. Wrong values at a recognized name are copied
/// unchanged so production deserialization/validation still rejects them.
pub(crate) fn known(raw: toml::Value) -> (toml::Value, Vec<String>) {
    let mut warnings = Vec::new();
    let value = project(raw, schema(), "", &mut warnings);
    (value, warnings)
}

fn project(raw: toml::Value, node: &Value, path: &str, warnings: &mut Vec<String>) -> toml::Value {
    match raw {
        toml::Value::Table(table) if path == "keys" => toml::Value::Table(
            table
                .into_iter()
                .filter(|(key, _)| {
                    if crate::keybind::Action::from_name(key).is_some() {
                        true
                    } else {
                        warnings.push(format!(
                            "unknown config key {:?}; ignored",
                            format!("keys.{key}")
                        ));
                        false
                    }
                })
                .collect(),
        ),
        toml::Value::Table(table) => {
            let Some(fields) = properties(node) else {
                return toml::Value::Table(table);
            };
            let mut known = toml::map::Map::new();
            for (key, value) in table {
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if let Some(field) = fields.get(&key) {
                    known.insert(key, project(value, field, &field_path, warnings));
                } else if path != "mux" {
                    // mux has its existing specific warning wording.
                    warnings.push(format!("unknown config key {field_path:?}; ignored"));
                }
            }
            toml::Value::Table(known)
        }
        toml::Value::Array(values) => {
            let Some(items) = member(node, "items") else {
                return toml::Value::Array(values);
            };
            toml::Value::Array(
                values
                    .into_iter()
                    .map(|value| project(value, items, &format!("{path}[]"), warnings))
                    .collect(),
            )
        }
        other => other,
    }
}

/// Structural leaves used by independent control coverage. Collections retain
/// their element contract; dynamic action names come from the real action enum.
#[cfg(test)]
pub(crate) fn leaves() -> Vec<(String, Value)> {
    fn visit(node: &Value, path: &str, out: &mut Vec<(String, Value)>) {
        if path == "keys" {
            for action in crate::keybind::Action::all() {
                out.push((format!("keys.{}", action.name()), node.clone()));
            }
        } else if let Some(fields) = properties(node) {
            for (key, value) in fields {
                let field = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                visit(value, &field, out);
            }
        } else if let Some(items) =
            member(node, "items").filter(|items| properties(items).is_some())
        {
            visit(items, &format!("{path}[]"), out);
        } else {
            out.push((path.to_string(), node.clone()));
        }
    }
    let mut out = Vec::new();
    visit(schema(), "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_schema_exposes_nested_fields_and_numbered_actions() {
        let paths: std::collections::BTreeSet<_> =
            leaves().into_iter().map(|(path, _)| path).collect();
        for path in [
            "settings_panel",
            "space_rail_pane_names",
            "remote[].ssh",
            "theme_overrides.ansi",
            "a11y.announce",
            "keys.select_tab_9",
            "keys.layout_9",
        ] {
            assert!(paths.contains(path), "schema omitted {path}");
        }
        assert!(!paths.contains("resolved_theme"));
        assert!(!paths.contains("unknown_warnings"));
    }
}
