//! Persist session-name → agent_id across pmuxd restarts.
//!
//! Live uniqueness is enforced in [`crate::Domain`]. This file is the durable
//! map so a later recreate of the same session name can be rebound by the
//! operator with `--agent`. Default create still leaves `agent_id` unset.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use super::default_mail_db_path;

fn map_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("PMUX_SESSION_AGENTS") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    default_mail_db_path()
        .parent()
        .map(|parent| parent.join("session-agents.json"))
        .unwrap_or_else(|| PathBuf::from("session-agents.json"))
}

fn load() -> BTreeMap<String, String> {
    let path = map_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(map: &BTreeMap<String, String>) {
    let path = map_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(map) {
        let _ = fs::write(path, raw);
    }
}

/// Record that `session_name` is bound to `agent_id`.
pub fn bind(session_name: &str, agent_id: &str) {
    let mut map = load();
    map.retain(|_, bound| bound != agent_id);
    map.insert(session_name.to_string(), agent_id.to_string());
    save(&map);
}

/// Drop the durable mapping for `session_name`.
pub fn unbind(session_name: &str) {
    let mut map = load();
    if map.remove(session_name).is_some() {
        save(&map);
    }
}
