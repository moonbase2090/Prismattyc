//! Space preferences, visible save state, and guarded undo of membership edits.
use super::*;

#[derive(Default)]
pub(super) struct State {
    pub failed: bool,
    last_poll: Option<Instant>,
    pub changed: Option<(String, Instant)>,
    pub undo: Option<PathBuf>,
}

pub(super) fn undo_label(host: &HostState) -> &'static str {
    if host.space_polish.undo.is_some() {
        "Restore the previous membership; keep processes running"
    } else {
        "No removal or move to undo in this window"
    }
}

pub(super) fn undo_path(host: &HostState) -> PathBuf {
    let base = host
        .attach_layout_path
        .clone()
        .unwrap_or_else(|| config::config_path().with_file_name("window"));
    base.with_extension(format!("undo-{}.json", std::process::id()))
}

pub(super) fn undo(host: &mut HostState) {
    let Some(path) = host.space_polish.undo.clone() else {
        rail_toast(host, " Nothing to undo ");
        return;
    };
    match run_pmux_space(&[
        "space".into(),
        "undo".into(),
        path.to_string_lossy().into_owned(),
    ]) {
        Ok(()) => {
            host.space_polish.undo = None;
            host.last_space_refresh = None;
            host.observed_space_sessions.clear();
            refresh_rail(host);
            rail_toast(host, " Membership restored; sessions kept running ");
        }
        Err(error) => rail_toast(host, &format!(" Could not undo: {error} ")),
    }
}

fn live_tabs(host: &HostState) -> Vec<(String, Vec<String>)> {
    let file = attach_records_from_live(host);
    file.tabs
        .iter()
        .map(|tab| {
            (
                tab.title.clone(),
                tab.sessions
                    .iter()
                    .map(|id| file.session_names.get(id).unwrap_or(id).clone())
                    .collect(),
            )
        })
        .collect()
}

fn saved_tabs(space: &prismattyc_mux::SavedSpace) -> Vec<(String, Vec<String>)> {
    if space.tabs.is_empty() {
        space
            .sessions
            .iter()
            .map(|s| {
                (
                    s.windows
                        .first()
                        .map(|w| w.title.clone())
                        .unwrap_or_else(|| s.name.clone()),
                    vec![s.name.clone()],
                )
            })
            .collect()
    } else {
        space
            .tabs
            .iter()
            .map(|t| (t.title.clone(), t.sessions.clone()))
            .collect()
    }
}

fn node_shape(node: &prismattyc_mux::SavedNode) -> serde_json::Value {
    use prismattyc_mux::SavedNode;
    match node {
        SavedNode::Leaf { title, .. } => serde_json::json!({"title":title}),
        SavedNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            serde_json::json!({"axis":axis,"ratio":ratio,"first":node_shape(first),"second":node_shape(second)})
        }
    }
}

fn saved_shapes(space: &prismattyc_mux::SavedSpace) -> (bool, String) {
    let Some(snapshot) = attach_log::live_snapshot() else {
        return (true, String::new());
    };
    let mut fingerprint = String::new();
    let mut matches = true;
    for saved in &space.sessions {
        let Some(live) = snapshot
            .sessions
            .iter()
            .find(|s| s.name == saved.name && s.space_id == space.id)
        else {
            continue;
        };
        let layout = prismattyc_mux::from_snapshot(live);
        let live_shape = layout
            .windows
            .iter()
            .map(|w| (&w.title, node_shape(&w.root)))
            .collect::<Vec<_>>();
        fingerprint.push_str(&format!("{}:{live_shape:?}", live.id));
        matches &= saved
            .windows
            .iter()
            .map(|w| (&w.title, node_shape(&w.root)))
            .collect::<Vec<_>>()
            == live_shape;
    }
    (matches, fingerprint)
}

pub(super) fn poll(host: &mut HostState) {
    let now = Instant::now();
    if host
        .space_polish
        .last_poll
        .is_some_and(|t| now.duration_since(t) < Duration::from_millis(400))
    {
        return;
    }
    host.space_polish.last_poll = Some(now);
    if host.restore_prompt.is_some() || host.space_opens.blocks_persist() {
        return;
    }
    let Some(name) = host.space_rail.current.clone() else {
        return;
    };
    let status = match load_space(&spaces_dir(), &name) {
        Ok(space) if space.id == host.mux.space_id => {
            let live = live_tabs(host);
            let (shapes_match, shapes) = saved_shapes(&space);
            if live == saved_tabs(&space) && shapes_match {
                host.space_polish.changed = None;
                if host.space_polish.failed {
                    "Save failed"
                } else {
                    "Saved"
                }
            } else {
                let fingerprint = format!("{name}:{live:?}:{shapes}");
                if host
                    .space_polish
                    .changed
                    .as_ref()
                    .is_none_or(|(old, _)| old != &fingerprint)
                {
                    host.space_polish.changed = Some((fingerprint, now));
                }
                let autosave = config::load(&config::config_path())
                    .ok()
                    .is_some_and(|c| c.space_autosave == Some(true));
                if autosave
                    && !host.space_polish.failed
                    && host.context_menu.is_none()
                    && host.session_prompt.is_none()
                    && host
                        .space_polish
                        .changed
                        .as_ref()
                        .is_some_and(|(_, t)| now.duration_since(*t) >= Duration::from_secs(2))
                {
                    save_space_from_host(host, &name);
                }
                if host.space_polish.failed {
                    "Save failed"
                } else {
                    "Unsaved changes"
                }
            }
        }
        _ => "Save unavailable",
    };
    if host.space_rail.save_status != status {
        host.space_rail.save_status = status.to_string();
        host.dirty = true;
    }
}
