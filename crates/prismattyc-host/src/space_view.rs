//! Project daemon ownership into a window and the live pane-name rail.

use crate::attach_tabs::{AttachTabRecord, AttachTabsFile, AttachTabsMode};
use prismattyc_mux::{SavedSpace, Snapshot};
use std::collections::{HashMap, HashSet};

/// Resolve a restart cache before opening any subscriptions. Old numeric-only
/// Space caches fall back to the saved layout when their IDs no longer resolve.
pub fn restore_layout(
    cache: &AttachTabsFile,
    space: Option<&SavedSpace>,
    snapshot: &Snapshot,
) -> anyhow::Result<(AttachTabsFile, HashSet<String>)> {
    use anyhow::{bail, Context};
    if let Some(space) = space {
        let owner = space
            .id
            .as_deref()
            .context("saved Space needs an ownership migration; open it with pmux space open")?;
        if cache
            .space_id
            .as_deref()
            .is_some_and(|saved| saved != owner)
        {
            bail!("the saved Space was replaced; open the current Space explicitly");
        }
    }
    let owner = space.and_then(|space| space.id.as_deref());
    let permitted = |session: &&prismattyc_mux::SessionSnapshot| {
        owner.is_none_or(|owner| session.space_id.as_deref() == Some(owner))
    };
    let saved_name = |key: &str| -> Option<String> {
        if let Some(name) = cache.session_names.get(key) {
            return Some(name.clone());
        }
        if space.is_some_and(|space| space.sessions.iter().any(|session| session.name == key)) {
            return Some(key.to_string());
        }
        // Legacy numeric keys are usable only with verified live ownership.
        if owner.is_some() {
            if let Some(session) = snapshot
                .sessions
                .iter()
                .filter(permitted)
                .find(|session| session.id.to_string() == key)
            {
                return Some(session.name.clone());
            }
        }
        key.parse::<u64>().is_err().then(|| key.to_string())
    };
    let mut file = cache.clone();
    if cache
        .tabs
        .iter()
        .flat_map(|tab| &tab.sessions)
        .any(|key| saved_name(key).is_none())
    {
        let space = space
            .context("saved session IDs are stale; start fresh and attach sessions by name")?;
        file.tabs = if space.tabs.is_empty() {
            space
                .sessions
                .iter()
                .map(|session| AttachTabRecord {
                    title: session.name.clone(),
                    sessions: vec![session.name.clone()],
                })
                .collect()
        } else {
            space
                .tabs
                .iter()
                .map(|tab| AttachTabRecord {
                    title: tab.title.clone(),
                    sessions: tab.sessions.clone(),
                })
                .collect()
        };
        file.active_tab = space.active_tab;
        file.focused_session = space.focused_session.clone();
        file.session_names.clear();
    } else {
        for key in file
            .tabs
            .iter_mut()
            .flat_map(|tab| &mut tab.sessions)
            .chain(file.focused_session.iter_mut())
        {
            *key = saved_name(key).context("saved focus cannot be resolved")?;
        }
    }
    let mut stopped = HashSet::new();
    let mut names = std::collections::BTreeMap::new();
    for key in file
        .tabs
        .iter_mut()
        .flat_map(|tab| &mut tab.sessions)
        .chain(file.focused_session.iter_mut())
    {
        let name = key.clone();
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.name == name)
        {
            if !permitted(&session) {
                bail!("session {name:?} belongs to another Space; restore was not applied");
            }
            *key = session.id.to_string();
        } else {
            if space.is_some_and(|space| !space.sessions.iter().any(|session| session.name == name))
            {
                bail!("session {name:?} is no longer saved in this Space");
            }
            stopped.insert(name.clone());
        }
        names.insert(key.clone(), name);
    }
    file.session_names = names;
    file.space_id = owner.map(str::to_string);
    file.mode = AttachTabsMode::Switch;
    Ok((file, stopped))
}

/// Names can change or be reused. An open window keeps its stable owner.
pub fn resolve_space(
    dir: &std::path::Path,
    name: &str,
    owner: Option<&str>,
) -> Option<(String, SavedSpace)> {
    if let Ok(space) = prismattyc_mux::load_space(dir, name) {
        if owner.is_none() || space.id.as_deref() == owner {
            return Some((name.to_string(), space));
        }
    }
    let owner = owner?;
    prismattyc_mux::list_spaces(dir)
        .ok()?
        .into_iter()
        .find_map(|entry| {
            let space = prismattyc_mux::load_space(dir, &entry.name).ok()?;
            (space.id.as_deref() == Some(owner)).then_some((entry.name, space))
        })
}

/// Show the session identity even when its panes have no optional title.
/// One session appears once, including when its mux layout has several panes.
pub fn pane_names(space: &SavedSpace, snapshot: &Snapshot) -> Vec<String> {
    let Some(owner) = space.id.as_deref() else {
        return Vec::new();
    };
    let order = prismattyc_mux::space_sessions_in_tab_order(space);
    let mut names: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id.as_deref() == Some(owner))
        .filter(|session| {
            session
                .windows
                .iter()
                .flat_map(|window| &window.panes)
                .any(|pane| pane.child_pid.is_some())
        })
        .map(|session| session.name.clone())
        .collect();
    names.sort_by_key(|name| {
        (
            order
                .iter()
                .position(|saved| saved == name)
                .unwrap_or(usize::MAX),
            name.clone(),
        )
    });
    names
}

/// Human-facing title for a newly visible session. Generated identity stays
/// in the ownership map and never becomes the default tab label.
pub fn session_title(space: &SavedSpace, session: &prismattyc_mux::SessionSnapshot) -> String {
    space
        .tabs
        .iter()
        .find(|tab| tab.sessions.contains(&session.name))
        .map(|tab| tab.title.as_str())
        .filter(|title| !title.trim().is_empty() && *title != session.name)
        .or_else(|| {
            session
                .windows
                .iter()
                .flat_map(|window| &window.panes)
                .map(|pane| pane.title.as_str())
                .find(|title| !title.trim().is_empty())
        })
        .or(session.agent_id.as_deref())
        .unwrap_or("shell")
        .to_string()
}

/// Keep this window's grouping and focus. Add new owned sessions and remove
/// foreign sessions, even when saved launch metadata has not yet caught up.
pub fn owned_layout(
    name: &str,
    space: &SavedSpace,
    snapshot: &Snapshot,
    current: &AttachTabsFile,
) -> AttachTabsFile {
    owned_layout_with_local_tabs(name, space, snapshot, current, &HashSet::new())
}

pub fn owned_layout_with_local_tabs(
    name: &str,
    space: &SavedSpace,
    snapshot: &Snapshot,
    current: &AttachTabsFile,
    local_tabs: &HashSet<usize>,
) -> AttachTabsFile {
    let owned: HashMap<_, _> = snapshot
        .sessions
        .iter()
        .filter(|session| session.space_id.is_some() && session.space_id == space.id)
        .map(|session| (session.id.to_string(), session))
        .collect();
    // An exited saved session remains a named placeholder until you reopen it.
    // A live name or numeric ID still requires the correct Space owner.
    let resolve = |key: &str| {
        if owned.contains_key(key) {
            return Some(key.to_string());
        }
        if let Some(session) = owned.values().find(|session| session.name == key) {
            return Some(session.id.to_string());
        }
        (space.id.is_some()
            && space.sessions.iter().any(|session| session.name == key)
            && !snapshot
                .sessions
                .iter()
                .any(|session| session.name == key || session.id.to_string() == key))
        .then(|| key.to_string())
    };
    let mut kept = HashSet::new();
    let selected = current
        .tabs
        .get(current.active_tab)
        .and_then(|tab| tab.sessions.first())
        .and_then(|key| resolve(key));
    let mut retained_indices = Vec::new();
    let mut tabs: Vec<_> = current
        .tabs
        .iter()
        .enumerate()
        .filter_map(|(index, tab)| {
            let sessions: Vec<_> = tab
                .sessions
                .iter()
                .filter_map(|key| resolve(key))
                .filter(|id| kept.insert(id.clone()))
                .collect();
            if sessions.is_empty() && !local_tabs.contains(&index) {
                return None;
            }
            retained_indices.push(index);
            Some(AttachTabRecord {
                title: tab.title.clone(),
                sessions,
            })
        })
        .collect();
    for session in &snapshot.sessions {
        let id = session.id.to_string();
        if owned.contains_key(&id) && kept.insert(id.clone()) {
            tabs.push(AttachTabRecord {
                title: session_title(space, session),
                sessions: vec![id],
            });
        }
    }
    let active_tab = selected
        .and_then(|id| tabs.iter().position(|tab| tab.sessions.contains(&id)))
        .or_else(|| {
            retained_indices
                .iter()
                .position(|index| *index == current.active_tab)
        })
        .unwrap_or(0);
    let focused_session = current
        .focused_session
        .as_ref()
        .and_then(|key| resolve(key));
    AttachTabsFile {
        tabs,
        active_tab,
        focused_session,
        space: Some(name.into()),
        mode: AttachTabsMode::Switch,
        ..Default::default()
    }
}

pub fn permits_layout(space: &SavedSpace, snapshot: &Snapshot, layout: &AttachTabsFile) -> bool {
    let Some(owner) = space.id.as_deref() else {
        return false;
    };
    layout.tabs.iter().flat_map(|tab| &tab.sessions).all(|id| {
        snapshot.sessions.iter().any(|session| {
            session.id.to_string() == *id && session.space_id.as_deref() == Some(owner)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_mux::SessionSnapshot;

    fn snapshot() -> Snapshot {
        Snapshot {
            sequence: 1,
            sessions: [(1, "a"), (2, "b"), (3, "a")]
                .into_iter()
                .map(|(id, owner)| SessionSnapshot {
                    id,
                    name: format!("session-{id}"),
                    agent_id: None,
                    space_id: Some(owner.into()),
                    windows: vec![],
                })
                .collect(),
        }
    }
    fn layout(ids: &[&str]) -> AttachTabsFile {
        AttachTabsFile {
            tabs: ids
                .iter()
                .map(|id| AttachTabRecord {
                    title: format!("tab-{id}"),
                    sessions: vec![(*id).into()],
                })
                .collect(),
            active_tab: 1,
            focused_session: Some("3".into()),
            space: Some("a".into()),
            mode: AttachTabsMode::Switch,
            ..Default::default()
        }
    }

    fn saved_space() -> SavedSpace {
        serde_json::from_value(serde_json::json!({
            "version": 2, "id": "a", "saved_at_unix": 0,
            "sessions": [{"name": "session-1", "windows": []}, {"name": "session-3", "windows": []}],
            "tabs": [{"title": "First", "sessions": ["session-1"]}, {"title": "Last", "sessions": ["session-3"]}],
            "active_tab": 1, "focused_session": "session-3"
        })).unwrap()
    }

    #[test]
    fn rail_names_use_live_sessions_without_requiring_pane_titles() {
        let mut live = snapshot();
        for session in &mut live.sessions {
            session.windows = serde_json::from_value(serde_json::json!([{
                "id": session.id, "title": "shell", "bounds": {"window_id":session.id,"cols":80,"rows":24},
                "layout": {"kind":"leaf", "pane_id":session.id},
                "panes": [{"id":session.id,"title":"","child_pid":123,
                    "geometry":{"pane_id":session.id,"col":0,"row":0,"cols":80,"rows":24},
                    "ledger":{"focused":false,"last_write_ended_with_cr":true,"dirty_input":false}}]
            }])).unwrap();
        }
        assert_eq!(
            pane_names(&saved_space(), &live),
            ["session-1", "session-3"]
        );
        live.sessions[0].name = "renamed".into();
        let duplicate = live.sessions[0].windows[0].panes[0].clone();
        live.sessions[0].windows[0].panes.push(duplicate);
        assert_eq!(pane_names(&saved_space(), &live), ["session-3", "renamed"]);
        live.sessions[0].space_id = Some("b".into());
        live.sessions[2].windows[0].panes[0].child_pid = None;
        assert!(pane_names(&saved_space(), &live).is_empty());
    }

    #[test]
    fn restart_resolves_names_before_recycled_ids_and_preserves_focus() {
        let mut cache = layout(&["1", "3"]);
        cache.space_id = Some("a".into());
        cache.session_names = [
            ("1".into(), "session-1".into()),
            ("3".into(), "session-3".into()),
        ]
        .into();
        let mut live = snapshot();
        live.sessions[0].id = 30;
        live.sessions[2].id = 10;
        live.sessions[1].id = 1; // The old ID now belongs to a foreign Space.
        let (restored, stopped) = restore_layout(&cache, Some(&saved_space()), &live).unwrap();
        assert!(stopped.is_empty());
        assert_eq!(restored.tabs[0].sessions, ["30"]);
        assert_eq!(restored.tabs[1].sessions, ["10"]);
        assert_eq!(restored.focused_session.as_deref(), Some("10"));
        assert_eq!(restored.active_tab, 1);
        assert_eq!(restored.tabs[0].title, "tab-1");
    }

    #[test]
    fn legacy_cache_recovers_saved_names_when_old_ids_are_missing_or_foreign() {
        let cache = layout(&["1", "3"]);
        for live in [
            Snapshot {
                sequence: 0,
                sessions: vec![],
            },
            Snapshot {
                sequence: 1,
                sessions: snapshot()
                    .sessions
                    .into_iter()
                    .filter(|s| s.space_id.as_deref() == Some("b"))
                    .map(|mut s| {
                        s.id = 1;
                        s
                    })
                    .collect(),
            },
        ] {
            let (restored, stopped) = restore_layout(&cache, Some(&saved_space()), &live).unwrap();
            assert_eq!(stopped.len(), 2);
            assert_eq!(restored.tabs[1].sessions, ["session-3"]);
            assert_eq!(restored.focused_session.as_deref(), Some("session-3"));
            assert_eq!(restored.active_tab, 1);
            assert!(restore_layout(&cache, None, &live).is_err());
        }
    }

    #[test]
    fn restore_rejects_replaced_space_and_foreign_session_name() {
        let mut cache = layout(&["1", "3"]);
        cache.space_id = Some("old-owner".into());
        assert!(restore_layout(&cache, Some(&saved_space()), &snapshot())
            .unwrap_err()
            .to_string()
            .contains("replaced"));
        cache.space_id = Some("a".into());
        cache.session_names = [
            ("1".into(), "session-1".into()),
            ("3".into(), "session-3".into()),
        ]
        .into();
        let mut live = snapshot();
        live.sessions[2].space_id = Some("b".into());
        assert!(restore_layout(&cache, Some(&saved_space()), &live)
            .unwrap_err()
            .to_string()
            .contains("another Space"));
    }
    #[test]
    fn saved_exits_keep_focus_but_cannot_retain_a_foreign_live_name() {
        let space: SavedSpace = serde_json::from_value(serde_json::json!({
            "version": 2, "id": "a", "saved_at_unix": 0,
            "sessions": [{"name": "exited", "windows": []}, {"name": "session-2", "windows": []}]
        }))
        .unwrap();
        let mut current = layout(&["exited", "session-2"]);
        current.active_tab = 0;
        current.focused_session = Some("exited".into());
        let desired = owned_layout("work", &space, &snapshot(), &current);
        assert_eq!(desired.tabs[0].sessions, ["exited"]);
        assert_eq!(desired.focused_session.as_deref(), Some("exited"));
        assert_eq!(desired.active_tab, 0);
        assert!(!desired
            .tabs
            .iter()
            .any(|tab| tab.sessions.contains(&"session-2".into())));
    }

    #[test]
    fn layout_requires_matching_owner_for_every_live_identity() {
        let mut space: SavedSpace = serde_json::from_value(
            serde_json::json!({"version": 2, "id": "a", "saved_at_unix": 0, "sessions": []}),
        )
        .unwrap();
        assert!(permits_layout(&space, &snapshot(), &layout(&["1", "3"])));
        for invalid in ["2", "99", "session-1"] {
            assert!(!permits_layout(
                &space,
                &snapshot(),
                &layout(&["1", invalid])
            ));
        }
        space.id = None;
        assert!(!permits_layout(&space, &snapshot(), &layout(&["1"])));
    }
    #[test]
    fn transfers_project_once_without_foreign_members_and_keep_focus() {
        let mut live = snapshot();
        let current = layout(&["1", "3", "2"]);
        let space: SavedSpace = serde_json::from_value(
            serde_json::json!({"version": 2, "id": "a", "saved_at_unix": 0, "sessions": []}),
        )
        .unwrap();
        let desired = owned_layout("a", &space, &live, &current);
        assert_eq!(desired.tabs, current.tabs[..2]);
        assert_eq!(desired.active_tab, 1);
        assert_eq!(desired.focused_session.as_deref(), Some("3"));
        live.sessions[1].space_id = Some("a".into());
        let added = owned_layout("a", &space, &live, &desired);
        assert_eq!(added.tabs.len(), 3);
        assert_eq!(added.tabs[2].sessions, ["2"]);
        for session in &mut live.sessions {
            session.space_id = Some("b".into());
        }
        let empty = owned_layout("a", &space, &live, &added);
        assert!(empty.tabs.is_empty());
        assert!(empty.focused_session.is_none());
    }
}
