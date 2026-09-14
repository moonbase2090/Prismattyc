//! Domain root: sessions → windows → pane layout → panes.
//!
//! **Server topology only** (PRD §2.8.3): focus and active-tab selection are
//! per-client view state ([`ClientView`]), not stored on [`Window`]/[`Session`].

use std::collections::HashMap;

use crate::geometry::{self, suggested_focus_after_close};
use crate::ids::{ClientId, PaneId, SessionId, WindowId};
use crate::layout::PaneLayout;

/// Domain-level mutation or lookup failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    UnknownSession(SessionId),
    UnknownWindow(WindowId),
    UnknownPane(PaneId),
    EmptyName,
    /// Title/name failed CreateWindow rules (length or NUL).
    InvalidName,
    /// Last pane/window policy refused the destroy (named policies later).
    LastLeafRefused,
    /// An ID counter hit the end of the `u64` space (never reuses).
    IdSpaceExhausted,
    /// The same pane id appears more than once as a leaf in one layout.
    DuplicatePane(PaneId),
    /// Pane is already a leaf of a different window.
    PaneOwnedElsewhere {
        pane: PaneId,
        owner: WindowId,
    },
    /// Writable controller lease is held by another client (use takeover).
    LeaseHeld {
        holder: ClientId,
    },
    /// Caller is not the writable controller for this pane.
    NotController {
        holder: Option<ClientId>,
    },
    Geometry(crate::geometry::GeometryError),
    /// Another live session already holds this agent_id.
    AgentIdInUse {
        agent_id: String,
    },
    /// A transfer used an outdated or different Space owner.
    SpaceOwnerMismatch {
        session: SessionId,
        owner: Option<String>,
    },
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession(id) => write!(f, "unknown session {id}"),
            Self::UnknownWindow(id) => write!(f, "unknown window {id}"),
            Self::UnknownPane(id) => write!(f, "unknown pane {id}"),
            Self::EmptyName => write!(f, "name must be non-empty"),
            Self::InvalidName => write!(f, "name must be 1..=64 bytes and contain no NUL"),
            Self::LastLeafRefused => write!(f, "refused to destroy last leaf under active policy"),
            Self::IdSpaceExhausted => write!(f, "id space exhausted"),
            Self::DuplicatePane(id) => write!(f, "duplicate pane {id} in layout"),
            Self::PaneOwnedElsewhere { pane, owner } => {
                write!(f, "pane {pane} already owned by window {owner}")
            }
            Self::LeaseHeld { holder } => {
                write!(f, "controller lease held by {holder}")
            }
            Self::NotController { holder } => match holder {
                Some(id) => write!(f, "not controller (held by {id})"),
                None => write!(f, "not controller (lease free)"),
            },
            Self::Geometry(e) => write!(f, "geometry: {e}"),
            Self::AgentIdInUse { agent_id } => {
                write!(f, "agent_id {agent_id:?} already bound to a session")
            }
            Self::SpaceOwnerMismatch { session, owner } => {
                write!(
                    f,
                    "session {session} has Space owner {owner:?}; explicit transfer required"
                )
            }
        }
    }
}

impl std::error::Error for DomainError {}

/// Metadata for a pane leaf (runtime PTY binding is external).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub title: String,
    /// `pmux rename-pane` pin (PT-230). While true, child OSC 0/2 does not
    /// change `title` (tmux `allow-rename off`). An empty rename clears it.
    pub title_pinned: bool,
    /// Writable controller lease (PRD §2.8.4 — at most one per pane).
    pub controller: Option<ClientId>,
}

/// One tab within a session. Topology only — no focused pane (client-local).
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub id: WindowId,
    pub title: String,
    pub layout: PaneLayout,
    /// When true, typed input fans out to every pane in this window.
    pub sync_input: bool,
}

/// Named session containing windows. Active tab is client-local ([`ClientView`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<WindowId>,
    /// Mailbox address. `None` means the session is not a mail recipient
    /// (default, including the leftover `default` workspace).
    pub agent_id: Option<String>,
    /// Exclusive owner. None means this session has not joined a Space.
    pub space_id: Option<String>,
}

/// Per-attached-client view state (PRD §2.8.3). Not server topology.
///
/// Two clients may hold divergent focus and active-window selection over the
/// same domain snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientView {
    pub client: ClientId,
    pub session: Option<SessionId>,
    pub window: Option<WindowId>,
    /// Focused leaf per window this client cares about.
    pub pane_focus: HashMap<WindowId, PaneId>,
}

impl ClientView {
    pub fn new(client: ClientId) -> Self {
        Self {
            client,
            session: None,
            window: None,
            pane_focus: HashMap::new(),
        }
    }

    /// Seed view from a domain session (first window + first leaf when present).
    pub fn attach_session(domain: &Domain, client: ClientId, session: SessionId) -> Self {
        let mut v = Self::new(client);
        v.session = Some(session);
        if let Some(sess) = domain.session(session) {
            if let Some(&wid) = sess.windows.first() {
                v.window = Some(wid);
                if let Some(w) = domain.window(wid) {
                    if let Some(&pid) = w.layout.panes().first() {
                        v.pane_focus.insert(wid, pid);
                    }
                }
            }
        }
        v
    }

    pub fn focused_pane(&self, window: WindowId) -> Option<PaneId> {
        self.pane_focus.get(&window).copied()
    }

    pub fn set_focused_pane(&mut self, window: WindowId, pane: PaneId) {
        self.pane_focus.insert(window, pane);
    }
}

/// Server-side topology root. IDs minted here are never reused.
#[derive(Debug, Clone)]
pub struct Domain {
    /// Next raw id to mint; `0` means the space is exhausted (ids start at 1).
    next_session: u64,
    next_window: u64,
    next_pane: u64,
    next_client: u64,
    sessions: HashMap<SessionId, Session>,
    windows: HashMap<WindowId, Window>,
    panes: HashMap<PaneId, Pane>,
    /// Exclusive window ownership of each live pane leaf.
    pane_owner: HashMap<PaneId, WindowId>,
    /// Session insertion order for stable listing.
    session_order: Vec<SessionId>,
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

fn validated_window_title(title: &str) -> Result<String, DomainError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(DomainError::EmptyName);
    }
    if title.len() > 64 || title.contains('\0') {
        return Err(DomainError::InvalidName);
    }
    Ok(title.to_string())
}

fn validate_agent_id(agent_id: &str) -> Result<String, DomainError> {
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        return Err(DomainError::EmptyName);
    }
    if agent_id.len() > 64 || agent_id.contains('\0') {
        return Err(DomainError::InvalidName);
    }
    Ok(agent_id.to_string())
}

impl Domain {
    /// Empty domain (no sessions). Prefer [`Self::bootstrap`] for a ready workspace.
    pub fn new() -> Self {
        Self {
            next_session: 1,
            next_window: 1,
            next_pane: 1,
            next_client: 1,
            sessions: HashMap::new(),
            windows: HashMap::new(),
            panes: HashMap::new(),
            pane_owner: HashMap::new(),
            session_order: Vec::new(),
        }
    }

    /// Create a domain with one session, one window, and one leaf pane.
    pub fn bootstrap(session_name: &str) -> Result<Self, DomainError> {
        let mut d = Self::new();
        let sid = d.create_session(session_name)?;
        let _ = d.create_window(sid, "main")?;
        Ok(d)
    }

    fn mint_raw(next: &mut u64) -> Result<u64, DomainError> {
        let raw = *next;
        if raw == 0 {
            return Err(DomainError::IdSpaceExhausted);
        }
        // After minting `u64::MAX`, mark exhausted with 0 so the next call fails
        // without reusing the last id.
        *next = raw.checked_add(1).unwrap_or(0);
        Ok(raw)
    }

    fn mint_session(&mut self) -> Result<SessionId, DomainError> {
        Ok(SessionId::from_raw(Self::mint_raw(&mut self.next_session)?))
    }

    fn mint_window(&mut self) -> Result<WindowId, DomainError> {
        Ok(WindowId::from_raw(Self::mint_raw(&mut self.next_window)?))
    }

    fn mint_pane(&mut self) -> Result<PaneId, DomainError> {
        Ok(PaneId::from_raw(Self::mint_raw(&mut self.next_pane)?))
    }

    /// Mint a client id for lease tests / control plane.
    pub fn mint_client(&mut self) -> Result<ClientId, DomainError> {
        Ok(ClientId::from_raw(Self::mint_raw(&mut self.next_client)?))
    }

    pub fn create_session(&mut self, name: &str) -> Result<SessionId, DomainError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        let id = self.mint_session()?;
        self.sessions.insert(
            id,
            Session {
                id,
                name: name.to_string(),
                windows: Vec::new(),
                agent_id: None,
                space_id: None,
            },
        );
        self.session_order.push(id);
        Ok(id)
    }

    /// Transfer a set of sessions only when every expected owner still matches.
    /// Validation precedes mutation, so a stale batch cannot partially move.
    pub fn transfer_space_sessions(
        &mut self,
        sessions: &[SessionId],
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<(), DomainError> {
        if [from, to]
            .into_iter()
            .flatten()
            .any(|id| id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(DomainError::InvalidName);
        }
        for id in sessions {
            let session = self
                .sessions
                .get(id)
                .ok_or(DomainError::UnknownSession(*id))?;
            if session.space_id.as_deref() != from {
                return Err(DomainError::SpaceOwnerMismatch {
                    session: *id,
                    owner: session.space_id.clone(),
                });
            }
        }
        for id in sessions {
            self.sessions
                .get_mut(id)
                .expect("validated session")
                .space_id = to.map(str::to_owned);
        }
        Ok(())
    }

    /// Bind `agent_id` on an existing session. Unique across live sessions.
    pub fn set_agent_id(
        &mut self,
        id: SessionId,
        agent_id: Option<String>,
    ) -> Result<(), DomainError> {
        if !self.sessions.contains_key(&id) {
            return Err(DomainError::UnknownSession(id));
        }
        if let Some(ref agent) = agent_id {
            let agent = validate_agent_id(agent)?;
            if let Some(other) = self.session_by_agent(&agent) {
                if other != id {
                    return Err(DomainError::AgentIdInUse { agent_id: agent });
                }
            }
            self.sessions.get_mut(&id).expect("session exists").agent_id = Some(agent);
        } else {
            self.sessions.get_mut(&id).expect("session exists").agent_id = None;
        }
        Ok(())
    }

    /// Rename and bind one session without replacing its panes or processes.
    pub fn name_session(&mut self, id: SessionId, name: &str) -> Result<(), DomainError> {
        let name = validated_window_title(name)?;
        if self.sessions.values().any(|s| s.id != id && s.name == name) {
            return Err(DomainError::AgentIdInUse { agent_id: name });
        }
        self.set_agent_id(id, Some(name.clone()))?;
        self.sessions.get_mut(&id).expect("validated session").name = name;
        Ok(())
    }

    /// Live session that owns `agent_id`, if any.
    #[must_use]
    pub fn session_by_agent(&self, agent_id: &str) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|session| session.agent_id.as_deref() == Some(agent_id))
            .map(|session| session.id)
    }

    /// Create a window with a single leaf pane inside `session`.
    pub fn create_window(
        &mut self,
        session: SessionId,
        title: &str,
    ) -> Result<(WindowId, PaneId), DomainError> {
        let title = validated_window_title(title)?;
        if !self.sessions.contains_key(&session) {
            return Err(DomainError::UnknownSession(session));
        }
        let pane_id = self.mint_pane()?;
        self.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                title: String::new(),
                title_pinned: false,
                controller: None,
            },
        );
        let window_id = match self.mint_window() {
            Ok(w) => w,
            Err(e) => {
                self.panes.remove(&pane_id);
                return Err(e);
            }
        };
        self.windows.insert(
            window_id,
            Window {
                id: window_id,
                title,
                layout: PaneLayout::leaf(pane_id),
                sync_input: false,
            },
        );
        self.pane_owner.insert(pane_id, window_id);
        let sess = self.sessions.get_mut(&session).expect("checked");
        sess.windows.push(window_id);
        Ok((window_id, pane_id))
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.session_order
            .iter()
            .filter_map(|id| self.sessions.get(id))
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// Move `session.windows[from]` to index `to`. No-op when the
    /// indexes match or either is out of range.
    pub fn reorder_window(
        &mut self,
        session: SessionId,
        from: usize,
        to: usize,
    ) -> Result<bool, DomainError> {
        let sess = self
            .sessions
            .get_mut(&session)
            .ok_or(DomainError::UnknownSession(session))?;
        let n = sess.windows.len();
        if from >= n || to >= n || from == to {
            return Ok(false);
        }
        let id = sess.windows.remove(from);
        sess.windows.insert(to, id);
        Ok(true)
    }

    /// Take `pane` out of its owner and give it a new window at the
    /// end of `session`. Last leaf of the source window destroys that
    /// window. The pane id is preserved.
    pub fn open_window_with_pane(
        &mut self,
        session: SessionId,
        title: &str,
        pane: PaneId,
    ) -> Result<WindowId, DomainError> {
        let title = validated_window_title(title)?;
        if !self.sessions.contains_key(&session) {
            return Err(DomainError::UnknownSession(session));
        }
        let src = *self
            .pane_owner
            .get(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        let src_layout = self
            .windows
            .get(&src)
            .ok_or(DomainError::UnknownWindow(src))?
            .layout
            .clone();
        if !src_layout.contains_pane(pane) {
            return Err(DomainError::UnknownPane(pane));
        }
        let src_is_last = matches!(src_layout, PaneLayout::Leaf(id) if id == pane);
        let window_id = self.mint_window()?;
        if !src_is_last {
            let src_new =
                geometry::close_pane_in_layout(&src_layout, pane).map_err(DomainError::Geometry)?;
            self.set_layout(src, src_new)?;
        }
        self.windows.insert(
            window_id,
            Window {
                id: window_id,
                title,
                layout: PaneLayout::leaf(pane),
                sync_input: false,
            },
        );
        self.pane_owner.insert(pane, window_id);
        self.sessions
            .get_mut(&session)
            .expect("checked")
            .windows
            .push(window_id);
        if src_is_last {
            if let Err(error) = self.destroy_window(src) {
                self.pane_owner.insert(pane, src);
                self.windows.remove(&window_id);
                if let Some(sess) = self.sessions.get_mut(&session) {
                    sess.windows.retain(|id| *id != window_id);
                }
                return Err(error);
            }
        }
        Ok(window_id)
    }

    /// Set a pane's title (PT-128). Empty clears it back to the default
    /// and unpins OSC 0/2 (PT-230). A non-empty rename pins the title.
    pub fn rename_pane(&mut self, pane: PaneId, title: &str) -> Result<(), DomainError> {
        let entry = self
            .panes
            .get_mut(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        entry.title = title.to_string();
        entry.title_pinned = !title.is_empty();
        Ok(())
    }

    /// Set a window title. Same rules as [`Self::create_window`].
    pub fn rename_window(&mut self, window: WindowId, title: &str) -> Result<(), DomainError> {
        let title = validated_window_title(title)?;
        let win = self
            .windows
            .get_mut(&window)
            .ok_or(DomainError::UnknownWindow(window))?;
        win.title = title;
        Ok(())
    }

    /// Enable or disable input fan-out for every pane in `window`.
    pub fn set_sync_input(&mut self, window: WindowId, enabled: bool) -> Result<(), DomainError> {
        let win = self
            .windows
            .get_mut(&window)
            .ok_or(DomainError::UnknownWindow(window))?;
        win.sync_input = enabled;
        Ok(())
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Window that currently owns `pane` as a layout leaf, if any.
    pub fn pane_owner(&self, pane: PaneId) -> Option<WindowId> {
        self.pane_owner.get(&pane).copied()
    }

    /// Current writable controller for `pane`, if any.
    pub fn controller(&self, pane: PaneId) -> Result<Option<ClientId>, DomainError> {
        Ok(self
            .panes
            .get(&pane)
            .ok_or(DomainError::UnknownPane(pane))?
            .controller)
    }

    /// Acquire a free controller lease. Fails if another client holds it.
    pub fn acquire_controller(
        &mut self,
        pane: PaneId,
        client: ClientId,
    ) -> Result<(), DomainError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        match p.controller {
            None => {
                p.controller = Some(client);
                Ok(())
            }
            Some(holder) if holder == client => Ok(()),
            Some(holder) => Err(DomainError::LeaseHeld { holder }),
        }
    }

    /// Release the caller's controller lease. Fails if `client` does not hold it.
    pub fn release_controller(
        &mut self,
        pane: PaneId,
        client: ClientId,
    ) -> Result<(), DomainError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        match p.controller {
            Some(holder) if holder == client => {
                p.controller = None;
                Ok(())
            }
            holder => Err(DomainError::NotController { holder }),
        }
    }

    /// Explicit takeover: replace any holder with `client`. Returns previous holder.
    pub fn takeover_controller(
        &mut self,
        pane: PaneId,
        client: ClientId,
    ) -> Result<Option<ClientId>, DomainError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        let previous = p.controller;
        p.controller = Some(client);
        Ok(previous)
    }

    /// Clear the lease without knowing the holder (disconnect/admin path).
    pub fn clear_controller(&mut self, pane: PaneId) -> Result<Option<ClientId>, DomainError> {
        let p = self
            .panes
            .get_mut(&pane)
            .ok_or(DomainError::UnknownPane(pane))?;
        Ok(p.controller.take())
    }

    /// Drop every lease held by `client`. Returns pane ids that changed.
    pub fn release_all_controller_leases(&mut self, client: ClientId) -> Vec<PaneId> {
        let mut released = Vec::new();
        for (id, pane) in &mut self.panes {
            if pane.controller == Some(client) {
                pane.controller = None;
                released.push(*id);
            }
        }
        released
    }

    /// Require that `client` is the writable controller (observer rejection).
    pub fn require_controller(&self, pane: PaneId, client: ClientId) -> Result<(), DomainError> {
        let holder = self.controller(pane)?;
        if holder == Some(client) {
            Ok(())
        } else {
            Err(DomainError::NotController { holder })
        }
    }

    /// Allow a WritePane when `client` holds the lease, or when the pane has
    /// no controller (lease-free one-shot). Reject observers while a holder
    /// exists.
    pub fn allow_write(&self, pane: PaneId, client: ClientId) -> Result<(), DomainError> {
        match self.controller(pane)? {
            Some(holder) if holder == client => Ok(()),
            None => Ok(()),
            Some(holder) => Err(DomainError::NotController {
                holder: Some(holder),
            }),
        }
    }

    /// Low-level controller write (prefer acquire/release/takeover).
    ///
    /// `Some(client)` uses explicit takeover; `None` clears the lease.
    pub fn set_controller(
        &mut self,
        pane: PaneId,
        controller: Option<ClientId>,
    ) -> Result<(), DomainError> {
        match controller {
            Some(client) => {
                self.takeover_controller(pane, client)?;
                Ok(())
            }
            None => {
                self.clear_controller(pane)?;
                Ok(())
            }
        }
    }

    /// Destroy a session and all of its windows/panes. IDs are never reissued.
    pub fn destroy_session(&mut self, id: SessionId) -> Result<(), DomainError> {
        let sess = self
            .sessions
            .remove(&id)
            .ok_or(DomainError::UnknownSession(id))?;
        self.session_order.retain(|s| *s != id);
        for wid in sess.windows {
            self.destroy_window_inner(wid);
        }
        Ok(())
    }

    /// Destroy a window and its panes. Removes from parent session.
    ///
    /// **Default empty-session policy (ADR-0007):** when the session has no
    /// windows left, the session is destroyed.
    pub fn destroy_window(&mut self, id: WindowId) -> Result<(), DomainError> {
        if !self.windows.contains_key(&id) {
            return Err(DomainError::UnknownWindow(id));
        }
        let mut empty_session: Option<SessionId> = None;
        for sess in self.sessions.values_mut() {
            if let Some(pos) = sess.windows.iter().position(|w| *w == id) {
                sess.windows.remove(pos);
                if sess.windows.is_empty() {
                    empty_session = Some(sess.id);
                }
                break;
            }
        }
        self.destroy_window_inner(id);
        if let Some(sid) = empty_session {
            // Windows already gone; drop the empty session shell.
            self.sessions.remove(&sid);
            self.session_order.retain(|s| *s != sid);
        }
        Ok(())
    }

    fn destroy_window_inner(&mut self, id: WindowId) {
        if let Some(w) = self.windows.remove(&id) {
            for pane in w.layout.panes() {
                // A pane already claimed by another window (move_pane handoff)
                // keeps its record and lease. Never free_pane the moved leaf.
                match self.pane_owner.get(&pane) {
                    Some(owner) if *owner != id => {}
                    _ => {
                        self.pane_owner.remove(&pane);
                        self.panes.remove(&pane);
                    }
                }
            }
        }
    }

    /// Replace window layout. Validates leaves exist, are unique within the tree,
    /// and are not owned by another window. Does not free panes that leave the
    /// tree — caller must [`Self::free_pane`].
    pub fn set_layout(&mut self, window: WindowId, layout: PaneLayout) -> Result<(), DomainError> {
        if !self.windows.contains_key(&window) {
            return Err(DomainError::UnknownWindow(window));
        }
        let new_panes = layout.panes();
        {
            let mut seen = std::collections::HashSet::with_capacity(new_panes.len());
            for &pane in &new_panes {
                if !seen.insert(pane) {
                    return Err(DomainError::DuplicatePane(pane));
                }
            }
        }
        for &pane in &new_panes {
            if !self.panes.contains_key(&pane) {
                return Err(DomainError::UnknownPane(pane));
            }
            if let Some(owner) = self.pane_owner.get(&pane) {
                if *owner != window {
                    return Err(DomainError::PaneOwnedElsewhere {
                        pane,
                        owner: *owner,
                    });
                }
            }
        }
        // Drop ownership for panes leaving this window's layout.
        let old_panes = self.windows.get(&window).expect("checked").layout.panes();
        for pane in old_panes {
            if !layout.contains_pane(pane) {
                self.pane_owner.remove(&pane);
            }
        }
        for &pane in &new_panes {
            self.pane_owner.insert(pane, window);
        }
        self.windows.get_mut(&window).expect("checked").layout = layout;
        Ok(())
    }

    /// Mint a new pane record (not yet placed in any layout). Prefer
    /// [`Self::split_pane`] which preserves ownership invariants.
    pub fn alloc_pane(&mut self, title: &str) -> Result<PaneId, DomainError> {
        let id = self.mint_pane()?;
        self.panes.insert(
            id,
            Pane {
                id,
                title: title.to_string(),
                title_pinned: false,
                controller: None,
            },
        );
        Ok(id)
    }

    /// Drop a pane record that is no longer referenced by any layout.
    pub fn free_pane(&mut self, id: PaneId) -> Result<(), DomainError> {
        for w in self.windows.values() {
            if w.layout.contains_pane(id) {
                return Err(DomainError::LastLeafRefused);
            }
        }
        self.pane_owner.remove(&id);
        self.panes.remove(&id).ok_or(DomainError::UnknownPane(id))?;
        Ok(())
    }

    /// Split `target` leaf into two panes. Allocates a new pane id.
    ///
    /// When `probe` is `Some((cols, rows, min_cols, min_rows))`, the resulting
    /// layout must satisfy minima for that window size or the mutation is
    /// refused atomically (no new pane retained, topology unchanged).
    pub fn split_pane(
        &mut self,
        window: WindowId,
        target: PaneId,
        axis: crate::layout::Axis,
        ratio: f64,
        probe: Option<(usize, usize, usize, usize)>,
    ) -> Result<PaneId, DomainError> {
        let layout = self
            .windows
            .get(&window)
            .ok_or(DomainError::UnknownWindow(window))?
            .layout
            .clone();

        // Structural pre-checks before minting so failed splits leave no orphan.
        if !(ratio > 0.0 && ratio < 1.0) {
            return Err(DomainError::Geometry(geometry::GeometryError::InvalidRatio));
        }
        if !layout.contains_pane(target) {
            return Err(DomainError::Geometry(geometry::GeometryError::UnknownPane(
                target,
            )));
        }

        let new_pane = self.alloc_pane("")?;
        let rollback_pane = |this: &mut Self, pane: PaneId| {
            this.panes.remove(&pane);
            this.pane_owner.remove(&pane);
        };

        let new_layout = match geometry::split_leaf(&layout, target, new_pane, axis, ratio) {
            Ok(l) => l,
            Err(e) => {
                rollback_pane(self, new_pane);
                return Err(DomainError::Geometry(e));
            }
        };
        if let Some((cols, rows, min_c, min_r)) = probe {
            let bounds = geometry::CellRect {
                col: 0,
                row: 0,
                cols,
                rows,
            };
            if let Err(e) = geometry::layout_to_rects(&new_layout, bounds, min_c, min_r) {
                rollback_pane(self, new_pane);
                return Err(DomainError::Geometry(e));
            }
        }
        if let Err(e) = self.set_layout(window, new_layout) {
            rollback_pane(self, new_pane);
            return Err(e);
        }
        Ok(new_pane)
    }

    /// Move `pane` from `src_window` onto `dst_target_leaf` in `dst_window`.
    ///
    /// Clones both layouts, probes each tree against its own bounds, then
    /// commits both `set_layout` calls only after both probes pass. Ownership
    /// handoff is `set_layout`'s pane_owner release/claim — the moved pane is
    /// never `free_pane`'d. Same-window moves are refused. Last pane of the
    /// source follows the existing collapse policy (source window destroyed;
    /// empty session dies).
    ///
    /// Returns the suggested focus for the surviving source window, or `None`
    /// when the source window was destroyed.
    #[allow(clippy::too_many_arguments)] // ticket contract
    pub fn move_pane(
        &mut self,
        src_window: WindowId,
        dst_window: WindowId,
        pane: PaneId,
        dst_target_leaf: PaneId,
        axis: crate::layout::Axis,
        ratio: f64,
        probe_src_bounds: Option<(usize, usize, usize, usize)>,
        probe_dst_bounds: Option<(usize, usize, usize, usize)>,
    ) -> Result<Option<PaneId>, DomainError> {
        if src_window == dst_window {
            return Err(DomainError::Geometry(geometry::GeometryError::UnknownPane(
                pane,
            )));
        }
        if !(ratio > 0.0 && ratio < 1.0) {
            return Err(DomainError::Geometry(geometry::GeometryError::InvalidRatio));
        }

        let src_layout = self
            .windows
            .get(&src_window)
            .ok_or(DomainError::UnknownWindow(src_window))?
            .layout
            .clone();
        let dst_layout = self
            .windows
            .get(&dst_window)
            .ok_or(DomainError::UnknownWindow(dst_window))?
            .layout
            .clone();

        if !src_layout.contains_pane(pane) {
            return Err(DomainError::Geometry(geometry::GeometryError::UnknownPane(
                pane,
            )));
        }
        if !dst_layout.contains_pane(dst_target_leaf) {
            return Err(DomainError::Geometry(geometry::GeometryError::UnknownPane(
                dst_target_leaf,
            )));
        }

        let src_is_last_leaf = matches!(src_layout, PaneLayout::Leaf(id) if id == pane);
        let src_new = if src_is_last_leaf {
            None
        } else {
            Some(geometry::close_pane_in_layout(&src_layout, pane).map_err(DomainError::Geometry)?)
        };
        let dst_new = geometry::split_leaf(&dst_layout, dst_target_leaf, pane, axis, ratio)
            .map_err(DomainError::Geometry)?;

        if let Some(src_new) = src_new.as_ref() {
            probe_layout(src_new, probe_src_bounds)?;
        }
        probe_layout(&dst_new, probe_dst_bounds)?;

        let suggested = src_new
            .as_ref()
            .map(|layout| suggested_focus_after_close(pane, pane, layout));

        if let Some(src_new) = src_new {
            self.set_layout(src_window, src_new)?;
            if let Err(error) = self.set_layout(dst_window, dst_new) {
                let _ = self.set_layout(src_window, src_layout);
                return Err(error);
            }
        } else {
            self.pane_owner.remove(&pane);
            if let Err(error) = self.set_layout(dst_window, dst_new) {
                self.pane_owner.insert(pane, src_window);
                return Err(error);
            }
            self.destroy_window(src_window)?;
        }
        Ok(suggested)
    }

    /// Close a pane; collapse parent. Frees the pane id. Refuses last pane.
    ///
    /// Returns a **suggested** focus target for client view state (not stored
    /// server-side). Pass `prior_focus` from the acting client's [`ClientView`].
    pub fn close_pane(
        &mut self,
        window: WindowId,
        pane: PaneId,
        prior_focus: PaneId,
    ) -> Result<PaneId, DomainError> {
        let layout = self
            .windows
            .get(&window)
            .ok_or(DomainError::UnknownWindow(window))?
            .layout
            .clone();
        let new_layout =
            geometry::close_pane_in_layout(&layout, pane).map_err(DomainError::Geometry)?;
        let focus = suggested_focus_after_close(prior_focus, pane, &new_layout);
        self.set_layout(window, new_layout)?;
        self.panes.remove(&pane);
        self.pane_owner.remove(&pane);
        Ok(focus)
    }

    /// Change the split ratio of the parent of `pane`.
    pub fn resize_parent_split(
        &mut self,
        window: WindowId,
        pane: PaneId,
        ratio: f64,
        probe: Option<(usize, usize, usize, usize)>,
    ) -> Result<(), DomainError> {
        let layout = self
            .windows
            .get(&window)
            .ok_or(DomainError::UnknownWindow(window))?
            .layout
            .clone();
        let new_layout =
            geometry::set_parent_ratio(&layout, pane, ratio).map_err(DomainError::Geometry)?;
        if let Some((cols, rows, min_c, min_r)) = probe {
            let bounds = geometry::CellRect {
                col: 0,
                row: 0,
                cols,
                rows,
            };
            geometry::layout_to_rects(&new_layout, bounds, min_c, min_r)
                .map_err(DomainError::Geometry)?;
        }
        self.set_layout(window, new_layout)?;
        Ok(())
    }

    /// Highest minted raw values (for tests asserting non-reuse after destroy).
    ///
    /// Returns the next-to-issue watermark; `0` means that id space is exhausted.
    pub fn id_watermarks(&self) -> (u64, u64, u64, u64) {
        (
            self.next_session,
            self.next_window,
            self.next_pane,
            self.next_client,
        )
    }
}

fn probe_layout(
    layout: &PaneLayout,
    probe: Option<(usize, usize, usize, usize)>,
) -> Result<(), DomainError> {
    let Some((cols, rows, min_c, min_r)) = probe else {
        return Ok(());
    };
    geometry::layout_to_rects(
        layout,
        geometry::CellRect {
            col: 0,
            row: 0,
            cols,
            rows,
        },
        min_c,
        min_r,
    )
    .map_err(DomainError::Geometry)?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn space_transfer_is_exclusive_and_checks_the_entire_batch() {
        let mut domain = Domain::bootstrap("first").unwrap();
        let first = domain.sessions().next().unwrap().id;
        let second = domain.create_session("second").unwrap();
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let windows = domain.session(first).unwrap().windows.clone();
        domain
            .transfer_space_sessions(&[first], None, Some(a))
            .unwrap();
        assert!(domain
            .transfer_space_sessions(&[second, first], None, Some(b))
            .is_err());
        assert_eq!(domain.session(second).unwrap().space_id, None);
        assert_eq!(domain.session(first).unwrap().space_id.as_deref(), Some(a));
        domain
            .transfer_space_sessions(&[first], Some(a), Some(b))
            .unwrap();
        assert_eq!(domain.session(first).unwrap().space_id.as_deref(), Some(b));
        assert_eq!(domain.session(first).unwrap().windows, windows);
        assert!(domain
            .transfer_space_sessions(&[first], Some(a), None)
            .is_err());
    }
    use super::*;
    use crate::layout::{Axis, Split};

    #[test]
    fn bootstrap_single_leaf() {
        let d = Domain::bootstrap("work").unwrap();
        let s = d.sessions().next().unwrap();
        assert_eq!(s.name, "work");
        assert_eq!(s.windows.len(), 1);
        let wid = s.windows[0];
        let w = d.window(wid).unwrap();
        assert_eq!(w.layout.pane_count(), 1);
        assert!(matches!(w.layout, PaneLayout::Leaf(_)));
        let view = ClientView::attach_session(&d, ClientId::from_raw(1), s.id);
        assert_eq!(view.window, Some(wid));
        assert_eq!(view.focused_pane(wid), Some(w.layout.panes()[0]));
    }

    #[test]
    fn ids_never_reused_after_destroy() {
        let mut d = Domain::bootstrap("a").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let (wid, pid) = {
            let s = d.session(sid).unwrap();
            let wid = s.windows[0];
            let pid = d.window(wid).unwrap().layout.panes()[0];
            (wid, pid)
        };
        let marks_before = d.id_watermarks();
        d.destroy_session(sid).unwrap();
        assert!(d.session(sid).is_none());
        assert!(d.window(wid).is_none());
        assert!(d.pane(pid).is_none());

        let sid2 = d.create_session("b").unwrap();
        let (wid2, pid2) = d.create_window(sid2, "t").unwrap();
        assert_ne!(sid2, sid);
        assert_ne!(wid2, wid);
        assert_ne!(pid2, pid);
        let marks_after = d.id_watermarks();
        // Counters only move forward (or stay exhausted at 0).
        assert!(marks_after.0 == 0 || marks_after.0 >= marks_before.0);
        assert!(marks_after.1 == 0 || marks_after.1 >= marks_before.1);
        assert!(marks_after.2 == 0 || marks_after.2 >= marks_before.2);
    }

    #[test]
    fn id_spaces_exhaust_without_reuse() {
        // Drive every counter to the last representable id, then assert exhaustion.
        for space in 0..4u8 {
            let mut d = Domain::new();
            match space {
                0 => d.next_session = u64::MAX,
                1 => d.next_window = u64::MAX,
                2 => d.next_pane = u64::MAX,
                3 => d.next_client = u64::MAX,
                _ => unreachable!(),
            }
            match space {
                0 => {
                    let a = d.mint_session().unwrap();
                    assert_eq!(a.get(), u64::MAX);
                    assert_eq!(d.mint_session(), Err(DomainError::IdSpaceExhausted));
                    assert_eq!(d.mint_session(), Err(DomainError::IdSpaceExhausted));
                }
                1 => {
                    let a = d.mint_window().unwrap();
                    assert_eq!(a.get(), u64::MAX);
                    assert_eq!(d.mint_window(), Err(DomainError::IdSpaceExhausted));
                }
                2 => {
                    let a = d.mint_pane().unwrap();
                    assert_eq!(a.get(), u64::MAX);
                    assert_eq!(d.mint_pane(), Err(DomainError::IdSpaceExhausted));
                }
                3 => {
                    let a = d.mint_client().unwrap();
                    assert_eq!(a.get(), u64::MAX);
                    assert_eq!(d.mint_client(), Err(DomainError::IdSpaceExhausted));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn controller_lease_per_pane() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sess = d.sessions().next().unwrap();
        let pid = d.window(sess.windows[0]).unwrap().layout.panes()[0];
        let c1 = d.mint_client().unwrap();
        let c2 = d.mint_client().unwrap();
        d.acquire_controller(pid, c1).unwrap();
        assert_eq!(d.controller(pid).unwrap(), Some(c1));
        // Second client cannot acquire without explicit takeover.
        assert_eq!(
            d.acquire_controller(pid, c2),
            Err(DomainError::LeaseHeld { holder: c1 })
        );
        assert_eq!(
            d.require_controller(pid, c2),
            Err(DomainError::NotController { holder: Some(c1) })
        );
        d.require_controller(pid, c1).unwrap();
        d.allow_write(pid, c1).unwrap();
        assert_eq!(
            d.allow_write(pid, c2),
            Err(DomainError::NotController { holder: Some(c1) })
        );
        d.release_controller(pid, c1).unwrap();
        d.allow_write(pid, c2).unwrap();
        d.acquire_controller(pid, c1).unwrap();
        // Explicit takeover replaces; at most one writer remains.
        assert_eq!(d.takeover_controller(pid, c2).unwrap(), Some(c1));
        assert_eq!(d.controller(pid).unwrap(), Some(c2));
        d.release_controller(pid, c2).unwrap();
        assert_eq!(d.controller(pid).unwrap(), None);
        // Release by non-holder fails when free.
        assert_eq!(
            d.release_controller(pid, c1),
            Err(DomainError::NotController { holder: None })
        );
    }

    #[test]
    fn release_all_controller_leases_on_disconnect() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let wid = d.session(sid).unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let p1 = d
            .split_pane(wid, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let c1 = d.mint_client().unwrap();
        let c2 = d.mint_client().unwrap();
        d.acquire_controller(p0, c1).unwrap();
        d.acquire_controller(p1, c2).unwrap();
        let released = d.release_all_controller_leases(c1);
        assert_eq!(released, vec![p0]);
        assert_eq!(d.controller(p0).unwrap(), None);
        assert_eq!(d.controller(p1).unwrap(), Some(c2));
    }

    #[test]
    fn set_layout_requires_known_panes() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let p1 = d.alloc_pane("second").unwrap();
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p0)),
            second: Box::new(PaneLayout::leaf(p1)),
        });
        d.set_layout(wid, layout).unwrap();
        assert_eq!(d.window(wid).unwrap().layout.pane_count(), 2);
        assert_eq!(d.pane_owner(p1), Some(wid));
    }

    #[test]
    fn set_layout_rejects_pane_owned_by_other_window() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let wid0 = d.session(sid).unwrap().windows[0];
        let p0 = d.window(wid0).unwrap().layout.panes()[0];
        let (wid1, p1) = d.create_window(sid, "other").unwrap();
        // Attempt to place p0 (owned by wid0) into wid1.
        let layout = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p1)),
            second: Box::new(PaneLayout::leaf(p0)),
        });
        let err = d.set_layout(wid1, layout).unwrap_err();
        assert!(matches!(
            err,
            DomainError::PaneOwnedElsewhere { pane, owner }
            if pane == p0 && owner == wid0
        ));
        // wid1 topology unchanged.
        assert_eq!(d.window(wid1).unwrap().layout.pane_count(), 1);
    }

    #[test]
    fn empty_name_rejected() {
        let mut d = Domain::new();
        assert_eq!(d.create_session("  "), Err(DomainError::EmptyName));
    }

    #[test]
    fn agent_id_defaults_none_and_is_unique() {
        let mut d = Domain::new();
        let a = d.create_session("work").unwrap();
        let b = d.create_session("other").unwrap();
        assert_eq!(d.session(a).unwrap().agent_id, None);
        assert_eq!(d.session_by_agent("operator-a"), None);

        d.set_agent_id(a, Some("operator-a".into())).unwrap();
        assert_eq!(
            d.session(a).unwrap().agent_id.as_deref(),
            Some("operator-a")
        );
        assert_eq!(d.session_by_agent("operator-a"), Some(a));

        let err = d.set_agent_id(b, Some("operator-a".into())).unwrap_err();
        assert_eq!(
            err,
            DomainError::AgentIdInUse {
                agent_id: "operator-a".into()
            }
        );
        d.set_agent_id(b, Some("operator-b".into())).unwrap();
        assert_eq!(d.session_by_agent("operator-b"), Some(b));

        d.destroy_session(a).unwrap();
        assert_eq!(d.session_by_agent("operator-a"), None);
        d.set_agent_id(b, Some("operator-a".into())).unwrap();
        assert_eq!(d.session_by_agent("operator-a"), Some(b));
    }

    #[test]
    fn free_pane_while_in_layout_refused() {
        let mut d = Domain::bootstrap("s").unwrap();
        let pid = d
            .window(d.sessions().next().unwrap().windows[0])
            .unwrap()
            .layout
            .panes()[0];
        assert_eq!(d.free_pane(pid), Err(DomainError::LastLeafRefused));
    }

    #[test]
    fn destroy_last_window_destroys_session() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let wid = d.session(sid).unwrap().windows[0];
        d.destroy_window(wid).unwrap();
        assert!(d.session(sid).is_none());
        assert!(d.window(wid).is_none());
        assert!(d.sessions().next().is_none());
    }

    #[test]
    fn destroy_window_keeps_nonempty_session() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let wid0 = d.session(sid).unwrap().windows[0];
        let (wid1, _) = d.create_window(sid, "two").unwrap();
        d.destroy_window(wid0).unwrap();
        assert!(d.session(sid).is_some());
        assert_eq!(d.session(sid).unwrap().windows, vec![wid1]);
        assert!(d.window(wid0).is_none());
    }

    #[test]
    fn split_close_three_panes() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let p1 = d
            .split_pane(wid, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let p2 = d
            .split_pane(wid, p1, Axis::Vertical, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        assert_eq!(d.window(wid).unwrap().layout.pane_count(), 3);
        let focus = d.close_pane(wid, p1, p1).unwrap();
        assert!(!d.window(wid).unwrap().layout.contains_pane(p1));
        assert!(d.pane(p1).is_none());
        assert!(d.window(wid).unwrap().layout.contains_pane(p0));
        assert!(d.window(wid).unwrap().layout.contains_pane(p2));
        assert!(d.window(wid).unwrap().layout.contains_pane(focus));
    }

    #[test]
    fn split_probe_too_small_atomic() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let before = d.id_watermarks().2;
        let err = d
            .split_pane(wid, p0, Axis::Horizontal, 0.5, Some((3, 10, 2, 1)))
            .unwrap_err();
        assert!(matches!(err, DomainError::Geometry(_)));
        assert_eq!(d.window(wid).unwrap().layout.pane_count(), 1);
        // Pane counter advanced but orphaned pane must not remain in map.
        assert_eq!(d.panes.len(), 1);
        assert!(d.pane_owner.len() == 1);
        assert!(d.id_watermarks().2 == 0 || d.id_watermarks().2 >= before);
    }

    #[test]
    fn split_invalid_ratio_atomic_no_orphan() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let panes_before = d.panes.len();
        let err = d
            .split_pane(wid, p0, Axis::Horizontal, 0.0, None)
            .unwrap_err();
        assert!(matches!(err, DomainError::Geometry(_)));
        assert_eq!(d.panes.len(), panes_before);
        assert_eq!(d.window(wid).unwrap().layout.pane_count(), 1);
    }

    #[test]
    fn split_unknown_target_atomic_no_orphan() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let ghost = PaneId::from_raw(999_999);
        let panes_before = d.panes.len();
        let err = d
            .split_pane(wid, ghost, Axis::Horizontal, 0.5, None)
            .unwrap_err();
        assert!(matches!(err, DomainError::Geometry(_)));
        assert_eq!(d.panes.len(), panes_before);
    }

    #[test]
    fn client_local_divergent_focus() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let wid = d.session(sid).unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let p1 = d
            .split_pane(wid, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();

        let c1 = d.mint_client().unwrap();
        let c2 = d.mint_client().unwrap();
        let mut v1 = ClientView::attach_session(&d, c1, sid);
        let mut v2 = ClientView::attach_session(&d, c2, sid);
        v1.set_focused_pane(wid, p0);
        v2.set_focused_pane(wid, p1);
        assert_eq!(v1.focused_pane(wid), Some(p0));
        assert_eq!(v2.focused_pane(wid), Some(p1));
        // Server topology has no single global focus field.
        // (Window no longer carries focused_pane.)
        let _ = d.window(wid).unwrap().layout;
    }

    #[test]
    fn set_layout_rejects_duplicate_pane_id() {
        let mut d = Domain::bootstrap("s").unwrap();
        let wid = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(wid).unwrap().layout.panes()[0];
        let dup = PaneLayout::Split(Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::leaf(p0)),
            second: Box::new(PaneLayout::leaf(p0)),
        });
        let err = d.set_layout(wid, dup).unwrap_err();
        assert_eq!(err, DomainError::DuplicatePane(p0));
        // Topology unchanged: still a single leaf.
        assert_eq!(d.window(wid).unwrap().layout.pane_count(), 1);
        assert!(matches!(
            d.window(wid).unwrap().layout,
            PaneLayout::Leaf(id) if id == p0
        ));
    }

    fn assert_geometry_conserved(layout: &PaneLayout, cols: usize, rows: usize) {
        let rects = geometry::layout_to_rects(
            layout,
            geometry::CellRect {
                col: 0,
                row: 0,
                cols,
                rows,
            },
            geometry::DEFAULT_MIN_COLS,
            geometry::DEFAULT_MIN_ROWS,
        )
        .unwrap();
        let mut cells = std::collections::HashSet::new();
        for (_, rect) in &rects {
            assert!(rect.cols >= geometry::DEFAULT_MIN_COLS);
            assert!(rect.rows >= geometry::DEFAULT_MIN_ROWS);
            for col in rect.col..rect.col + rect.cols {
                for row in rect.row..rect.row + rect.rows {
                    assert!(cells.insert((col, row)), "overlap at ({col},{row})");
                }
            }
        }
        assert_eq!(cells.len(), cols * rows);
    }

    #[test]
    fn move_pane_preserves_identity_and_controller() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let src = d.session(sid).unwrap().windows[0];
        let p0 = d.window(src).unwrap().layout.panes()[0];
        let moved = d
            .split_pane(src, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let (dst, target) = d.create_window(sid, "dst").unwrap();
        let client = d.mint_client().unwrap();
        d.acquire_controller(moved, client).unwrap();
        let title = d.pane(moved).unwrap().title.clone();

        let focus = d
            .move_pane(
                src,
                dst,
                moved,
                target,
                Axis::Vertical,
                0.5,
                Some((80, 24, 2, 1)),
                Some((80, 24, 2, 1)),
            )
            .unwrap();
        assert_eq!(focus, Some(p0));
        assert!(d.pane(moved).is_some());
        assert_eq!(d.pane(moved).unwrap().id, moved);
        assert_eq!(d.pane(moved).unwrap().controller, Some(client));
        assert_eq!(d.pane(moved).unwrap().title, title);
        assert_eq!(d.controller(moved).unwrap(), Some(client));
        assert_eq!(d.pane_owner(moved), Some(dst));
        assert_eq!(d.pane_owner(p0), Some(src));
        assert_eq!(d.pane_owner(target), Some(dst));
        assert!(!d.window(src).unwrap().layout.contains_pane(moved));
        assert!(d.window(dst).unwrap().layout.contains_pane(moved));
        assert!(d.window(dst).unwrap().layout.contains_pane(target));
    }

    #[test]
    fn move_pane_atomic_failure_leaves_windows_identical() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let src = d.session(sid).unwrap().windows[0];
        let p0 = d.window(src).unwrap().layout.panes()[0];
        let moved = d
            .split_pane(src, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let (dst, target) = d.create_window(sid, "dst").unwrap();
        let src_before = d.window(src).unwrap().layout.clone();
        let dst_before = d.window(dst).unwrap().layout.clone();
        let owners_before = (d.pane_owner(moved), d.pane_owner(p0), d.pane_owner(target));

        let err = d
            .move_pane(
                src,
                dst,
                moved,
                target,
                Axis::Horizontal,
                0.5,
                Some((80, 24, 2, 1)),
                Some((3, 10, 2, 1)),
            )
            .unwrap_err();
        assert!(matches!(err, DomainError::Geometry(_)));
        assert_eq!(d.window(src).unwrap().layout, src_before);
        assert_eq!(d.window(dst).unwrap().layout, dst_before);
        assert_eq!(
            (d.pane_owner(moved), d.pane_owner(p0), d.pane_owner(target)),
            owners_before
        );
    }

    #[test]
    fn move_pane_same_window_rejected() {
        let mut d = Domain::bootstrap("s").unwrap();
        let src = d.sessions().next().unwrap().windows[0];
        let p0 = d.window(src).unwrap().layout.panes()[0];
        let p1 = d
            .split_pane(src, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let before = d.window(src).unwrap().layout.clone();
        let err = d
            .move_pane(src, src, p1, p0, Axis::Vertical, 0.5, None, None)
            .unwrap_err();
        assert!(matches!(err, DomainError::Geometry(_)));
        assert_eq!(d.window(src).unwrap().layout, before);
    }

    #[test]
    fn move_pane_last_leaf_source_collapses_window() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let src = d.session(sid).unwrap().windows[0];
        let moved = d.window(src).unwrap().layout.panes()[0];
        let (dst, target) = d.create_window(sid, "dst").unwrap();
        let client = d.mint_client().unwrap();
        d.acquire_controller(moved, client).unwrap();

        let focus = d
            .move_pane(
                src,
                dst,
                moved,
                target,
                Axis::Horizontal,
                0.5,
                Some((80, 24, 2, 1)),
                Some((80, 24, 2, 1)),
            )
            .unwrap();
        assert_eq!(focus, None);
        assert!(d.window(src).is_none());
        assert!(d.session(sid).is_some());
        assert_eq!(d.session(sid).unwrap().windows, vec![dst]);
        assert_eq!(d.pane_owner(moved), Some(dst));
        assert_eq!(d.controller(moved).unwrap(), Some(client));
        assert!(d.window(dst).unwrap().layout.contains_pane(moved));
        assert!(d.window(dst).unwrap().layout.contains_pane(target));
    }

    #[test]
    fn move_pane_last_leaf_of_last_window_destroys_source_session() {
        let mut d = Domain::bootstrap("src").unwrap();
        let src_sid = d.sessions().next().unwrap().id;
        let src = d.session(src_sid).unwrap().windows[0];
        let moved = d.window(src).unwrap().layout.panes()[0];
        let dst_sid = d.create_session("dst").unwrap();
        let (dst, target) = d.create_window(dst_sid, "dst").unwrap();

        d.move_pane(
            src,
            dst,
            moved,
            target,
            Axis::Horizontal,
            0.5,
            None,
            Some((80, 24, 2, 1)),
        )
        .unwrap();
        assert!(d.session(src_sid).is_none());
        assert!(d.window(src).is_none());
        assert!(d.session(dst_sid).is_some());
        assert_eq!(d.pane_owner(moved), Some(dst));
        assert!(d.pane(moved).is_some());
    }

    #[test]
    fn move_pane_into_last_leaf_window_ok() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let src = d.session(sid).unwrap().windows[0];
        let p0 = d.window(src).unwrap().layout.panes()[0];
        let moved = d
            .split_pane(src, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let (dst, target) = d.create_window(sid, "dst").unwrap();
        assert!(matches!(d.window(dst).unwrap().layout, PaneLayout::Leaf(_)));

        d.move_pane(
            src,
            dst,
            moved,
            target,
            Axis::Horizontal,
            0.5,
            Some((80, 24, 2, 1)),
            Some((80, 24, 2, 1)),
        )
        .unwrap();
        assert_eq!(d.window(dst).unwrap().layout.pane_count(), 2);
        assert_geometry_conserved(&d.window(src).unwrap().layout, 80, 24);
        assert_geometry_conserved(&d.window(dst).unwrap().layout, 80, 24);
    }

    #[test]
    fn rename_window_sets_title_and_rejects_empty_or_oversized() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let window = d.session(sid).unwrap().windows[0];
        d.rename_window(window, "  work  ").unwrap();
        assert_eq!(d.window(window).unwrap().title, "work");
        assert_eq!(d.rename_window(window, "   "), Err(DomainError::EmptyName));
        assert_eq!(
            d.rename_window(window, &"x".repeat(65)),
            Err(DomainError::InvalidName)
        );
        assert_eq!(
            d.rename_window(window, "no\0pe"),
            Err(DomainError::InvalidName)
        );
        let missing = WindowId::from_raw(99);
        assert_eq!(
            d.rename_window(missing, "x"),
            Err(DomainError::UnknownWindow(missing))
        );
        assert_eq!(d.window(window).unwrap().title, "work");
    }

    #[test]
    fn reorder_window_moves_index_and_keeps_ids() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let first = d.session(sid).unwrap().windows[0];
        let (second, _) = d.create_window(sid, "b").unwrap();
        let (third, _) = d.create_window(sid, "c").unwrap();
        assert_eq!(d.session(sid).unwrap().windows, vec![first, second, third]);
        assert!(d.reorder_window(sid, 0, 2).unwrap());
        assert_eq!(d.session(sid).unwrap().windows, vec![second, third, first]);
        assert!(!d.reorder_window(sid, 1, 1).unwrap());
        assert!(!d.reorder_window(sid, 9, 0).unwrap());
    }

    #[test]
    fn open_window_with_pane_extracts_leaf_and_collapses_empty_source() {
        let mut d = Domain::bootstrap("s").unwrap();
        let sid = d.sessions().next().unwrap().id;
        let src = d.session(sid).unwrap().windows[0];
        let p0 = d.window(src).unwrap().layout.panes()[0];
        let p1 = d
            .split_pane(src, p0, Axis::Horizontal, 0.5, Some((80, 24, 2, 1)))
            .unwrap();
        let new = d.open_window_with_pane(sid, "moved", p1).unwrap();
        assert_eq!(d.pane_owner(p1), Some(new));
        assert!(d.window(src).unwrap().layout.contains_pane(p0));
        assert!(!d.window(src).unwrap().layout.contains_pane(p1));
        assert!(matches!(d.window(new).unwrap().layout, PaneLayout::Leaf(id) if id == p1));
        assert_eq!(d.window(new).unwrap().title, "moved");

        let last = d.open_window_with_pane(sid, "last", p0).unwrap();
        assert!(d.window(src).is_none());
        assert_eq!(d.pane_owner(p0), Some(last));
        assert_eq!(d.session(sid).unwrap().windows, vec![new, last]);
    }
}
