//! Host attach-tabs cache: `{stem}.attach-tabs.json` beside the mux socket.
//!
//! Contract: [docs/mux-cli.md](../../../docs/mux-cli.md) (Where things live).
//! `save` writes a temp file in the same directory and renames it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One host tab: title plus mux session ids in pane order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachTabRecord {
    pub title: String,
    pub sessions: Vec<String>,
}

/// Serde skip for default tab index 0.
pub fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// How the host applies this file (PT-213). Missing / `add` keeps today's
/// merge: attached sessions absent from the file stay. `switch` detaches
/// those host panes; the mux sessions stay alive in pmuxd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttachTabsMode {
    #[default]
    Add,
    Switch,
}

impl AttachTabsMode {
    #[must_use]
    pub fn is_add(self) -> bool {
        matches!(self, Self::Add)
    }
}

fn mode_is_add(mode: &AttachTabsMode) -> bool {
    mode.is_add()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttachTabsFile {
    pub tabs: Vec<AttachTabRecord>,
    /// Index into `tabs`. Missing or 0 means the first tab.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_tab: usize,
    /// Mux session id of the focused host pane. Missing means the first
    /// session of `active_tab`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session: Option<String>,
    /// Saved space this arrangement came from (PT-91). `pmux space open`
    /// writes it; the host keeps it across its own rewrites so the spaces
    /// rail knows which chip is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Open mode. Old writers omit it; that is `add`.
    #[serde(default, skip_serializing_if = "mode_is_add")]
    pub mode: AttachTabsMode,
    /// Stable names for cached daemon-local IDs. Restore must not trust an
    /// ID after the daemon restarts and reuses its counters.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub session_names: std::collections::BTreeMap<String, String>,
    /// Owner at save time. A different Space may later reuse the file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
}

/// `{stem}.attach-tabs.json` beside the control socket.
#[must_use]
pub fn layout_path_from_socket(socket: &Path) -> PathBuf {
    let stem = socket.file_stem().map_or_else(
        || std::ffi::OsString::from("prism"),
        std::ffi::OsStr::to_os_string,
    );
    let mut file = stem;
    file.push(".attach-tabs.json");
    match socket.parent() {
        Some(dir) => dir.join(file),
        None => PathBuf::from(file),
    }
}

pub fn load(path: &Path) -> Option<AttachTabsFile> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save(path: &Path, file: &AttachTabsFile) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = tmp_path(path);
    fs::write(&tmp, &bytes)?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Keep non-empty mapped tabs. Remap `selected` (input index) onto the
/// kept list. If that tab was dropped, use the last kept tab before it,
/// else 0.
pub fn remap_tabs<T, S>(
    tabs: impl IntoIterator<Item = T>,
    selected: usize,
    mut map: impl FnMut(T) -> Option<(String, Vec<S>)>,
) -> (Vec<(String, Vec<S>)>, usize) {
    let mut out = Vec::new();
    let mut active = 0;
    let mut last_before = 0;
    let mut saw = false;
    for (i, tab) in tabs.into_iter().enumerate() {
        let Some((title, sessions)) = map(tab) else {
            continue;
        };
        if sessions.is_empty() {
            continue;
        }
        let kept = out.len();
        if i < selected {
            last_before = kept;
        }
        if i == selected {
            active = kept;
            saw = true;
        }
        out.push((title, sessions));
    }
    if !saw {
        active = if out.is_empty() {
            0
        } else {
            last_before.min(out.len() - 1)
        };
    }
    (out, active)
}

/// Plan to make live tabs match `file`. Never includes a close.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegroupDiff {
    /// Live session that must move to this file tab index.
    pub moves: Vec<(String, usize)>,
    /// File session that is not live and must be attached into this tab.
    pub attaches: Vec<(String, usize)>,
    /// Live sessions that the file does not name. Keep them.
    pub keep: Vec<String>,
}

impl RegroupDiff {
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.moves.is_empty() && self.attaches.is_empty()
    }
}

/// Diff live tab membership against the attach-tabs file.
///
/// Live extra sessions are `keep`. File sessions on the wrong live tab are
/// `moves`. File sessions with no live pane are `attaches`.
#[must_use]
pub fn regroup_diff(live: &[(String, Vec<String>)], file: &AttachTabsFile) -> RegroupDiff {
    let mut live_at: Vec<(String, usize)> = Vec::new();
    for (tab_index, (_title, sessions)) in live.iter().enumerate() {
        for session in sessions {
            live_at.push((session.clone(), tab_index));
        }
    }
    let mut file_at = Vec::new();
    for (tab_index, tab) in file.tabs.iter().enumerate() {
        for session in &tab.sessions {
            file_at.push((session.clone(), tab_index));
        }
    }
    let mut diff = RegroupDiff::default();
    for (session, dest) in &file_at {
        match live_at
            .iter()
            .find(|(id, _)| id == session)
            .map(|(_, src)| *src)
        {
            Some(src) if src != *dest => diff.moves.push((session.clone(), *dest)),
            Some(_) => {}
            None => diff.attaches.push((session.clone(), *dest)),
        }
    }
    for (session, _) in &live_at {
        if !file_at.iter().any(|(id, _)| id == session) {
            diff.keep.push(session.clone());
        }
    }
    diff
}

/// One-line stdout after `pmux space open` writes the cache (PT-213).
#[must_use]
pub fn space_open_report(
    mode: AttachTabsMode,
    from: Option<&str>,
    to: &str,
    detached: usize,
    opened_sessions: usize,
    opened_tabs: usize,
) -> String {
    match mode {
        AttachTabsMode::Add => {
            let holds = match from.filter(|name| *name != to) {
                Some(from) => format!("{from} + {to}"),
                None => to.to_string(),
            };
            format!("added {to} to the window: it now holds {holds}")
        }
        AttachTabsMode::Switch => {
            let tabs = if opened_tabs == 1 { "tab" } else { "tabs" };
            match from.filter(|name| *name != to) {
                Some(from) => format!(
                    "switched from {from} to {to}: detaching {detached} panes, opened {opened_sessions} sessions in {opened_tabs} {tabs}"
                ),
                None => format!(
                    "switched to {to}: opened {opened_sessions} sessions in {opened_tabs} {tabs}"
                ),
            }
        }
    }
}

/// Warn when a no-list `space save` would fold more than one space (PT-213).
///
/// The live window is the source of truth. Two space files that share the
/// same seats do not warn. Warn when a live attached session is not a
/// member of `cache_space`, or when `cache_space` is missing and the live
/// sessions map to more than one file.
#[must_use]
pub fn space_save_span_warning(
    cache_space: Option<&str>,
    live_sessions: &[String],
    spaces: &[(String, Vec<String>)],
) -> Option<String> {
    fn warn(a: &str, b: &str) -> String {
        format!(
            "warning: this window spans {a} and {b}. Save a subset with `pmux space save NAME S1 S2`."
        )
    }
    match cache_space {
        Some(current) => {
            let members = spaces
                .iter()
                .find(|(name, _)| name == current)
                .map(|(_, members)| members.as_slice())
                .unwrap_or(&[]);
            let foreign: Vec<&String> = live_sessions
                .iter()
                .filter(|session| !members.contains(session))
                .collect();
            if foreign.is_empty() {
                return None;
            }
            let other = spaces.iter().find_map(|(name, members)| {
                (name != current && foreign.iter().any(|session| members.contains(session)))
                    .then_some(name.as_str())
            });
            Some(warn(current, other.unwrap_or("another space")))
        }
        None => {
            let mut names: Vec<String> = Vec::new();
            for (name, members) in spaces {
                if members
                    .iter()
                    .any(|session| live_sessions.contains(session))
                    && !names.contains(name)
                {
                    names.push(name.clone());
                }
            }
            (names.len() >= 2).then(|| warn(&names[0], &names[1]))
        }
    }
}

/// Union for `space open --add`: keep previous tabs that still have live
/// sessions not named by `incoming`, then append `incoming` tabs.
#[must_use]
pub fn merge_add_cache(
    previous: &AttachTabsFile,
    incoming: AttachTabsFile,
    live_ids: &[String],
) -> AttachTabsFile {
    let incoming_ids: Vec<String> = incoming
        .tabs
        .iter()
        .flat_map(|tab| tab.sessions.iter().cloned())
        .collect();
    let mut tabs = Vec::new();
    for tab in &previous.tabs {
        let sessions: Vec<String> = tab
            .sessions
            .iter()
            .filter(|id| live_ids.contains(id) && !incoming_ids.contains(id))
            .cloned()
            .collect();
        if !sessions.is_empty() {
            tabs.push(AttachTabRecord {
                title: tab.title.clone(),
                sessions,
            });
        }
    }
    let kept = tabs.len();
    let active_tab = kept.saturating_add(incoming.active_tab);
    let focused_session = incoming.focused_session.clone();
    let mut session_names = previous.session_names.clone();
    session_names.extend(incoming.session_names);
    tabs.extend(incoming.tabs);
    AttachTabsFile {
        tabs,
        active_tab,
        focused_session,
        space: incoming.space,
        mode: AttachTabsMode::Add,
        session_names,
        space_id: incoming.space_id,
    }
}

/// Session names in the live window, cache tab order. Skip unknown ids.
#[must_use]
pub fn cache_sessions_in_tab_order(
    file: &AttachTabsFile,
    names: &[(String, String)],
) -> Vec<String> {
    let mut out = Vec::new();
    for tab in &file.tabs {
        for id in &tab.sessions {
            let Some((_, name)) = names.iter().find(|(got, _)| got == id) else {
                continue;
            };
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// Place a host seat in the attach-tabs cache (PT-306).
///
/// A missing or empty cache becomes one tab named `title`. An existing
/// session is focused. A new session joins the active tab so a bare host
/// pane stays the dummy that regroup closes after the log-replica split.
#[must_use]
pub fn plan_host_seat_cache(
    existing: Option<AttachTabsFile>,
    session_id: &str,
    title: &str,
) -> AttachTabsFile {
    let mut file = existing.unwrap_or_default();
    file.mode = AttachTabsMode::Add;
    if session_id.is_empty() {
        return file;
    }
    if let Some(index) = file
        .tabs
        .iter()
        .position(|tab| tab.sessions.iter().any(|session| session == session_id))
    {
        file.active_tab = index;
        file.focused_session = Some(session_id.to_string());
        return file;
    }
    if file.tabs.is_empty() {
        file.tabs.push(AttachTabRecord {
            title: title.to_string(),
            sessions: vec![session_id.to_string()],
        });
        file.active_tab = 0;
    } else {
        let index = file.active_tab.min(file.tabs.len().saturating_sub(1));
        file.tabs[index].sessions.push(session_id.to_string());
        if file.tabs[index].title.trim().is_empty() {
            file.tabs[index].title = title.to_string();
        }
        file.active_tab = index;
    }
    file.focused_session = Some(session_id.to_string());
    file
}

/// Keep the caller's attach session in the switch file so regroup does
/// not detach the pane that ran `pmux space open`. Returns true when a
/// tab was appended.
pub fn keep_caller_session(file: &mut AttachTabsFile, caller_id: &str, caller_name: &str) -> bool {
    if caller_id.is_empty()
        || file
            .tabs
            .iter()
            .any(|tab| tab.sessions.iter().any(|session| session == caller_id))
    {
        return false;
    }
    file.tabs.push(AttachTabRecord {
        title: caller_name.to_string(),
        sessions: vec![caller_id.to_string()],
    });
    true
}

/// Write `file` when it differs from `last`. Returns whether a write ran.
pub fn persist_if_changed(
    path: &Path,
    last: &mut Option<AttachTabsFile>,
    file: AttachTabsFile,
) -> std::io::Result<bool> {
    if last.as_ref() == Some(&file) {
        return Ok(false);
    }
    save(path, &file)?;
    *last = Some(file);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn layout_path_sits_beside_the_socket() {
        let path = layout_path_from_socket(Path::new("/run/user/1000/prismattyc/pmux.sock"));
        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/prismattyc/pmux.attach-tabs.json")
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pmux-attach-tabs-{}", std::process::id()));
        let path = dir.join("pmux.attach-tabs.json");
        let file = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "seats".into(),
                sessions: vec!["2".into(), "3".into()],
            }],
            ..Default::default()
        };
        save(&path, &file).unwrap();
        assert_eq!(load(&path).as_ref(), Some(&file));
        assert!(
            !tmp_path(&path).exists(),
            "atomic save must not leave a tmp file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remap_tabs_drops_empty_and_remaps_active() {
        let input = [("A", vec!["1"]), ("B", vec!["2"]), ("C", vec!["3"])];
        let (kept, active) = remap_tabs(input, 2, |(title, sessions)| {
            if title == "A" {
                None
            } else {
                Some((
                    title.to_string(),
                    sessions.into_iter().map(str::to_string).collect(),
                ))
            }
        });
        assert_eq!(
            active, 1,
            "C was file index 2, kept index 1 after dropping A"
        );
        assert_eq!(kept[0].0, "B");
        assert_eq!(kept[1].0, "C");
        let input = [
            ("A", vec!["1"]),
            ("shell", Vec::<&str>::new()),
            ("B", vec!["2"]),
        ];
        let (kept, active) = remap_tabs(input, 1, |(title, sessions)| {
            let sessions: Vec<String> = sessions.into_iter().map(str::to_string).collect();
            (!sessions.is_empty()).then_some((title.to_string(), sessions))
        });
        assert_eq!(kept.len(), 2);
        assert_eq!(active, 0, "shell tab drops; nearest kept before it is A");
    }

    #[test]
    fn regroup_diff_moves_attaches_and_never_closes() {
        let live = vec![
            ("A".into(), vec!["1".into(), "2".into()]),
            ("B".into(), vec!["3".into()]),
        ];
        let file = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "work".into(),
                    sessions: vec!["1".into(), "3".into()],
                },
                AttachTabRecord {
                    title: "mail".into(),
                    sessions: vec!["2".into()],
                },
            ],
            ..Default::default()
        };
        let diff = regroup_diff(&live, &file);
        assert_eq!(
            diff.moves,
            vec![("3".into(), 0), ("2".into(), 1)],
            "3 leaves B for work; 2 leaves A for mail"
        );
        assert!(diff.attaches.is_empty());
        assert!(diff.keep.is_empty());
        assert!(!diff.is_noop());

        let already = vec![
            ("work".into(), vec!["1".into(), "3".into()]),
            ("mail".into(), vec!["2".into()]),
        ];
        assert!(
            regroup_diff(&already, &file).is_noop(),
            "identical grouping is a no-op"
        );

        let with_keep = vec![
            ("work".into(), vec!["1".into(), "3".into(), "9".into()]),
            ("mail".into(), vec!["2".into()]),
        ];
        let keep = regroup_diff(&with_keep, &file);
        assert!(keep.moves.is_empty() && keep.attaches.is_empty());
        assert_eq!(keep.keep, vec!["9".to_string()]);

        let missing = vec![("A".into(), vec!["1".into()])];
        let need_attach = regroup_diff(&missing, &file);
        assert_eq!(need_attach.attaches, vec![("3".into(), 0), ("2".into(), 1)]);
        assert!(
            need_attach.moves.is_empty(),
            "session 1 is already on tab 0"
        );
        assert!(need_attach.keep.is_empty());
    }

    #[test]
    fn persist_if_changed_skips_identical_then_writes_a_diff() {
        let dir = std::env::temp_dir().join(format!("pmux-attach-tabs-chg-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("pmux.attach-tabs.json");
        let a = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "a".into(),
                sessions: vec!["2".into()],
            }],
            ..Default::default()
        };
        let b = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "b".into(),
                sessions: vec!["3".into()],
            }],
            active_tab: 0,
            focused_session: Some("3".into()),
            space: None,
            mode: AttachTabsMode::Add,
            ..Default::default()
        };
        let mut last = None;
        assert!(persist_if_changed(&path, &mut last, a.clone()).unwrap());
        assert_eq!(load(&path).as_ref(), Some(&a));
        assert!(!persist_if_changed(&path, &mut last, a.clone()).unwrap());
        assert_eq!(load(&path).as_ref(), Some(&a));
        assert!(persist_if_changed(&path, &mut last, b.clone()).unwrap());
        assert_eq!(load(&path).as_ref(), Some(&b));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_mode_deserializes_as_add_and_switch_round_trips() {
        let add: AttachTabsFile = serde_json::from_str(r#"{"tabs":[]}"#).unwrap();
        assert_eq!(add.mode, AttachTabsMode::Add);
        let switch = AttachTabsFile {
            mode: AttachTabsMode::Switch,
            space: Some("web".into()),
            ..Default::default()
        };
        let raw = serde_json::to_string(&switch).unwrap();
        assert!(raw.contains("\"switch\""));
        assert!(!raw.contains("\"add\""));
        let loaded: AttachTabsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(loaded.mode, AttachTabsMode::Switch);
        assert_eq!(loaded.space.as_deref(), Some("web"));
    }

    #[test]
    fn space_open_report_names_switch_and_add() {
        assert_eq!(
            space_open_report(AttachTabsMode::Switch, Some("prismattyc"), "web", 3, 2, 1),
            "switched from prismattyc to web: detaching 3 panes, opened 2 sessions in 1 tab"
        );
        assert_eq!(
            space_open_report(AttachTabsMode::Add, Some("prismattyc"), "web", 0, 2, 1),
            "added web to the window: it now holds prismattyc + web"
        );
        assert_eq!(
            space_open_report(AttachTabsMode::Switch, None, "web", 0, 2, 1),
            "switched to web: opened 2 sessions in 1 tab"
        );
    }

    #[test]
    fn space_save_span_warning_true_positive_and_shared_seat_false_positive() {
        let mixed = vec!["a".into(), "b".into()];
        let split = vec![
            ("prismattyc".into(), vec!["a".into()]),
            ("web".into(), vec!["b".into()]),
        ];
        let text = space_save_span_warning(Some("prismattyc"), &mixed, &split).unwrap();
        assert!(text.contains("prismattyc") && text.contains("web"));
        assert!(text.contains("pmux space save NAME S1 S2"));

        let shared = vec![
            ("prismattyc".into(), vec!["a".into(), "b".into()]),
            ("web".into(), vec!["a".into(), "b".into()]),
        ];
        assert!(
            space_save_span_warning(Some("prismattyc"), &mixed, &shared).is_none(),
            "two files of the same seats must not warn"
        );
        assert!(
            space_save_span_warning(None, &mixed, &split).is_some(),
            "no cache space and two covering files must warn"
        );
        assert!(space_save_span_warning(
            Some("prismattyc"),
            &["a".into()],
            &[("prismattyc".into(), vec!["a".into()])]
        )
        .is_none());
    }

    #[test]
    fn plan_host_seat_cache_replaces_a_bare_host_and_focuses_a_live_seat() {
        let first = plan_host_seat_cache(None, "5", "astra-pc");
        assert_eq!(first.tabs.len(), 1);
        assert_eq!(first.tabs[0].title, "astra-pc");
        assert_eq!(first.tabs[0].sessions, ["5"]);
        assert_eq!(first.active_tab, 0);
        assert_eq!(first.focused_session.as_deref(), Some("5"));
        assert_eq!(first.mode, AttachTabsMode::Add);

        let focused = plan_host_seat_cache(Some(first.clone()), "5", "astra-pc");
        assert_eq!(focused.tabs, first.tabs);
        assert_eq!(focused.focused_session.as_deref(), Some("5"));

        let added = plan_host_seat_cache(Some(first), "9", "grok-pc");
        assert_eq!(added.tabs[0].sessions, ["5", "9"]);
        assert_eq!(added.focused_session.as_deref(), Some("9"));
        assert_eq!(added.active_tab, 0);
        assert!(plan_host_seat_cache(None, "", "x").tabs.is_empty());
    }

    #[test]
    fn keep_caller_session_appends_a_tab_once() {
        let mut file = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "web".into(),
                sessions: vec!["7".into()],
            }],
            mode: AttachTabsMode::Switch,
            ..Default::default()
        };
        assert!(keep_caller_session(&mut file, "3", "fable-pc"));
        assert_eq!(file.tabs.len(), 2);
        assert_eq!(file.tabs[1].title, "fable-pc");
        assert_eq!(file.tabs[1].sessions, ["3"]);
        assert!(!keep_caller_session(&mut file, "3", "fable-pc"));
        assert_eq!(file.tabs.len(), 2);
        assert!(!keep_caller_session(&mut file, "", "x"));
    }

    #[test]
    fn merge_add_cache_keeps_previous_live_tabs_then_appends() {
        let previous = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "beta".into(),
                sessions: vec!["c".into(), "gone".into()],
            }],
            space: Some("beta".into()),
            mode: AttachTabsMode::Switch,
            ..Default::default()
        };
        let incoming = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "a".into(),
                    sessions: vec!["a".into()],
                },
                AttachTabRecord {
                    title: "b".into(),
                    sessions: vec!["b".into()],
                },
            ],
            active_tab: 1,
            focused_session: Some("b".into()),
            space: Some("alpha".into()),
            ..Default::default()
        };
        let merged = merge_add_cache(&previous, incoming, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(merged.tabs.len(), 3);
        assert_eq!(merged.tabs[0].sessions, ["c"]);
        assert_eq!(merged.tabs[1].sessions, ["a"]);
        assert_eq!(merged.tabs[2].sessions, ["b"]);
        assert_eq!(merged.active_tab, 2);
        assert_eq!(merged.focused_session.as_deref(), Some("b"));
        assert_eq!(merged.mode, AttachTabsMode::Add);
        let overlap = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "mixed".into(),
                sessions: vec!["a".into(), "c".into()],
            }],
            ..Default::default()
        };
        let incoming = AttachTabsFile {
            tabs: vec![AttachTabRecord {
                title: "a".into(),
                sessions: vec!["a".into()],
            }],
            ..Default::default()
        };
        let merged = merge_add_cache(&overlap, incoming, &["a".into(), "c".into()]);
        assert_eq!(merged.tabs[0].sessions, ["c"]);
        assert_eq!(merged.tabs[1].sessions, ["a"]);
    }

    #[test]
    fn cache_sessions_in_tab_order_skips_unknown_and_dedups() {
        let file = AttachTabsFile {
            tabs: vec![
                AttachTabRecord {
                    title: "seats".into(),
                    sessions: vec!["3".into(), "2".into()],
                },
                AttachTabRecord {
                    title: "solo".into(),
                    sessions: vec!["2".into(), "9".into()],
                },
            ],
            ..Default::default()
        };
        let names = [
            ("3".into(), "fable-pc".into()),
            ("2".into(), "grok-pc".into()),
        ];
        assert_eq!(
            cache_sessions_in_tab_order(&file, &names),
            ["fable-pc", "grok-pc"]
        );
    }
}
