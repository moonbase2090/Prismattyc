//! Window-local Space views containing explicit blank terminals.
//!
//! Keep the complete runtime so PTYs, split ratios, tabs, and focus survive a
//! switch. Stable Space IDs distinguish a rename from a replacement Space.

use super::*;
use std::collections::HashSet;

pub(super) struct Parked {
    pub(super) mux: mux::MuxRuntime,
    adopted: attach_adopt::Adopted,
    observed: HashSet<String>,
}

#[derive(Default)]
pub(super) struct Views {
    pub(super) parked: HashMap<Option<String>, Parked>,
    last_poll: Option<Instant>,
    loaded: bool,
    pending: HashMap<Option<String>, mux::LocalRecipe>,
    pub(super) fresh: bool,
}

pub(super) fn has_local(mux: &mux::MuxRuntime) -> bool {
    mux.tab_panes()
        .iter()
        .flat_map(|(_, panes)| panes)
        .any(|pane| mux.is_retained_local_terminal(*pane))
}

/// Switch only when one of the views has local state worth retaining.
pub(super) fn switch(host: &mut HostState, owner: Option<String>) -> Result<bool> {
    if host.mux.space_id == owner {
        return Ok(false);
    }
    if !has_local(&host.mux) && !host.local_views.parked.contains_key(&owner) {
        return Ok(false);
    }
    // Construct before parking so an allocation/spawn failure keeps the view.
    let restored = host.local_views.parked.contains_key(&owner);
    let incoming = if restored {
        host.local_views.parked.remove(&owner).unwrap()
    } else {
        Parked {
            mux: host.mux.empty_view()?,
            adopted: Default::default(),
            observed: Default::default(),
        }
    };
    let old_owner = host.mux.space_id.clone();
    let old = Parked {
        mux: std::mem::replace(&mut host.mux, incoming.mux),
        adopted: std::mem::replace(&mut host.adopted, incoming.adopted),
        observed: std::mem::replace(&mut host.observed_space_sessions, incoming.observed),
    };
    host.hyperlink_hover = None;
    if has_local(&old.mux) {
        host.local_views.parked.insert(old_owner, old);
    }
    host.mux.space_id = owner;
    sync_attach_pane_sessions(host);
    host.attach_layout = None;
    host.last_space_refresh = None;
    host.pane_damage.clear();
    host.last_pane_views.clear();
    host.last_painted_cursor_rows.clear();
    host.last_layout_snapshot = None;
    host.last_chrome_snapshot = None;
    host.last_focused = host.mux.focused_id();
    host.last_mail_depths.clear();
    host.bell_toasts.clear();
    host.pane_bells.cancel();
    host.last_attention_notify.clear();
    host.pending_attention_announce = None;
    host.pending_mail_announce = None;
    host.find = FindMode::default();
    host.preedit = Preedit::default();
    host.tab_rename = None;
    host.strip_drag = None;
    host.divider_drag = None;
    host.rail_resizing = false;
    host.scrollbar_drag = None;
    host.left_button_down = false;
    host.app_mouse_button = None;
    host.rich_pointer = None;
    host.cursor_cell = None;
    host.last_app_mouse_cell = None;
    host.hover_target = None;
    host.pending_full_repaint = Some(FullRepaintReason::Resize);
    host.dirty = true;
    Ok(restored)
}

/// Hidden shells continue consuming output without repainting the active Space.
pub(super) fn drain(host: &mut HostState) -> bool {
    let mut more = false;
    host.local_views.parked.retain(|_, view| {
        more |= view.mux.drain_all().1;
        view.mux.take_pending_bells();
        view.mux.take_pending_attentions();
        view.mux.take_pending_toasts();
        has_local(&view.mux) && !view.mux.all_children_exited()
    });
    more
}

/// Reconciliation needs physical tab indices, including local-only tabs.
/// These records stay in memory; the persistent cache still omits local shells.
pub(super) fn records(host: &HostState) -> attach_tabs::AttachTabsFile {
    let mut file = attach_records_from_live(host);
    if has_local(&host.mux) {
        file.tabs = host
            .mux
            .tab_panes()
            .into_iter()
            .map(|(title, panes)| {
                let mut seen = HashSet::new();
                let sessions = panes
                    .iter()
                    .filter_map(|pane| host.mux.attach_session_of(*pane))
                    .filter(|id| seen.insert(id.to_string()))
                    .map(str::to_string)
                    .collect();
                attach_tabs::AttachTabRecord { title, sessions }
            })
            .collect();
        file.active_tab = host.mux.selected_tab_index();
    }
    file
}

pub(super) fn layout(
    host: &HostState,
    name: &str,
    space: &prismattyc_mux::SavedSpace,
    snapshot: &prismattyc_mux::Snapshot,
    current: &attach_tabs::AttachTabsFile,
) -> attach_tabs::AttachTabsFile {
    let local_tabs: HashSet<usize> = host
        .mux
        .tab_panes()
        .iter()
        .enumerate()
        .filter_map(|(index, (_, panes))| {
            panes
                .iter()
                .any(|pane| host.mux.is_retained_local_terminal(*pane))
                .then_some(index)
        })
        .collect();
    if local_tabs.is_empty() {
        space_view::owned_layout(name, space, snapshot, current)
    } else {
        space_view::owned_layout_with_local_tabs(name, space, snapshot, current, &local_tabs)
    }
}

pub(super) fn restore_local_focus(host: &mut HostState, pane: PaneId) {
    if !host.mux.is_retained_local_terminal(pane) {
        return;
    }
    if let Some(window) = host.mux.pane_window(pane) {
        if let Some(index) = host.mux.window_ids().iter().position(|id| *id == window) {
            let _ = host.mux.select_tab(index);
            host.mux.focus(pane);
        }
    }
}

/// Build a live-only view. Saved launch commands are never executed here.
fn incoming(host: &HostState, owner: &Option<String>) -> Result<Parked> {
    let mut mux = host.mux.empty_view()?;
    mux.space_id = owner.clone();
    let file = live_layout(owner)?;
    let mut bindings = HashMap::new();
    regroup::apply(
        &mut mux,
        &mut bindings,
        &file,
        &find_mux_bin().to_string_lossy(),
        &attach_log::session_names(),
    )?;
    Ok(Parked {
        mux,
        adopted: Default::default(),
        observed: Default::default(),
    })
}
fn owner_name(owner: &Option<String>) -> Result<Option<String>> {
    if owner.is_none() {
        return Ok(None);
    }
    let name = prismattyc_mux::list_spaces(&spaces_dir())?
        .into_iter()
        .find(|e| {
            load_space(&spaces_dir(), &e.name)
                .ok()
                .is_some_and(|space| &space.id == owner)
        })
        .map(|e| e.name);
    if owner.is_some() {
        anyhow::ensure!(name.is_some(), "Space was deleted");
    }
    Ok(name)
}
fn live_layout(owner: &Option<String>) -> Result<attach_tabs::AttachTabsFile> {
    let snapshot = attach_log::live_snapshot().context("daemon unavailable")?;
    let name = owner_name(owner)?;
    let tabs = snapshot
        .sessions
        .iter()
        .filter(|s| &s.space_id == owner)
        .map(|s| attach_tabs::AttachTabRecord {
            title: s.name.clone(),
            sessions: vec![s.id.to_string()],
        })
        .collect();
    Ok(attach_tabs::AttachTabsFile {
        tabs,
        space: name,
        space_id: owner.clone(),
        ..Default::default()
    })
}
pub(super) fn activate_owner(host: &mut HostState, owner: Option<String>) -> Result<()> {
    if host.mux.space_id != owner {
        if !host.local_views.parked.contains_key(&owner) {
            let incoming = incoming(host, &owner)?;
            host.local_views.parked.insert(owner.clone(), incoming);
        }
        switch(host, owner.clone())?;
    }
    let name = owner_name(&owner)?;
    set_current_space(host, name);
    sync_attach_pane_sessions(host);
    host.attach_layout = Some(records(host));
    App::refit_geom(host, host.window.inner_size(), Some("Space view"));
    persist_attach_layout_from_live(host);
    Ok(())
}
pub(super) fn move_blank(host: &mut HostState, target: &str) -> Result<()> {
    let owner = load_space(&spaces_dir(), target)?
        .id
        .context("Space has no stable identity")?;
    let key = Some(owner);
    anyhow::ensure!(key != host.mux.space_id, "this is the current Space");
    let mut destination = match host.local_views.parked.remove(&key) {
        Some(view) => view,
        None => incoming(host, &key)?,
    };
    if let Some(recipe) = host.local_views.pending.remove(&key) {
        if let Err(error) = destination.mux.restore_local_recipe(&recipe) {
            host.local_views.pending.insert(key.clone(), recipe);
            host.local_views.parked.insert(key, destination);
            return Err(error);
        }
    }
    let pane = host.mux.focused_id();
    let result = host.mux.transfer_local(&mut destination.mux, pane);
    host.local_views.parked.insert(key, destination);
    result?;
    host.pane_damage.clear();
    host.last_pane_views.clear();
    host.last_layout_snapshot = None;
    sync_attach_pane_sessions(host);
    App::refit_geom(host, host.window.inner_size(), Some("move blank terminal"));
    persist_attach_layout_from_live(host);
    host.dirty = true;
    rail_toast(host, &format!(" Moved blank terminal to {target} "));
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedViews {
    version: u32,
    views: Vec<(Option<String>, mux::LocalRecipe)>,
}
fn recipe_path(host: &HostState) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    host.attach_layout_path.as_ref()?.hash(&mut hash);
    Some(
        spaces_dir()
            .parent()?
            .join("local-views")
            .join(format!("{:016x}.json", hash.finish())),
    )
}

/// Poll recipes after Space reconciliation, and once before closing a window.
pub(super) fn persist_and_restore(host: &mut HostState, closing: bool) {
    if host.restore_prompt.is_some() || !host.cache_writer || host.space_opens.blocks_persist() {
        return;
    }
    if !closing
        && host
            .local_views
            .last_poll
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
    {
        return;
    }
    host.local_views.last_poll = Some(Instant::now());
    let Some(path) = recipe_path(host) else {
        return;
    };
    let Ok(config) = config::load(&config::config_path()) else {
        return;
    };
    let enabled = config.restore_blank_terminals.unwrap_or(false);
    if !enabled {
        host.local_views.pending.clear();
        host.local_views.loaded = false;
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        return;
    }
    if !host.local_views.loaded {
        host.local_views.loaded = true;
        if let Ok(raw) = std::fs::read(&path) {
            if raw.len() <= 2 * 1024 * 1024 {
                if let Ok(saved) = serde_json::from_slice::<SavedViews>(&raw) {
                    if saved.version == 1 && saved.views.len() <= 64 {
                        host.local_views.pending = saved.views.into_iter().collect();
                    }
                }
            }
        }
    }
    if host.local_views.fresh {
        host.local_views.pending.remove(&None);
        host.local_views.fresh = false;
    }
    let owner = host.mux.space_id.clone();
    if !closing && !has_local(&host.mux) {
        if let Some(recipe) = host.local_views.pending.remove(&owner) {
            match host.mux.restore_local_recipe(&recipe) {
                Ok(()) => {
                    host.pane_damage.clear();
                    host.last_pane_views.clear();
                    host.last_layout_snapshot = None;
                    host.last_chrome_snapshot = None;
                    host.last_focused = host.mux.focused_id();
                    sync_attach_pane_sessions(host);
                    App::refit_geom(
                        host,
                        host.window.inner_size(),
                        Some("restore blank terminals"),
                    );
                    persist_attach_layout_from_live(host);
                    host.dirty = true;
                }
                Err(error) => {
                    host.local_views.pending.insert(owner, recipe);
                    rail_toast(host, &format!("Could not restore blank terminals: {error}"));
                    return;
                }
            }
        }
    }
    if !closing || has_local(&host.mux) {
        host.local_views.pending.remove(&owner);
    }
    let mut views = host.local_views.pending.clone();
    if let Some(recipe) = host.mux.local_recipe() {
        views.insert(owner, recipe);
    }
    for (owner, view) in &host.local_views.parked {
        views.remove(owner);
        if let Some(recipe) = view.mux.local_recipe() {
            views.insert(owner.clone(), recipe);
        }
    }
    let mut views: Vec<_> = views.into_iter().collect();
    views.sort_by(|a, b| a.0.cmp(&b.0));
    let body = match serde_json::to_vec(&SavedViews { version: 1, views }) {
        Ok(body) => body,
        Err(_) => return,
    };
    if std::fs::read(&path).ok().as_ref() == Some(&body) {
        return;
    }
    let write = || -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension(format!("local-views-{}.tmp", std::process::id()));
        std::fs::write(&temp, &body)?;
        std::fs::rename(temp, &path)?;
        Ok(())
    };
    if let Err(error) = write() {
        rail_toast(
            host,
            &format!("Could not save blank terminal layout: {error}"),
        );
    }
}
