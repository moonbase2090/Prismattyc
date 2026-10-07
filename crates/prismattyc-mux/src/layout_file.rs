//! Saved mux layouts and multi-session spaces: snapshot → JSON file → split plan.
//!
//! Files do not store pane ids. Single-session layouts store spawn-time cwd.
//! Space files prefer the live child cwd from [`crate::procinfo::cwd_of`].
//! Split planning matches [`crate::geometry::split_leaf`]: the original
//! pane stays `first`; the new pane is `second`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control::{AxisWire, LayoutSnapshot, PaneSnapshot, SessionSnapshot};

/// File format version. Bump only with a matching reader.
pub const SAVED_LAYOUT_VERSION: u32 = 1;

/// Multi-session space file version. Independent of [`SAVED_LAYOUT_VERSION`].
pub const SAVED_SPACE_VERSION: u32 = 1;
/// Version with a stable Space identity and exclusive live ownership.
pub const OWNED_SPACE_VERSION: u32 = 2;

const SPACE_ORDER_FILE: &str = ".space-order";
static SPACE_ORDER_WRITE_ID: AtomicU64 = AtomicU64::new(0);
static SPACE_WRITE_ID: AtomicU64 = AtomicU64::new(0);

/// Inclusive clamp so [`crate::ControlRequest::Split`] accepts the ratio.
const MIN_RATIO: f64 = 0.01;
const MAX_RATIO: f64 = 0.99;

/// One saved session tree. No live pane or window ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {
    pub version: u32,
    pub saved_at_unix: u64,
    pub session: String,
    pub windows: Vec<SavedWindow>,
}

/// One window: title, size at save time, and the split tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    pub root: SavedNode,
}

/// Layout tree node. Leaves carry cwd and optional spawn program (not re-run).
/// Space files also store the live foreground `command` (PT-93).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedNode {
    Leaf {
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        /// Foreground command at save time (space files). Absent when the
        /// pane child is the shell itself. `space open` may re-run it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        /// Pane title set with `pmux rename-pane` (PT-128). Absent when the
        /// pane had no title at save time; `space open` restores it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Split {
        axis: AxisWire,
        ratio: f64,
        first: Box<SavedNode>,
        second: Box<SavedNode>,
    },
}

/// Index of a virtual pane in the apply walk. `0` is the window's first pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRef(pub usize);

/// One `Split` to issue while rebuilding a tree.
///
/// `target` is the pane that stays as `first`. The new pane becomes `second`
/// and is spawned with `cwd` (first leaf of the saved `second` subtree).
#[derive(Debug, Clone, PartialEq)]
pub struct SplitOp {
    pub target: NodeRef,
    pub new: NodeRef,
    pub axis: AxisWire,
    pub ratio: f64,
    pub cwd: Option<PathBuf>,
}

/// One file from [`list_layouts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutListEntry {
    pub name: String,
    pub windows: usize,
    pub panes: usize,
}

/// Named collection of sessions (PT-56). Not the runtime Space from PT-54.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSpace {
    pub version: u32,
    /// Stable identity, independent of the filename. Absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Creation time stays fixed across saves and renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<u64>,
    pub saved_at_unix: u64,
    pub sessions: Vec<SavedSpaceSession>,
    /// Host tab arrangement: which sessions share a tab as panes, in tab
    /// and pane order (PT-60). Empty means one tab per session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SavedSpaceTab>,
    /// Index into `tabs`. Missing or 0 means the first tab (PT-66).
    #[serde(default, skip_serializing_if = "crate::attach_tabs::is_zero")]
    pub active_tab: usize,
    /// Session name of the focused host pane. Missing means the first
    /// session of the active tab (PT-66).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session: Option<String>,
}

/// One host tab inside a [`SavedSpace`]: session names in pane order.
///
/// `layout` is the host split tree. Leaves name sessions. Absent on a
/// single-pane tab and on files written before the tree was stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSpaceTab {
    pub title: String,
    pub sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<crate::attach_tabs::TabLayoutNode>,
}

/// Session names in host tab order, then any saved session not in a tab.
///
/// Duplicate names keep the first occurrence. An empty `tabs` list yields
/// `sessions` file order (one tab per session).
pub fn space_sessions_in_tab_order(space: &SavedSpace) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |name: &str| {
        if !out.iter().any(|existing: &String| existing == name) {
            out.push(name.to_string());
        }
    };
    for tab in &space.tabs {
        for name in &tab.sessions {
            push(name);
        }
    }
    for session in &space.sessions {
        push(&session.name);
    }
    out
}

/// Minimal session record for `space add` when no live snapshot is available.
#[must_use]
pub fn stub_space_session(name: impl Into<String>) -> SavedSpaceSession {
    let name = name.into();
    SavedSpaceSession {
        name: name.clone(),
        agent: None,
        windows: vec![SavedWindow {
            title: name,
            cols: 80,
            rows: 24,
            root: SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            },
        }],
    }
}

/// Append a session as a new tab. Refuses a name already in the space.
pub fn space_add_session(
    space: &mut SavedSpace,
    session: SavedSpaceSession,
    tab_title: Option<&str>,
) -> Result<()> {
    if space_contains_session(space, &session.name) {
        bail!("session {:?} is already in this space", session.name);
    }
    let name = session.name.clone();
    space.sessions.push(session);
    let title = tab_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name.clone());
    space.tabs.push(SavedSpaceTab {
        title,
        sessions: vec![name],
        layout: None,
    });
    space.saved_at_unix = now_unix();
    Ok(())
}

/// Remove a session from sessions and tabs. Refuses a missing or last session.
pub fn space_remove_session(space: &mut SavedSpace, session: &str) -> Result<()> {
    if !space.sessions.iter().any(|entry| entry.name == session) {
        bail!("session {session:?} is not in this space");
    }
    if space.sessions.len() == 1 {
        bail!("cannot remove the last session from the space");
    }
    space.sessions.retain(|entry| entry.name != session);
    for tab in &mut space.tabs {
        tab.sessions.retain(|name| name != session);
        let kept = tab.sessions.clone();
        tab.layout = tab.layout.as_ref().and_then(|node| {
            crate::attach_tabs::remap_layout(node, |name| {
                (name != session).then(|| name.to_string())
            })
        });
        tab.layout = crate::attach_tabs::layout_for_sessions(tab.layout.take(), &kept);
    }
    space.tabs.retain(|tab| !tab.sessions.is_empty());
    if space.focused_session.as_deref() == Some(session) {
        space.focused_session = None;
    }
    if !space.tabs.is_empty() && space.active_tab >= space.tabs.len() {
        space.active_tab = space.tabs.len() - 1;
    }
    space.saved_at_unix = now_unix();
    Ok(())
}

fn space_contains_session(space: &SavedSpace, session: &str) -> bool {
    space.sessions.iter().any(|entry| entry.name == session)
        || space
            .tabs
            .iter()
            .any(|tab| tab.sessions.iter().any(|name| name == session))
}

/// Focused session when it is in the space, else the first name in tab order.
pub fn space_active_session(space: &SavedSpace) -> Option<String> {
    let names = space_sessions_in_tab_order(space);
    if let Some(focus) = space.focused_session.as_ref() {
        if names.iter().any(|name| name == focus) {
            return Some(focus.clone());
        }
    }
    names.into_iter().next()
}

/// One session inside a [`SavedSpace`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSpaceSession {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub windows: Vec<SavedWindow>,
}

/// One file from [`list_spaces`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceListEntry {
    pub name: String,
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    /// Host tabs recorded in the file. Zero means none were saved (one tab
    /// per session at restore).
    pub tabs: usize,
    pub saved_at_unix: u64,
}

/// How to pick a leaf cwd when snapshotting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdSource {
    /// `PaneSnapshot.spawn.cwd` (PT-49 layout files).
    Spawn,
    /// Live `/proc/<pid>/cwd` when readable, else spawn cwd (PT-56 space files).
    Live,
}

impl SavedNode {
    /// Number of leaves in this subtree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Leaf `command` fields in first-then-second order, one slot per pane.
    #[must_use]
    pub fn leaf_commands(&self) -> Vec<Option<&str>> {
        let mut out = Vec::new();
        self.collect_leaf_commands(&mut out);
        out
    }

    fn collect_leaf_commands<'a>(&'a self, out: &mut Vec<Option<&'a str>>) {
        match self {
            Self::Leaf { command, .. } => out.push(command.as_deref()),
            Self::Split { first, second, .. } => {
                first.collect_leaf_commands(out);
                second.collect_leaf_commands(out);
            }
        }
    }

    /// Pane titles per leaf in apply order (same walk as [`Self::leaf_commands`]).
    pub fn leaf_titles(&self) -> Vec<Option<&str>> {
        let mut out = Vec::new();
        self.collect_leaf_titles(&mut out);
        out
    }

    fn collect_leaf_titles<'a>(&'a self, out: &mut Vec<Option<&'a str>>) {
        match self {
            Self::Leaf { title, .. } => out.push(title.as_deref()),
            Self::Split { first, second, .. } => {
                first.collect_leaf_titles(out);
                second.collect_leaf_titles(out);
            }
        }
    }
}

/// Convert a live session snapshot into a self-contained layout.
///
/// `cwd` on each leaf is `PaneSnapshot.spawn.cwd` (spawn-time).
#[must_use]
pub fn from_snapshot(session: &SessionSnapshot) -> SavedLayout {
    from_snapshot_with(session, CwdSource::Spawn)
}

/// Convert a session snapshot using `cwd_source` for leaf directories.
#[must_use]
pub fn from_snapshot_with(session: &SessionSnapshot, cwd_source: CwdSource) -> SavedLayout {
    SavedLayout {
        version: SAVED_LAYOUT_VERSION,
        saved_at_unix: now_unix(),
        session: session.name.clone(),
        windows: session
            .windows
            .iter()
            .map(|window| saved_window(window, cwd_source))
            .collect(),
    }
}

/// Convert live sessions into a space file. Leaves use [`CwdSource::Live`].
#[must_use]
pub fn from_sessions(sessions: &[&SessionSnapshot]) -> SavedSpace {
    SavedSpace {
        version: SAVED_SPACE_VERSION,
        id: None,
        created_at_unix_ms: Some(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        ),
        saved_at_unix: now_unix(),
        sessions: sessions
            .iter()
            .map(|session| SavedSpaceSession {
                name: session.name.clone(),
                agent: session
                    .agent_id
                    .as_deref()
                    .filter(|agent| !agent.is_empty())
                    .map(str::to_string),
                windows: session
                    .windows
                    .iter()
                    .map(|window| saved_window(window, CwdSource::Live))
                    .collect(),
            })
            .collect(),
        tabs: Vec::new(),
        active_tab: 0,
        focused_session: None,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn saved_window(window: &crate::control::WindowSnapshot, cwd_source: CwdSource) -> SavedWindow {
    let meta_by_pane: HashMap<u64, LeafMeta> = window
        .panes
        .iter()
        .map(|pane| (pane.id, leaf_meta(pane, cwd_source)))
        .collect();
    SavedWindow {
        title: window.title.clone(),
        cols: u16::try_from(window.bounds.cols).unwrap_or(u16::MAX).max(1),
        rows: u16::try_from(window.bounds.rows).unwrap_or(u16::MAX).max(1),
        root: convert_layout(&window.layout, &meta_by_pane),
    }
}

#[derive(Clone, Default)]
struct LeafMeta {
    cwd: Option<PathBuf>,
    program: Option<String>,
    command: Option<String>,
    title: Option<String>,
}

fn leaf_meta(pane: &PaneSnapshot, cwd_source: CwdSource) -> LeafMeta {
    LeafMeta {
        title: Some(pane.title.clone()).filter(|title| !title.is_empty()),
        cwd: pane_cwd(pane, cwd_source),
        program: pane
            .spawn
            .as_ref()
            .map(|spawn| spawn.program.clone())
            .filter(|program| !program.is_empty()),
        command: match cwd_source {
            CwdSource::Live => pane
                .child_pid
                .and_then(crate::procinfo::foreground_command)
                .filter(|command| !command.is_empty()),
            CwdSource::Spawn => None,
        },
    }
}

fn pane_cwd(pane: &PaneSnapshot, cwd_source: CwdSource) -> Option<PathBuf> {
    let spawn = spawn_cwd(pane);
    match cwd_source {
        CwdSource::Spawn => spawn,
        CwdSource::Live => pane
            .child_pid
            .and_then(crate::procinfo::cwd_of)
            .filter(|path| path.is_absolute())
            .or(spawn),
    }
}

fn spawn_cwd(pane: &PaneSnapshot) -> Option<PathBuf> {
    pane.spawn.as_ref().and_then(|spawn| spawn.cwd.clone())
}

fn convert_layout(layout: &LayoutSnapshot, meta_by_pane: &HashMap<u64, LeafMeta>) -> SavedNode {
    match layout {
        LayoutSnapshot::Leaf { pane_id } => {
            let meta = meta_by_pane.get(pane_id).cloned().unwrap_or_default();
            SavedNode::Leaf {
                cwd: meta.cwd,
                program: meta.program,
                command: meta.command,
                title: meta.title,
            }
        }
        LayoutSnapshot::Split {
            axis,
            ratio,
            first,
            second,
        } => SavedNode::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(convert_layout(first, meta_by_pane)),
            second: Box::new(convert_layout(second, meta_by_pane)),
        },
    }
}

/// Walk `root` into an ordered split list.
///
/// Returns the first-leaf cwd of `root` (spawn cwd for the initial pane)
/// and the splits. Each split creates the `second` child; recurse into
/// `first` on the original pane, then `second` on the new pane.
#[must_use]
pub fn plan(root: &SavedNode) -> (Option<PathBuf>, Vec<SplitOp>) {
    let mut ops = Vec::new();
    let mut next = 1;
    build_plan(root, NodeRef(0), &mut ops, &mut next);
    (first_leaf_cwd(root), ops)
}

fn build_plan(node: &SavedNode, pane: NodeRef, ops: &mut Vec<SplitOp>, next: &mut usize) {
    if let SavedNode::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    {
        let new_pane = NodeRef(*next);
        *next += 1;
        ops.push(SplitOp {
            target: pane,
            new: new_pane,
            axis: *axis,
            ratio: clamp_ratio(*ratio),
            cwd: first_leaf_cwd(second),
        });
        build_plan(first, pane, ops, next);
        build_plan(second, new_pane, ops, next);
    }
}

fn first_leaf_cwd(node: &SavedNode) -> Option<PathBuf> {
    match node {
        SavedNode::Leaf { cwd, .. } => cwd.clone(),
        SavedNode::Split { first, .. } => first_leaf_cwd(first),
    }
}

/// Clamp a split ratio into `(0, 1)` so the control plane accepts it.
#[must_use]
pub fn clamp_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

/// `$XDG_DATA_HOME/prismattyc/layouts`, else `$HOME/.local/share/prismattyc/layouts`.
#[must_use]
pub fn layouts_dir() -> PathBuf {
    layouts_dir_from(crate::platform::data_home(), crate::platform::home_dir())
}

fn layouts_dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = xdg
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| Path::new(&home).join(".local/share")));
    base.map_or_else(
        || PathBuf::from("prismattyc-layouts"),
        |base| base.join("prismattyc").join("layouts"),
    )
}

/// Reject empty names, `'/'`, and `'..'`.
pub fn validate_layout_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains("..") || name.contains('\0') {
        bail!("layout name must be a non-empty file name without '/' or '..'");
    }
    #[cfg(windows)]
    {
        if name.bytes().any(|byte| {
            byte < 32
                || matches!(
                    byte,
                    b'\\' | b':' | b'<' | b'>' | b'"' | b'|' | b'?' | b'*' | b'~'
                )
        }) || name.ends_with(['.', ' '])
        {
            bail!("layout name contains a Windows filename character or alias");
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
        let stem = stem.to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
        if device {
            bail!("layout name is a reserved Windows device name");
        }
    }
    Ok(())
}

/// `{dir}/{name}.json` after validating `name`.
pub fn layout_path(dir: &Path, name: &str) -> Result<PathBuf> {
    validate_layout_name(name)?;
    Ok(dir.join(format!("{name}.json")))
}

/// Write `layout` as pretty JSON. Creates `dir` when missing.
pub fn save_layout(dir: &Path, name: &str, layout: &SavedLayout) -> Result<PathBuf> {
    let path = layout_path(dir, name)?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let body = serde_json::to_string_pretty(layout).context("serialize layout")?;
    let mut file = fs::File::create(&path).with_context(|| format!("write {}", path.display()))?;
    file.write_all(body.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Read `{dir}/{name}.json`.
pub fn load_layout(dir: &Path, name: &str) -> Result<SavedLayout> {
    let path = layout_path(dir, name)?;
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let layout: SavedLayout =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    if layout.version != SAVED_LAYOUT_VERSION {
        bail!(
            "unsupported layout version {} in {} (want {SAVED_LAYOUT_VERSION})",
            layout.version,
            path.display()
        );
    }
    if layout.windows.is_empty() {
        bail!("layout {name:?} has no windows");
    }
    Ok(layout)
}

/// List `*.json` layouts in `dir`. Missing dir yields an empty list.
pub fn list_layouts(dir: &Path) -> Result<Vec<LayoutListEntry>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("list {}", dir.display()));
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_layout_name(stem).is_err() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(layout) = serde_json::from_str::<SavedLayout>(&raw) else {
            continue;
        };
        out.push(LayoutListEntry {
            name: stem.to_string(),
            windows: layout.windows.len(),
            panes: layout
                .windows
                .iter()
                .map(|window| window.root.pane_count())
                .sum(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The default daemon retains its historical Space directory. Other daemons
/// get a stable private subdirectory keyed by their recorded socket identity.
#[must_use]
pub fn spaces_dir() -> PathBuf {
    let base = spaces_dir_default();
    let socket = std::env::var_os("PRISMATTYC_SPACES_SOCKET")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("PMUX_SOCKET").filter(|value| !value.is_empty()));
    match socket {
        None => base,
        Some(socket) => spaces_dir_for_socket(Path::new(&socket)),
    }
}

/// Return the explicitly socket-scoped Spaces directory.
#[must_use]
pub fn spaces_dir_for_socket(socket: &Path) -> PathBuf {
    let base = spaces_dir_default();
    spaces_dir_for_socket_with_base(socket, &base)
}

/// Stable marker path shared by the mux daemon and its clients.
#[must_use]
pub fn spaces_daemon_identity_path(socket: &Path) -> PathBuf {
    let stable = stable_socket_identity(socket);
    let parent = stable.parent().unwrap_or_else(|| Path::new("."));
    let name = stable
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    parent.join(format!("{name}.spaces-identity"))
}

/// Canonical socket identity used when a daemon has no marker yet.
#[must_use]
pub fn spaces_socket_identity(socket: &Path) -> String {
    stable_socket_identity(socket).display().to_string()
}

fn spaces_dir_default() -> PathBuf {
    spaces_dir_from(crate::platform::data_home(), crate::platform::home_dir())
}

fn spaces_dir_for_socket_with_base(socket: &Path, base: &Path) -> PathBuf {
    let marker = spaces_daemon_identity_path(socket);
    if let Ok(identity) = fs::read_to_string(marker) {
        let identity = identity.trim();
        if !identity.is_empty() {
            return spaces_dir_for_identity(identity, base);
        }
    }
    let identity = spaces_socket_identity(socket);
    if crate::default_socket_path("default")
        .ok()
        .is_some_and(|default| spaces_socket_identity(&default) == identity)
    {
        return base.to_path_buf();
    }
    spaces_dir_for_identity(&identity, base)
}

fn spaces_dir_for_identity(identity: &str, base: &Path) -> PathBuf {
    if identity == "default" {
        return base.to_path_buf();
    }
    let digest = Sha256::digest(identity.as_bytes());
    base.join("instances").join(format!("sha256-{digest:x}"))
}

fn stable_socket_identity(socket: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(socket) {
        return canonical;
    }
    let mut candidate = socket.to_path_buf();
    if let Ok(link) = fs::read_link(&candidate) {
        candidate = if link.is_absolute() {
            link
        } else {
            candidate
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    let parent = candidate
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok())
        .unwrap_or_else(|| {
            candidate
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        });
    let file_name = candidate.file_name().map(PathBuf::from).unwrap_or_default();
    parent.join(file_name)
}

fn spaces_dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = xdg
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| Path::new(&home).join(".local/share")));
    base.map_or_else(
        || PathBuf::from("prismattyc-spaces"),
        |base| base.join("prismattyc").join("spaces"),
    )
}

/// Write `space` as pretty JSON. Creates `dir` when missing.
pub fn save_space(dir: &Path, name: &str, space: &SavedSpace) -> Result<PathBuf> {
    let path = layout_path(dir, name)?;
    let append_to_custom_order = !path.exists() && space_order_path(dir).exists();
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut space = space.clone();
    space.created_at_unix_ms = Some(
        load_space(dir, name)
            .ok()
            .map(|old| creation_time_ms(&path, &old))
            .or(space.created_at_unix_ms)
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            }),
    );
    let body = serde_json::to_string_pretty(&space).context("serialize space")?;
    let mut bytes = body.into_bytes();
    bytes.push(b'\n');
    // A crash mid-write must leave the previous space file intact.
    write_space_atomic(dir, &path, &bytes)?;
    if append_to_custom_order {
        let mut order = list_spaces(dir)?
            .into_iter()
            .map(|entry| entry.name)
            .filter(|saved_name| saved_name != name)
            .collect::<Vec<_>>();
        order.push(name.to_owned());
        write_space_order(dir, &order)?;
    }
    Ok(path)
}

/// Delete `{dir}/{name}.json` after validating `name`.
pub fn remove_named_json(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = layout_path(dir, name)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            bail!("no such file {}", path.display())
        }
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

/// Delete a space file. Same name rules as [`save_space`].
pub fn remove_space(dir: &Path, name: &str) -> Result<PathBuf> {
    let removed = remove_named_json(dir, name)?;
    let order_path = space_order_path(dir);
    if order_path.exists() {
        let order = read_space_order(dir)?
            .into_iter()
            .filter(|saved_name| saved_name != name)
            .collect::<Vec<_>>();
        write_space_order(dir, &order)?;
    }
    Ok(removed)
}

/// Delete a layout file. Same name rules as [`save_layout`].
pub fn remove_layout(dir: &Path, name: &str) -> Result<PathBuf> {
    remove_named_json(dir, name)
}

/// Rename `{dir}/{old}.json` to `{dir}/{new}.json`. Both names follow the
/// [`save_space`] rules; an existing target is refused, never overwritten.
pub fn rename_space(dir: &Path, old: &str, new: &str) -> Result<PathBuf> {
    let from = layout_path(dir, old)?;
    let to = layout_path(dir, new)?;
    if from == to {
        return Ok(to);
    }
    if to.exists() {
        bail!("space {new:?} already exists at {}", to.display());
    }
    match fs::rename(&from, &to) {
        Ok(()) => {
            let order_path = space_order_path(dir);
            if order_path.exists() {
                let order = read_space_order(dir)?
                    .into_iter()
                    .map(|saved_name| {
                        if saved_name == old {
                            new.to_owned()
                        } else {
                            saved_name
                        }
                    })
                    .collect::<Vec<_>>();
                write_space_order(dir, &order)?;
            }
            Ok(to)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            bail!("no such space {old:?} at {}", from.display())
        }
        Err(err) => {
            Err(err).with_context(|| format!("rename {} to {}", from.display(), to.display()))
        }
    }
}

/// Read `{dir}/{name}.json` as a space file.
pub fn load_space(dir: &Path, name: &str) -> Result<SavedSpace> {
    let path = layout_path(dir, name)?;
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut space: SavedSpace =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    if space.version != SAVED_SPACE_VERSION && space.version != OWNED_SPACE_VERSION {
        bail!(
            "unsupported space version {} in {} (want {SAVED_SPACE_VERSION})",
            space.version,
            path.display()
        );
    }
    if space.version == OWNED_SPACE_VERSION && !space.id.as_deref().is_some_and(valid_space_id) {
        bail!("space {name:?} has no valid stable identity");
    }
    if space.sessions.is_empty() && space.version == SAVED_SPACE_VERSION {
        bail!("space {name:?} has no sessions");
    }
    for session in &space.sessions {
        if session.windows.is_empty() {
            bail!("space {name:?} session {:?} has no windows", session.name);
        }
    }
    space.created_at_unix_ms = Some(creation_time_ms(&path, &space));
    Ok(space)
}

/// Opaque identity for a Space or a freshly created session name.
pub fn new_space_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("Space identity: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn valid_space_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn creation_time_ms(path: &Path, space: &SavedSpace) -> u64 {
    space.created_at_unix_ms.unwrap_or_else(|| {
        fs::metadata(path)
            .ok()
            .and_then(|meta| meta.created().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|age| age.as_millis() as u64)
            .unwrap_or_else(|| space.saved_at_unix.saturating_mul(1000))
    })
}

/// List spaces in their persisted custom order. Unlisted spaces follow in
/// creation order, so newly discovered files append before the rail's `+`.
/// When there is no custom order file, legacy creation order is preserved.
pub fn list_spaces(dir: &Path) -> Result<Vec<SpaceListEntry>> {
    let mut entries = list_spaces_by_creation(dir)?;
    let order = read_space_order(dir)?;
    if order.is_empty() {
        return Ok(entries);
    }

    let indexes = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut included = HashSet::with_capacity(entries.len());
    let mut ordered = Vec::with_capacity(entries.len());
    for name in order {
        if let Some(&index) = indexes.get(name.as_str()) {
            if included.insert(index) {
                ordered.push(entries[index].clone());
            }
        }
    }
    for (index, entry) in entries.drain(..).enumerate() {
        if included.insert(index) {
            ordered.push(entry);
        }
    }
    Ok(ordered)
}

/// Persist one exact permutation of the saved spaces. The space JSON files
/// are untouched; list consumers all read the same small order file.
pub fn reorder_spaces(dir: &Path, names: &[String]) -> Result<bool> {
    let existing = list_spaces(dir)?;
    let existing_names = existing
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<HashSet<_>>();
    let requested_names = names.iter().map(String::as_str).collect::<HashSet<_>>();
    if names.len() != existing.len() || requested_names.len() != names.len() {
        bail!("space order must contain each saved space exactly once");
    }
    if requested_names != existing_names {
        bail!("space order does not match the saved spaces");
    }
    if existing.iter().map(|entry| &entry.name).eq(names.iter()) {
        return Ok(false);
    }
    write_space_order(dir, names)?;
    Ok(true)
}

fn list_spaces_by_creation(dir: &Path) -> Result<Vec<SpaceListEntry>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("list {}", dir.display()));
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_layout_name(stem).is_err() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(space) = serde_json::from_str::<SavedSpace>(&raw) else {
            continue;
        };
        out.push((
            creation_time_ms(&path, &space),
            SpaceListEntry {
                name: stem.to_string(),
                sessions: space.sessions.len(),
                windows: space
                    .sessions
                    .iter()
                    .map(|session| session.windows.len())
                    .sum(),
                panes: space
                    .sessions
                    .iter()
                    .flat_map(|session| session.windows.iter())
                    .map(|window| window.root.pane_count())
                    .sum(),
                tabs: space.tabs.len(),
                saved_at_unix: space.saved_at_unix,
            },
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    Ok(out.into_iter().map(|(_, entry)| entry).collect())
}

fn space_order_path(dir: &Path) -> PathBuf {
    dir.join(SPACE_ORDER_FILE)
}

fn read_space_order(dir: &Path) -> Result<Vec<String>> {
    let path = space_order_path(dir);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Write `bytes` to a sibling temp file, fsync, then rename over `path`.
fn write_space_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let nonce = SPACE_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("space.json");
    let temporary = dir.join(format!(".{file_name}.{}.{nonce}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        crate::platform::replace_file(&temporary, path)
            .with_context(|| format!("replace {}", path.display()))?;
        crate::platform::sync_directory(dir)
            .with_context(|| format!("sync directory {}", dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_space_order(dir: &Path, names: &[String]) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = space_order_path(dir);
    let nonce = SPACE_ORDER_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(
        ".{SPACE_ORDER_FILE}.{}.{nonce}.tmp",
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec_pretty(names).context("serialize space order")?;
    bytes.push(b'\n');
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        crate::platform::replace_file(&temporary, &path)
            .with_context(|| format!("replace {}", path.display()))?;
        crate::platform::sync_directory(dir)
            .with_context(|| format!("sync directory {}", dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Agent id to bind when applying a space session.
///
/// Saved agent wins. An empty or missing agent falls back to the session name
/// so named seats (`grok-pc`) restore a mailbox address.
#[must_use]
pub fn space_bind_agent(saved_agent: Option<&str>, session_name: &str) -> Option<String> {
    let agent = saved_agent
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .unwrap_or(session_name);
    let agent = agent.trim();
    if agent.is_empty() {
        None
    } else {
        Some(agent.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        PaneGeometry, PaneInputLedger, PaneSnapshot, SpawnSpec, WindowBounds, WindowSnapshot,
    };
    use std::collections::BTreeMap;

    fn ledger() -> PaneInputLedger {
        PaneInputLedger {
            last_output_at_ms: None,
            focused: false,
            controller_id: None,
            last_controller_write_at_ms: None,
            last_write_ended_with_cr: false,
            dirty_input: false,
            last_input_at_ms: None,
        }
    }

    fn pane(id: u64, cwd: Option<&str>, col: u32, cols: u32) -> PaneSnapshot {
        PaneSnapshot {
            pane_write: None,
            id,
            title: format!("p{id}"),
            title_pinned: false,
            controller_id: None,
            geometry: PaneGeometry {
                pane_id: id,
                col,
                row: 0,
                cols,
                rows: 24,
            },
            spawn: Some(SpawnSpec {
                program: "/bin/sh".into(),
                argv: vec![],
                cwd: cwd.map(PathBuf::from),
                env: BTreeMap::new(),
            }),
            child_pid: None,
            mail: None,
            status: None,
            attention: None,
            mail_inject: None,
            ledger: ledger(),
            size_owner: None,
        }
    }

    /// Three panes: vertical 0.5, then horizontal 0.3 on the first child.
    fn three_pane_session() -> SessionSnapshot {
        SessionSnapshot {
            space_id: None,
            id: 2,
            name: "work".into(),
            agent_id: None,
            windows: vec![WindowSnapshot {
                id: 1,
                title: "main".into(),
                bounds: WindowBounds {
                    window_id: 1,
                    cols: 80,
                    rows: 24,
                },
                sync_input: false,
                layout: LayoutSnapshot::Split {
                    axis: AxisWire::Vertical,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Split {
                        axis: AxisWire::Horizontal,
                        ratio: 0.3,
                        first: Box::new(LayoutSnapshot::Leaf { pane_id: 10 }),
                        second: Box::new(LayoutSnapshot::Leaf { pane_id: 12 }),
                    }),
                    second: Box::new(LayoutSnapshot::Leaf { pane_id: 11 }),
                },
                panes: vec![
                    pane(10, Some("/a"), 0, 24),
                    pane(12, Some("/b"), 24, 56),
                    pane(11, Some("/c"), 0, 80),
                ],
            }],
        }
    }

    #[test]
    fn snapshot_json_round_trip_preserves_tree() {
        let saved = from_snapshot(&three_pane_session());
        assert_eq!(saved.version, 1);
        assert_eq!(saved.session, "work");
        assert_eq!(saved.windows.len(), 1);
        let json = serde_json::to_string(&saved).unwrap();
        let loaded: SavedLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(saved, loaded);
        match &loaded.windows[0].root {
            SavedNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*axis, AxisWire::Vertical);
                assert_eq!(*ratio, 0.5);
                assert!(
                    matches!(second.as_ref(), SavedNode::Leaf { cwd, .. } if cwd.as_deref() == Some(Path::new("/c")))
                );
                match first.as_ref() {
                    SavedNode::Split {
                        axis,
                        ratio,
                        first,
                        second,
                    } => {
                        assert_eq!(*axis, AxisWire::Horizontal);
                        assert_eq!(*ratio, 0.3);
                        assert!(
                            matches!(first.as_ref(), SavedNode::Leaf { cwd, .. } if cwd.as_deref() == Some(Path::new("/a")))
                        );
                        assert!(
                            matches!(second.as_ref(), SavedNode::Leaf { cwd, .. } if cwd.as_deref() == Some(Path::new("/b")))
                        );
                    }
                    other => panic!("expected inner split, got {other:?}"),
                }
            }
            other => panic!("expected root split, got {other:?}"),
        }
    }

    #[test]
    fn plan_order_for_three_pane_tree_new_pane_is_second() {
        let saved = from_snapshot(&three_pane_session());
        let (root_cwd, ops) = plan(&saved.windows[0].root);
        assert_eq!(root_cwd.as_deref(), Some(Path::new("/a")));
        assert_eq!(ops.len(), 2);
        // split_leaf keeps the target as first; the new pane is second.
        // First op splits the root (node 0); the new pane is node 1 (bottom).
        assert_eq!(ops[0].target, NodeRef(0));
        assert_eq!(ops[0].new, NodeRef(1));
        assert_eq!(ops[0].axis, AxisWire::Vertical);
        assert_eq!(ops[0].ratio, 0.5);
        assert_eq!(ops[0].cwd.as_deref(), Some(Path::new("/c")));
        // Second op splits the original pane again (first subtree).
        assert_eq!(ops[1].target, NodeRef(0));
        assert_eq!(ops[1].new, NodeRef(2));
        assert_eq!(ops[1].axis, AxisWire::Horizontal);
        assert_eq!(ops[1].ratio, 0.3);
        assert_eq!(ops[1].cwd.as_deref(), Some(Path::new("/b")));
    }

    #[test]
    fn plan_clamps_out_of_range_ratio() {
        let root = SavedNode::Split {
            axis: AxisWire::Horizontal,
            ratio: 1.0,
            first: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
            second: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
        };
        let (_, ops) = plan(&root);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].ratio, MAX_RATIO);
        let root = SavedNode::Split {
            axis: AxisWire::Horizontal,
            ratio: 0.0,
            first: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
            second: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
        };
        let (_, ops) = plan(&root);
        assert_eq!(ops[0].ratio, MIN_RATIO);
        let root = SavedNode::Split {
            axis: AxisWire::Horizontal,
            ratio: f64::NAN,
            first: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
            second: Box::new(SavedNode::Leaf {
                cwd: None,
                program: None,
                command: None,
                title: None,
            }),
        };
        let (_, ops) = plan(&root);
        assert_eq!(ops[0].ratio, 0.5);
    }

    #[test]
    fn validate_layout_name_rejects_slash_dotdot_empty() {
        assert!(validate_layout_name("work").is_ok());
        assert!(validate_layout_name("").is_err());
        assert!(validate_layout_name("foo/bar").is_err());
        assert!(validate_layout_name("..").is_err());
        assert!(validate_layout_name("foo..bar").is_err());
    }

    #[test]
    fn layouts_dir_mirrors_mailbox_xdg_home() {
        let xdg = layouts_dir_from(Some("/xdg".into()), Some("/home/me".into()));
        assert_eq!(xdg, PathBuf::from("/xdg/prismattyc/layouts"));
        let home = layouts_dir_from(None, Some("/home/me".into()));
        assert_eq!(
            home,
            PathBuf::from("/home/me/.local/share/prismattyc/layouts")
        );
        let empty_xdg = layouts_dir_from(Some("".into()), Some("/home/me".into()));
        assert_eq!(
            empty_xdg,
            PathBuf::from("/home/me/.local/share/prismattyc/layouts")
        );
    }

    #[test]
    fn save_load_list_round_trip_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "prism-layout-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        let saved = from_snapshot(&three_pane_session());
        let path = save_layout(&dir, "work", &saved).unwrap();
        assert_eq!(path, dir.join("work.json"));
        let loaded = load_layout(&dir, "work").unwrap();
        assert_eq!(loaded.windows[0].root, saved.windows[0].root);
        let list = list_layouts(&dir).unwrap();
        assert_eq!(
            list,
            vec![LayoutListEntry {
                name: "work".into(),
                windows: 1,
                panes: 3,
            }]
        );
        assert!(save_layout(&dir, "../x", &saved).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_leaf_json_without_program_still_loads() {
        let json = r#"{"kind":"leaf","cwd":"/a"}"#;
        let node: SavedNode = serde_json::from_str(json).unwrap();
        match node {
            SavedNode::Leaf {
                cwd,
                program,
                command,
                title: _,
            } => {
                assert_eq!(cwd.as_deref(), Some(Path::new("/a")));
                assert_eq!(program, None);
                assert!(command.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_cwd_falls_back_to_spawn_without_pid() {
        let saved = from_snapshot_with(&three_pane_session(), CwdSource::Live);
        match &saved.windows[0].root {
            SavedNode::Split { first, .. } => match first.as_ref() {
                SavedNode::Split { first, .. } => match first.as_ref() {
                    SavedNode::Leaf {
                        cwd,
                        program,
                        command,
                        title: _,
                    } => {
                        assert_eq!(cwd.as_deref(), Some(Path::new("/a")));
                        assert_eq!(program.as_deref(), Some("/bin/sh"));
                        assert!(command.is_none());
                    }
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_cwd_reads_proc_when_pid_is_alive() {
        let mut session = three_pane_session();
        let pid = std::process::id();
        session.windows[0].panes[0].child_pid = Some(pid);
        session.windows[0].panes[0].spawn.as_mut().unwrap().cwd = Some("/no-such-spawn".into());
        let saved = from_snapshot_with(&session, CwdSource::Live);
        match &saved.windows[0].root {
            SavedNode::Split { first, .. } => match first.as_ref() {
                SavedNode::Split { first, .. } => match first.as_ref() {
                    SavedNode::Leaf { cwd, .. } => {
                        let cwd = cwd.as_ref().expect("live cwd");
                        assert!(cwd.is_absolute(), "{cwd:?}");
                        assert_ne!(cwd, Path::new("/no-such-spawn"));
                    }
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_command_records_foreground_via_test_hook() {
        let mut session = three_pane_session();
        session.windows[0].panes[0].child_pid = Some(1);
        let prior = std::env::var_os(crate::procinfo::TEST_FOREGROUND_COMMAND_ENV);
        std::env::set_var(
            crate::procinfo::TEST_FOREGROUND_COMMAND_ENV,
            "claude --resume",
        );
        let saved = from_snapshot_with(&session, CwdSource::Live);
        match prior {
            Some(value) => std::env::set_var(crate::procinfo::TEST_FOREGROUND_COMMAND_ENV, value),
            None => std::env::remove_var(crate::procinfo::TEST_FOREGROUND_COMMAND_ENV),
        }
        match &saved.windows[0].root {
            SavedNode::Split { first, .. } => match first.as_ref() {
                SavedNode::Split { first, .. } => match first.as_ref() {
                    SavedNode::Leaf { command, .. } => {
                        assert_eq!(command.as_deref(), Some("claude --resume"));
                    }
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        let spawn = from_snapshot(&session);
        match &spawn.windows[0].root {
            SavedNode::Split { first, .. } => match first.as_ref() {
                SavedNode::Split { first, .. } => match first.as_ref() {
                    SavedNode::Leaf { command, .. } => {
                        assert!(
                            command.is_none(),
                            "layout save must not record command: {command:?}"
                        );
                    }
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn spaces_keep_creation_order_across_save_rename_and_legacy_files() {
        let dir = std::env::temp_dir().join(format!("prism-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut older = one_session_space("old");
        older.created_at_unix_ms = Some(10);
        save_space(&dir, "Zulu", &older).unwrap();
        let mut newer = one_session_space("new");
        newer.created_at_unix_ms = Some(20);
        save_space(&dir, "Alpha", &newer).unwrap();
        let names = || {
            list_spaces(&dir)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(), ["Zulu", "Alpha"]);
        older.created_at_unix_ms = None;
        older.saved_at_unix = 999;
        save_space(&dir, "Zulu", &older).unwrap();
        assert_eq!(
            load_space(&dir, "Zulu").unwrap().created_at_unix_ms,
            Some(10)
        );
        rename_space(&dir, "Zulu", "Renamed").unwrap();
        assert_eq!(names(), ["Renamed", "Alpha"]);
        // A legacy timestamp is fixed on its first save, independent of later saves.
        let legacy = dir.join("legacy.json");
        fs::write(&legacy, serde_json::to_vec(&older).unwrap()).unwrap();
        let creation = creation_time_ms(&legacy, &older);
        save_space(&dir, "legacy", &newer).unwrap();
        assert_eq!(
            load_space(&dir, "legacy").unwrap().created_at_unix_ms,
            Some(creation)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_space_order_round_trips_and_new_spaces_append() {
        let dir = std::env::temp_dir().join(format!(
            "prism-space-order-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        for (name, created_at) in [("Alpha", 10), ("Beta", 20), ("Gamma", 30)] {
            let mut space = one_session_space(name);
            space.created_at_unix_ms = Some(created_at);
            save_space(&dir, name, &space).unwrap();
        }

        let order = ["Gamma", "Alpha", "Beta"].map(str::to_owned);
        assert!(reorder_spaces(&dir, &order).unwrap());
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&fs::read(space_order_path(&dir)).unwrap())
                .unwrap(),
            order
        );
        assert_eq!(
            list_spaces(&dir)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            ["Gamma", "Alpha", "Beta"]
        );

        let mut new_space = one_session_space("Delta");
        new_space.created_at_unix_ms = Some(1);
        save_space(&dir, "Delta", &new_space).unwrap();
        assert_eq!(
            list_spaces(&dir)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            ["Gamma", "Alpha", "Beta", "Delta"]
        );
        let appended_order = ["Gamma", "Alpha", "Beta", "Delta"].map(str::to_owned);
        assert!(!reorder_spaces(&dir, &appended_order).unwrap());
        assert!(reorder_spaces(
            &dir,
            &["Gamma", "Alpha", "Alpha", "Delta"].map(str::to_owned)
        )
        .is_err());

        rename_space(&dir, "Delta", "Epsilon").unwrap();
        remove_space(&dir, "Gamma").unwrap();
        assert_eq!(
            list_spaces(&dir)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            ["Alpha", "Beta", "Epsilon"]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn space_round_trip_skips_empty_agent_and_lists() {
        let grok = three_pane_session();
        let mut fable = three_pane_session();
        fable.name = "fable-pc".into();
        fable.agent_id = Some("fable-pc".into());
        let space = from_sessions(&[&grok, &fable]);
        assert_eq!(space.version, 1);
        assert_eq!(space.sessions.len(), 2);
        assert_eq!(space.sessions[0].agent, None);
        assert_eq!(space.sessions[1].agent.as_deref(), Some("fable-pc"));
        let dir = std::env::temp_dir().join(format!(
            "prism-space-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = save_space(&dir, "today", &space).unwrap();
        assert_eq!(path, dir.join("today.json"));
        let loaded = load_space(&dir, "today").unwrap();
        assert_eq!(loaded.sessions[0].name, "work");
        assert_eq!(
            loaded.sessions[1].windows[0].root,
            space.sessions[1].windows[0].root
        );
        let list = list_spaces(&dir).unwrap();
        assert_eq!(
            list,
            vec![SpaceListEntry {
                name: "today".into(),
                sessions: 2,
                windows: 2,
                panes: 6,
                tabs: 0,
                saved_at_unix: space.saved_at_unix,
            }]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_focus_fields_default_when_absent_and_round_trip_when_set() {
        let grok = three_pane_session();
        let space = from_sessions(&[&grok]);
        assert_eq!(space.active_tab, 0);
        assert_eq!(space.focused_session, None);
        let raw = serde_json::to_string(&space).unwrap();
        assert!(!raw.contains("active_tab"), "{raw}");
        assert!(!raw.contains("focused_session"), "{raw}");
        let loaded: SavedSpace =
            serde_json::from_str(r#"{"version":1,"saved_at_unix":1,"sessions":[]}"#).unwrap();
        assert_eq!(loaded.active_tab, 0);
        assert_eq!(loaded.focused_session, None);

        let mut with_focus = space.clone();
        with_focus.active_tab = 1;
        with_focus.focused_session = Some("fable-pc".into());
        let encoded = serde_json::to_string(&with_focus).unwrap();
        assert!(encoded.contains("\"active_tab\":1"), "{encoded}");
        assert!(
            encoded.contains("\"focused_session\":\"fable-pc\""),
            "{encoded}"
        );
        let round: SavedSpace = serde_json::from_str(&encoded).unwrap();
        assert_eq!(round.active_tab, 1);
        assert_eq!(round.focused_session.as_deref(), Some("fable-pc"));
        assert_eq!(round.version, SAVED_SPACE_VERSION);
    }

    #[test]
    fn space_sessions_follow_tab_order_then_untabbed() {
        let space = SavedSpace {
            id: None,
            version: SAVED_SPACE_VERSION,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![
                SavedSpaceSession {
                    name: "a".into(),
                    agent: None,
                    windows: vec![],
                },
                SavedSpaceSession {
                    name: "b".into(),
                    agent: None,
                    windows: vec![],
                },
                SavedSpaceSession {
                    name: "c".into(),
                    agent: None,
                    windows: vec![],
                },
            ],
            tabs: vec![SavedSpaceTab {
                title: "t1".into(),
                sessions: vec!["b".into(), "a".into()],
                layout: None,
            }],
            active_tab: 0,
            focused_session: Some("a".into()),
        };
        assert_eq!(
            space_sessions_in_tab_order(&space),
            vec!["b".to_string(), "a".into(), "c".into()]
        );
        assert_eq!(space_active_session(&space).as_deref(), Some("a"));
        let mut no_focus = space.clone();
        no_focus.focused_session = None;
        assert_eq!(space_active_session(&no_focus).as_deref(), Some("b"));
        no_focus.tabs.clear();
        assert_eq!(
            space_sessions_in_tab_order(&no_focus),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
        assert_eq!(space_active_session(&no_focus).as_deref(), Some("a"));
    }

    #[test]
    fn space_bind_agent_falls_back_to_session_name() {
        assert_eq!(
            space_bind_agent(Some("grok-pc"), "ignored").as_deref(),
            Some("grok-pc")
        );
        assert_eq!(
            space_bind_agent(None, "grok-pc").as_deref(),
            Some("grok-pc")
        );
        assert_eq!(
            space_bind_agent(Some("  "), "fable-pc").as_deref(),
            Some("fable-pc")
        );
        assert_eq!(space_bind_agent(None, "").as_deref(), None);
    }

    #[test]
    fn save_space_replaces_the_file_without_leaving_a_partial_write() {
        let dir = std::env::temp_dir().join(format!(
            "prism-space-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desk.json");
        fs::write(&path, b"GARBAGE-NOT-A-SPACE-PARTIAL").unwrap();
        let mut session = stub_space_session("alpha");
        session.windows[0].title = "first".into();
        let first = SavedSpace {
            version: SAVED_SPACE_VERSION,
            id: Some("desk".into()),
            created_at_unix_ms: Some(42),
            saved_at_unix: 1,
            sessions: vec![session],
            tabs: vec![],
            active_tab: 0,
            focused_session: None,
        };
        save_space(&dir, "desk", &first).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("GARBAGE"), "{raw}");
        assert!(raw.ends_with('\n'));
        let loaded: SavedSpace = serde_json::from_str(&raw).unwrap();
        assert_eq!(loaded.sessions[0].windows[0].title, "first");
        assert_eq!(loaded.created_at_unix_ms, Some(42));

        let mut second = loaded;
        second.sessions[0].windows[0].title = "second".into();
        second.saved_at_unix = 9;
        save_space(&dir, "desk", &second).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("first"), "{raw}");
        assert!(raw.contains("second"), "{raw}");
        let loaded = load_space(&dir, "desk").unwrap();
        assert_eq!(loaded.sessions[0].windows[0].title, "second");
        assert_eq!(loaded.created_at_unix_ms, Some(42));
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .map(|entry| entry.path())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_space_deletes_file_and_rejects_bad_names() {
        let dir = std::env::temp_dir().join(format!(
            "prism-space-rm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let grok = three_pane_session();
        let space = from_sessions(&[&grok]);
        let path = save_space(&dir, "today", &space).unwrap();
        assert!(path.exists());
        let removed = remove_space(&dir, "today").unwrap();
        assert_eq!(removed, path);
        assert!(!path.exists());
        let err = remove_space(&dir, "today").unwrap_err().to_string();
        assert!(err.contains("today.json"), "{err}");
        assert!(remove_space(&dir, "foo/bar").is_err());
        assert!(remove_space(&dir, "..").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spaces_dir_mirrors_layouts_dir_sibling() {
        let xdg = spaces_dir_from(Some("/xdg".into()), Some("/home/me".into()));
        assert_eq!(xdg, PathBuf::from("/xdg/prismattyc/spaces"));
        let home = spaces_dir_from(None, Some("/home/me".into()));
        assert_eq!(
            home,
            PathBuf::from("/home/me/.local/share/prismattyc/spaces")
        );
    }

    #[test]
    fn spaces_dir_scopes_explicit_sockets_but_preserves_default() {
        let base = Path::new("/xdg/prismattyc/spaces");
        let first = Path::new("/tmp/pmux-a.sock");
        let second = Path::new("/tmp/pmux-b.sock");
        let same_basename_a = Path::new("/tmp/a/seat.sock");
        let same_basename_b = Path::new("/tmp/b/seat.other");

        assert_ne!(
            spaces_daemon_identity_path(same_basename_a),
            spaces_daemon_identity_path(same_basename_b)
        );
        assert_eq!(spaces_dir_for_identity("default", base), base);
        let first_dir = spaces_dir_for_socket_with_base(first, base);
        let second_dir = spaces_dir_for_socket_with_base(second, base);
        assert_ne!(first_dir, second_dir);
        assert!(first_dir.starts_with(base.join("instances")));
        assert!(second_dir.starts_with(base.join("instances")));
    }

    #[test]
    fn rename_space_moves_the_file_and_refuses_collisions() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-rename-space-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let space = SavedSpace {
            id: None,
            version: SAVED_SPACE_VERSION,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![SavedSpaceSession {
                name: "a".into(),
                agent: None,
                windows: vec![SavedWindow {
                    title: "a".into(),
                    cols: 80,
                    rows: 24,
                    root: SavedNode::Leaf {
                        cwd: None,
                        program: None,
                        command: None,
                        title: None,
                    },
                }],
            }],
            tabs: Vec::new(),
            active_tab: 0,
            focused_session: None,
        };
        save_space(&dir, "one", &space).unwrap();
        save_space(&dir, "two", &space).unwrap();
        let moved = rename_space(&dir, "one", "three").unwrap();
        assert_eq!(moved, dir.join("three.json"));
        assert!(!dir.join("one.json").exists());
        assert!(rename_space(&dir, "three", "two").is_err());
        assert!(dir.join("three.json").exists());
        assert!(rename_space(&dir, "missing", "four").is_err());
        assert!(rename_space(&dir, "two", "../evil").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    fn one_session_space(name: &str) -> SavedSpace {
        SavedSpace {
            id: None,
            version: SAVED_SPACE_VERSION,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![stub_space_session(name)],
            tabs: vec![SavedSpaceTab {
                title: name.into(),
                sessions: vec![name.into()],
                layout: None,
            }],
            active_tab: 0,
            focused_session: Some(name.into()),
        }
    }

    #[test]
    fn space_add_and_remove_session_table() {
        let mut space = one_session_space("keep");
        space_add_session(&mut space, stub_space_session("moved"), Some("Moved")).unwrap();
        assert_eq!(
            space
                .sessions
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["keep", "moved"]
        );
        assert_eq!(
            space.tabs.last().map(|tab| tab.title.as_str()),
            Some("Moved")
        );
        assert!(space_add_session(&mut space, stub_space_session("moved"), None).is_err());
        space_remove_session(&mut space, "moved").unwrap();
        assert_eq!(
            space
                .sessions
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["keep"]
        );
        assert!(space
            .tabs
            .iter()
            .all(|tab| !tab.sessions.contains(&"moved".into())));
        assert!(space_remove_session(&mut space, "keep").is_err());
        assert!(space_remove_session(&mut space, "ghost").is_err());
    }
}
