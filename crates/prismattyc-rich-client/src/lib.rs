//! Public app-side client for Prismattyc rich wire 0.1/0.2 plus the bounded 0.3
//! reserved-workspace slice.
//!
//! This crate owns negotiation, raw-TTY lifecycle, local generation/revision
//! tracking, event validation, detach, workspace snapshots, and classic
//! fallback. It depends only on `prismattyc-protocol`.
//!
//! Apps import this crate and `prismattyc-protocol`. They must not import host,
//! mux, or compositor types.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::time::{Duration, Instant};

use prismattyc_protocol::{
    decode_body, encode_capability_query, encode_detach, ApcCollector, CapabilityQuery,
    CapabilityReply, CollectedApc, CollectionItem, CollectionPatch, CollectionPatchKind,
    CollectionRejectReason, CollectionSnapshot, ControlMessage, DecodeError, Feature, FocusStatus,
    InputEvent, InputEventKind, ProtocolVersion, RequestId, StatusItem, StatusSnapshot, TreeNode,
    ViewerId, WorkspaceRows, WorkspaceSnapshot,
};

#[cfg(unix)]
mod raw;
#[cfg(unix)]
pub use raw::{enter_raw_stdin, RawStdin};

/// PTY hosts need more than 250ms; piped CI still returns immediately.
pub const NEGOTIATE_TIMEOUT: Duration = Duration::from_millis(750);

/// Default correlation id used by the 0.2 reference query.
pub fn default_request_id() -> RequestId {
    RequestId::new(1).expect("1 is a valid request id")
}

/// Default query: request id 1, `max=0.2`.
pub fn default_query() -> CapabilityQuery {
    CapabilityQuery {
        request_id: default_request_id(),
        max_version: ProtocolVersion::new(0, 2),
    }
}

/// Encode the default 0.2 capability query. Bytes match `prismattyc-protocol`.
///
/// # Errors
///
/// Returns a protocol decode error if the encoder rejects the query.
pub fn encode_default_query() -> Result<Vec<u8>, DecodeError> {
    encode_capability_query(default_query())
}

/// Query for the bounded 0.3 reserved-workspace/tree slice. The 0.2 default
/// remains frozen for existing apps.
pub fn surface_query() -> CapabilityQuery {
    CapabilityQuery {
        request_id: default_request_id(),
        max_version: ProtocolVersion::new(0, 3),
    }
}

pub fn encode_surface_query() -> Result<Vec<u8>, DecodeError> {
    encode_capability_query(surface_query())
}

/// Why a session stayed classic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassicReason {
    /// No capability reply arrived before the wait ended.
    Timeout,
    /// Stdin closed before a reply (child/host exit during negotiate).
    ChildExit,
    /// A body arrived that was not a well-formed capability reply.
    Malformed,
    /// A well-formed reply that does not grant `hybrid.attach.cell_rect`.
    Unsupported,
}

/// Outcome of 0.1/0.2 negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    Classic { reason: ClassicReason },
    Rich(RichGrant),
}

impl Session {
    pub fn is_rich(&self) -> bool {
        matches!(self, Self::Rich(_))
    }
}

/// Granted 0.1/0.2 session. Generation is 1 for the live PTY grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichGrant {
    pub reply: CapabilityReply,
    pub generation: u64,
    pub scene_rev: u64,
    registered: BTreeSet<u32>,
    focus_ids: BTreeSet<u32>,
    collection_revs: BTreeMap<String, u64>,
    status_rev: u64,
    action_bindings: BTreeMap<u32, u32>,
    viewers: BTreeMap<ViewerId, ViewerInputState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ViewerInputState {
    focused: bool,
    last_seq: u64,
}

impl RichGrant {
    fn from_reply(reply: CapabilityReply) -> Self {
        Self {
            reply,
            generation: 1,
            scene_rev: 0,
            registered: BTreeSet::new(),
            focus_ids: BTreeSet::new(),
            collection_revs: BTreeMap::new(),
            status_rev: 0,
            action_bindings: BTreeMap::new(),
            viewers: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, id: u32) {
        self.registered.insert(id);
    }

    pub fn register_focus(&mut self, id: u32) {
        self.registered.insert(id);
        self.focus_ids.insert(id);
    }

    pub fn is_registered(&self, id: u32) -> bool {
        self.registered.contains(&id)
    }

    pub fn registered_ids(&self) -> Vec<u32> {
        self.registered.iter().copied().collect()
    }

    pub fn supports(&self, feature: Feature) -> bool {
        self.reply.supports(feature)
    }

    pub fn has_viewport(&self) -> bool {
        self.supports(Feature::HybridOverlayViewport)
    }

    pub fn has_rich_focus(&self) -> bool {
        self.supports(Feature::InputRichFocus)
    }

    /// Both grants are required: a tree without reserved rows must not cover
    /// transcript cells, and reserved rows without a bounded tree are inert.
    pub fn has_workspace(&self) -> bool {
        self.supports(Feature::HybridReserveRows) && self.supports(Feature::RichTreeV1)
    }

    pub fn has_collections(&self) -> bool {
        self.has_workspace() && self.supports(Feature::RichCollectionV1)
    }

    pub fn has_semantics(&self) -> bool {
        self.has_workspace() && self.supports(Feature::RichSemanticTextV1)
    }

    pub fn has_status(&self) -> bool {
        self.has_workspace() && self.supports(Feature::RichStatusV1)
    }

    pub fn has_structured_input(&self) -> bool {
        self.has_workspace()
            && self.has_rich_focus()
            && (self.supports(Feature::InputRichKeyboardV1)
                || self.supports(Feature::InputRichPointerV1)
                || self.supports(Feature::InputRichScrollV1))
    }

    pub fn collection_rev(&self, collection_id: &str) -> u64 {
        self.collection_revs
            .get(collection_id)
            .copied()
            .unwrap_or(0)
    }

    /// Advance the local scene revision before a snapshot or patch send.
    pub fn bump_scene_rev(&mut self) -> u64 {
        self.scene_rev = self.scene_rev.saturating_add(1);
        self.scene_rev
    }

    /// Mark a 0.2 re-attach as a resnapshot (same generation, new revision).
    pub fn resnapshot(&mut self) -> u64 {
        self.bump_scene_rev()
    }

    /// Build the next atomic workspace snapshot for this negotiated lifetime.
    pub fn workspace_snapshot(
        &mut self,
        rows: WorkspaceRows,
        nodes: Vec<TreeNode>,
    ) -> Option<WorkspaceSnapshot> {
        if !self.has_workspace() {
            return None;
        }
        self.action_bindings = nodes
            .iter()
            .filter(|node| node.action_id != 0)
            .map(|node| (node.id, node.action_id))
            .collect();
        Some(WorkspaceSnapshot {
            surface_generation: self.generation,
            scene_rev: self.bump_scene_rev(),
            rows,
            nodes,
        })
    }

    pub fn collection_snapshot(
        &mut self,
        collection_id: impl Into<String>,
        items: Vec<CollectionItem>,
    ) -> Option<CollectionSnapshot> {
        if !self.has_collections() {
            return None;
        }
        let collection_id = collection_id.into();
        let rev = self
            .collection_revs
            .get(&collection_id)
            .copied()
            .unwrap_or(0)
            + 1;
        self.collection_revs.insert(collection_id.clone(), rev);
        Some(CollectionSnapshot {
            surface_generation: self.generation,
            collection_id,
            rev,
            items,
        })
    }

    /// Build a complete status layer for the current workspace scene. Status
    /// revisions are independent so replaceable updates may coalesce without
    /// pretending the structural scene changed.
    pub fn status_snapshot(&mut self, items: Vec<StatusItem>) -> Option<StatusSnapshot> {
        if !self.has_status() || self.scene_rev == 0 {
            return None;
        }
        self.status_rev = self.status_rev.saturating_add(1).max(1);
        Some(StatusSnapshot {
            surface_generation: self.generation,
            scene_rev: self.scene_rev,
            rev: self.status_rev,
            items,
        })
    }

    pub fn collection_append(
        &mut self,
        collection_id: impl Into<String>,
        items: Vec<CollectionItem>,
    ) -> Option<CollectionPatch> {
        self.collection_patch(collection_id, CollectionPatchKind::Append, items)
    }

    pub fn collection_replace(
        &mut self,
        collection_id: impl Into<String>,
        items: Vec<CollectionItem>,
    ) -> Option<CollectionPatch> {
        self.collection_patch(collection_id, CollectionPatchKind::Replace, items)
    }

    fn collection_patch(
        &mut self,
        collection_id: impl Into<String>,
        kind: CollectionPatchKind,
        items: Vec<CollectionItem>,
    ) -> Option<CollectionPatch> {
        if !self.has_collections() || items.is_empty() {
            return None;
        }
        let collection_id = collection_id.into();
        let base = *self.collection_revs.get(&collection_id)?;
        let next = base.saturating_add(1);
        self.collection_revs.insert(collection_id.clone(), next);
        Some(CollectionPatch {
            surface_generation: self.generation,
            collection_id,
            base,
            next,
            kind,
            items,
        })
    }

    pub fn note_collection_ack(&mut self, collection_id: &str, rev: u64) {
        self.collection_revs
            .entry(collection_id.to_string())
            .and_modify(|current| *current = (*current).max(rev))
            .or_insert(rev);
    }

    pub fn reset_collection(&mut self, collection_id: &str) {
        self.collection_revs.remove(collection_id);
    }
}

/// Classify an optional decoded message after the wait.
pub fn session_from_message(message: Option<ControlMessage>) -> Session {
    session_from_message_for(default_request_id(), message)
}

/// Same as [`session_from_message`], but the reply must echo `expected`.
pub fn session_from_message_for(expected: RequestId, message: Option<ControlMessage>) -> Session {
    match message {
        Some(ControlMessage::CapabilityReply(reply)) if reply.request_id != expected => {
            Session::Classic {
                reason: ClassicReason::Malformed,
            }
        }
        Some(ControlMessage::CapabilityReply(reply))
            if reply.supports(Feature::HybridAttachCellRect) =>
        {
            Session::Rich(RichGrant::from_reply(reply))
        }
        Some(ControlMessage::CapabilityReply(_)) => Session::Classic {
            reason: ClassicReason::Unsupported,
        },
        Some(_) => Session::Classic {
            reason: ClassicReason::Malformed,
        },
        None => Session::Classic {
            reason: ClassicReason::Timeout,
        },
    }
}

/// Classify a raw APC body. Unknown or invalid bodies are classic/malformed.
pub fn session_from_body(body: Option<&str>) -> Session {
    session_from_body_for(default_request_id(), body)
}

/// Same as [`session_from_body`], but the reply must echo `expected`.
pub fn session_from_body_for(expected: RequestId, body: Option<&str>) -> Session {
    match body {
        None => Session::Classic {
            reason: ClassicReason::Timeout,
        },
        Some(raw) => match decode_body(raw) {
            Ok(message) => session_from_message_for(expected, Some(message)),
            Err(_) => Session::Classic {
                reason: ClassicReason::Malformed,
            },
        },
    }
}

/// Wait on stdin for the first capability reply. No leftover reader thread.
pub fn await_session(timeout: Duration) -> Session {
    await_session_from_stdin(timeout, default_query())
}

/// Negotiate the bounded reserved-workspace slice on stdin.
pub fn await_surface_session(timeout: Duration) -> Session {
    await_session_from_stdin(timeout, surface_query())
}

/// Read `reader` until a reply, EOF, or the deadline. Does not spawn a thread
/// and does not poll process stdin (safe for Cursor tests).
pub fn await_session_from<R: Read>(reader: R, timeout: Duration, expected: RequestId) -> Session {
    await_session_from_query(
        reader,
        timeout,
        CapabilityQuery {
            request_id: expected,
            max_version: ProtocolVersion::new(0, 2),
        },
    )
}

pub fn await_session_from_query<R: Read>(
    mut reader: R,
    timeout: Duration,
    query: CapabilityQuery,
) -> Session {
    collect_reply(&mut reader, timeout, query, false)
}

#[cfg(unix)]
fn await_session_from_stdin(timeout: Duration, query: CapabilityQuery) -> Session {
    struct StdinFd;
    impl Read for StdinFd {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }
    }
    collect_reply(&mut StdinFd, timeout, query, true)
}

#[cfg(not(unix))]
fn await_session_from_stdin(timeout: Duration, query: CapabilityQuery) -> Session {
    collect_reply(&mut io::stdin(), timeout, query, true)
}

fn collect_reply<R: Read>(
    reader: &mut R,
    timeout: Duration,
    query: CapabilityQuery,
    poll_stdin: bool,
) -> Session {
    let deadline = Instant::now() + timeout;
    let mut collector = ApcCollector::new();
    // Read exactly through the matched APC terminator. A larger read can
    // consume already-buffered keyboard input (or the start of a following
    // host event) after the reply and strand it outside the caller's loop.
    let mut buf = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Session::Classic {
                reason: ClassicReason::Timeout,
            };
        }
        if poll_stdin && !wait_stdin(remaining) {
            return Session::Classic {
                reason: ClassicReason::Timeout,
            };
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                return Session::Classic {
                    reason: ClassicReason::ChildExit,
                };
            }
            Ok(n) => {
                for event in collector.push(&buf[..n]) {
                    if let CollectedApc::Body(body) = event {
                        if let Some(session) = session_from_negotiating_body(query, &body) {
                            return session;
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return Session::Classic {
                    reason: ClassicReason::ChildExit,
                };
            }
        }
    }
}

/// Continue past unrelated or stale APC. Fail closed only on a malformed
/// capability reply that names this request id.
fn session_from_negotiating_body(query: CapabilityQuery, body: &str) -> Option<Session> {
    match decode_body(body) {
        Ok(ControlMessage::CapabilityReply(reply)) if reply.request_id == query.request_id => {
            if reply.version.major != query.max_version.major
                || reply.version.minor > query.max_version.minor
            {
                Some(Session::Classic {
                    reason: ClassicReason::Malformed,
                })
            } else {
                Some(session_from_message_for(
                    query.request_id,
                    Some(ControlMessage::CapabilityReply(reply)),
                ))
            }
        }
        Ok(ControlMessage::CapabilityReply(_)) | Ok(_) => None,
        Err(_) if body_names_capability_reply(query.request_id, body) => Some(Session::Classic {
            reason: ClassicReason::Malformed,
        }),
        Err(_) => None,
    }
}

fn body_names_capability_reply(expected: RequestId, body: &str) -> bool {
    let mut family = false;
    let mut id_match = false;
    for field in body.split(';') {
        if field == "cap" {
            family = true;
        }
        if field == format!("id={}", expected.get()) {
            id_match = true;
        }
    }
    family && id_match && prismattyc_protocol::body_has_prefix(body, "cap;r;")
}

fn wait_stdin(timeout: Duration) -> bool {
    #[cfg(unix)]
    {
        let mut pollfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let n = unsafe { libc::poll(&mut pollfd, 1, ms) };
        n > 0 && (pollfd.revents & libc::POLLIN != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        true
    }
}

/// A host event the 0.2 client will deliver to the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    FocusGranted {
        id: u32,
    },
    FocusRevoked {
        id: u32,
    },
    FocusKey {
        id: u32,
        key: String,
    },
    CollectionAck {
        collection_id: String,
        rev: u64,
    },
    CollectionReject {
        collection_id: String,
        reason: CollectionRejectReason,
    },
    CollectionResnapshot {
        collection_id: String,
    },
    StructuredInput(InputEvent),
    Ignored,
}

/// Drop events that do not belong to a live 0.2 grant.
pub fn validate_event(session: &Session, message: ControlMessage) -> Incoming {
    let Session::Rich(grant) = session else {
        return Incoming::Ignored;
    };
    if grant.generation == 0 {
        return Incoming::Ignored;
    }
    match message {
        ControlMessage::FocusReply {
            id,
            status: FocusStatus::Granted,
        } if grant.has_rich_focus() && grant.focus_ids.contains(&id) => {
            Incoming::FocusGranted { id }
        }
        ControlMessage::FocusReply { id, .. }
            if grant.has_rich_focus() && grant.focus_ids.contains(&id) =>
        {
            Incoming::FocusRevoked { id }
        }
        ControlMessage::FocusKey { id, key }
            if grant.has_rich_focus() && grant.focus_ids.contains(&id) =>
        {
            Incoming::FocusKey { id, key }
        }
        ControlMessage::CollectionAck {
            surface_generation,
            collection_id,
            rev,
        } if grant.has_collections() && surface_generation == grant.generation => {
            Incoming::CollectionAck { collection_id, rev }
        }
        ControlMessage::CollectionReject {
            surface_generation,
            collection_id,
            reason,
        } if grant.has_collections() && surface_generation == grant.generation => {
            Incoming::CollectionReject {
                collection_id,
                reason,
            }
        }
        ControlMessage::CollectionResnapshot {
            surface_generation,
            collection_id,
        } if grant.has_collections() && surface_generation == grant.generation => {
            Incoming::CollectionResnapshot { collection_id }
        }
        _ => Incoming::Ignored,
    }
}

fn structured_feature_granted(grant: &RichGrant, kind: &InputEventKind) -> bool {
    match kind {
        InputEventKind::Focus { .. } => grant.has_rich_focus(),
        InputEventKind::Key { .. } => grant.supports(Feature::InputRichKeyboardV1),
        InputEventKind::Pointer { .. } => grant.supports(Feature::InputRichPointerV1),
        InputEventKind::Scroll { .. } | InputEventKind::Viewport { .. } => {
            grant.supports(Feature::InputRichScrollV1)
        }
    }
}

fn validate_structured_event(grant: &mut RichGrant, event: InputEvent) -> Incoming {
    if !grant.has_structured_input()
        || event.surface_generation != grant.generation
        || event.scene_rev != grant.scene_rev
        || grant.action_bindings.get(&event.node_id) != Some(&event.action_id)
        || !structured_feature_granted(grant, &event.kind)
    {
        return Incoming::Ignored;
    }
    match (&event.collection_id, event.collection_rev) {
        (Some(collection_id), rev) if grant.collection_rev(collection_id) == rev => {}
        (None, 0) => {}
        _ => return Incoming::Ignored,
    }
    let prior = grant.viewers.get(&event.viewer_id).copied();
    match &event.kind {
        InputEventKind::Focus { focused: true } => {
            if prior.is_some_and(|state| event.request_seq <= state.last_seq) {
                return Incoming::Ignored;
            }
            grant.viewers.insert(
                event.viewer_id,
                ViewerInputState {
                    focused: true,
                    last_seq: event.request_seq,
                },
            );
        }
        InputEventKind::Focus { focused: false } => {
            let Some(state) = prior else {
                return Incoming::Ignored;
            };
            if !state.focused || event.request_seq <= state.last_seq {
                return Incoming::Ignored;
            }
            grant.viewers.insert(
                event.viewer_id,
                ViewerInputState {
                    focused: false,
                    last_seq: event.request_seq,
                },
            );
        }
        _ => {
            let Some(state) = prior else {
                return Incoming::Ignored;
            };
            if !state.focused || event.request_seq <= state.last_seq {
                return Incoming::Ignored;
            }
            grant.viewers.insert(
                event.viewer_id,
                ViewerInputState {
                    focused: true,
                    last_seq: event.request_seq,
                },
            );
        }
    }
    Incoming::StructuredInput(event)
}

/// Decode one APC body and validate it for the current session.
pub fn decode_event(session: &Session, body: &str) -> Incoming {
    match decode_body(body) {
        Ok(message) => validate_event(session, message),
        Err(_) => Incoming::Ignored,
    }
}

/// Decode and statefully validate one host event. Structured input needs this
/// mutable path so replayed sequences, unknown viewers, and revoked focus are
/// rejected before application mutation. Existing 0.1/0.2 callers may keep
/// using [`decode_event`].
pub fn decode_event_mut(session: &mut Session, body: &str) -> Incoming {
    let Ok(message) = decode_body(body) else {
        return Incoming::Ignored;
    };
    match message {
        ControlMessage::InputEvent(event) => {
            let Session::Rich(grant) = session else {
                return Incoming::Ignored;
            };
            validate_structured_event(grant, event)
        }
        other => validate_event(session, other),
    }
}

/// Encode detach frames for every registered region (0.2 teardown).
pub fn encode_detach_all(ids: &[u32]) -> Result<Vec<Vec<u8>>, DecodeError> {
    ids.iter().copied().map(encode_detach).collect()
}

type DetachWrite = Box<dyn FnMut(&[u8]) + Send>;

/// Writes detach frames for each registered id on drop unless disarmed.
pub struct DetachOnDrop {
    ids: Vec<u32>,
    write: Option<DetachWrite>,
    armed: bool,
}

impl DetachOnDrop {
    pub fn new(ids: impl Into<Vec<u32>>, write: impl FnMut(&[u8]) + Send + 'static) -> Self {
        Self {
            ids: ids.into(),
            write: Some(Box::new(write)),
            armed: true,
        }
    }

    /// Disarm without sending. Used when the caller already tore down.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let Ok(frames) = encode_detach_all(&self.ids) else {
            return;
        };
        if let Some(write) = self.write.as_mut() {
            for frame in frames {
                write(&frame);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_protocol::{
        encode_capability_query, encode_capability_reply, encode_detach, encode_focus_key,
        encode_focus_query, encode_focus_reply, encode_input_event, InputModifiers, TreeNodeKind,
    };

    fn v2_query() -> CapabilityQuery {
        default_query()
    }

    #[test]
    fn default_query_bytes_match_protocol() {
        let expected = encode_capability_query(v2_query()).unwrap();
        assert_eq!(encode_default_query().unwrap(), expected);
        assert_eq!(
            encode_surface_query().unwrap(),
            encode_capability_query(surface_query()).unwrap()
        );
    }

    #[test]
    fn timeout_and_missing_body_are_classic() {
        assert_eq!(
            session_from_message(None),
            Session::Classic {
                reason: ClassicReason::Timeout
            }
        );
        assert_eq!(
            session_from_body(None),
            Session::Classic {
                reason: ClassicReason::Timeout
            }
        );
    }

    #[test]
    fn malformed_reply_is_classic() {
        assert_eq!(
            session_from_body(Some("not-a-prism-body")),
            Session::Classic {
                reason: ClassicReason::Malformed
            }
        );
        assert_eq!(
            session_from_body(Some("Prismattyc;cap;r;id=1;v=nope")),
            Session::Classic {
                reason: ClassicReason::Malformed
            }
        );
        let attach = ControlMessage::FocusQuery { id: 3 };
        assert_eq!(
            session_from_message(Some(attach)),
            Session::Classic {
                reason: ClassicReason::Malformed
            }
        );
    }

    #[test]
    fn unsupported_reply_is_classic() {
        let reply = CapabilityReply {
            request_id: default_request_id(),
            version: ProtocolVersion::new(0, 2),
            features: Default::default(),
            limits: Default::default(),
        };
        assert_eq!(
            session_from_message(Some(ControlMessage::CapabilityReply(reply))),
            Session::Classic {
                reason: ClassicReason::Unsupported
            }
        );
    }

    #[test]
    fn v2_grant_is_rich_generation_one() {
        let reply = CapabilityReply::for_v1_query(v2_query()).unwrap();
        let Session::Rich(grant) =
            session_from_message(Some(ControlMessage::CapabilityReply(reply)))
        else {
            panic!("expected rich");
        };
        assert_eq!(grant.generation, 1);
        assert_eq!(grant.scene_rev, 0);
        assert!(grant.reply.supports(Feature::HybridAttachCellRect));
        let mut grant = grant;
        assert_eq!(grant.resnapshot(), 1);
        assert_eq!(grant.scene_rev, 1);
    }

    #[test]
    fn spike_reply_is_still_rich() {
        let query = CapabilityQuery {
            request_id: RequestId::new(2).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        };
        let reply = CapabilityReply::for_spike_query(query).unwrap();
        assert!(session_from_message_for(
            RequestId::new(2).unwrap(),
            Some(ControlMessage::CapabilityReply(reply))
        )
        .is_rich());
    }

    #[test]
    fn events_drop_in_classic_and_accept_in_rich() {
        let classic = Session::Classic {
            reason: ClassicReason::Timeout,
        };
        let grant = encode_focus_reply(3, FocusStatus::Granted).unwrap();
        let body = std::str::from_utf8(&grant[2..grant.len() - 2]).unwrap();
        assert_eq!(decode_event(&classic, body), Incoming::Ignored);

        let reply = CapabilityReply::for_v1_query(v2_query()).unwrap();
        let mut rich = session_from_message(Some(ControlMessage::CapabilityReply(reply)));
        if let Session::Rich(grant) = &mut rich {
            grant.register_focus(3);
        }
        assert_eq!(decode_event(&rich, body), Incoming::FocusGranted { id: 3 });
        let other = encode_focus_reply(9, FocusStatus::Granted).unwrap();
        let other_body = std::str::from_utf8(&other[2..other.len() - 2]).unwrap();
        assert_eq!(decode_event(&rich, other_body), Incoming::Ignored);
        let key = encode_focus_key(3, "a").unwrap();
        let key_body = std::str::from_utf8(&key[2..key.len() - 2]).unwrap();
        assert_eq!(
            decode_event(&rich, key_body),
            Incoming::FocusKey {
                id: 3,
                key: "a".into()
            }
        );
        assert_eq!(decode_event(&rich, "junk"), Incoming::Ignored);
        let query = encode_focus_query(3).unwrap();
        let qbody = std::str::from_utf8(&query[2..query.len() - 2]).unwrap();
        assert_eq!(decode_event(&rich, qbody), Incoming::Ignored);
    }

    #[test]
    fn detach_encodes_every_id_and_drop_is_safe() {
        let frames = encode_detach_all(&[1, 2, 3]).unwrap();
        assert_eq!(frames.len(), 3);
        for (idx, id) in [1_u32, 2, 3].into_iter().enumerate() {
            let expected = encode_detach(id).unwrap();
            assert_eq!(frames[idx], expected);
        }
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let captured = sent.clone();
        {
            let _guard = DetachOnDrop::new(vec![1, 2], move |frame| {
                captured.lock().expect("lock").push(frame.to_vec());
            });
        }
        let wrote = sent.lock().expect("lock");
        assert_eq!(wrote.len(), 2);
        assert_eq!(wrote[0], encode_detach(1).unwrap());
        assert_eq!(wrote[1], encode_detach(2).unwrap());
    }

    #[test]
    fn child_exit_reason_is_distinct_from_timeout() {
        assert_ne!(ClassicReason::ChildExit, ClassicReason::Timeout);
        assert_eq!(
            Session::Classic {
                reason: ClassicReason::ChildExit
            },
            Session::Classic {
                reason: ClassicReason::ChildExit
            }
        );
        assert_eq!(
            await_session_from(
                std::io::Cursor::new([]),
                NEGOTIATE_TIMEOUT,
                default_request_id()
            ),
            Session::Classic {
                reason: ClassicReason::ChildExit
            }
        );
    }

    #[test]
    fn reply_must_echo_query_id() {
        let reply = CapabilityReply::for_v1_query(CapabilityQuery {
            request_id: RequestId::new(9).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        assert_eq!(
            session_from_message_for(
                default_request_id(),
                Some(ControlMessage::CapabilityReply(reply))
            ),
            Session::Classic {
                reason: ClassicReason::Malformed
            }
        );
    }

    #[test]
    fn spike_grant_does_not_accept_focus_events() {
        let query = CapabilityQuery {
            request_id: default_request_id(),
            max_version: ProtocolVersion::new(0, 1),
        };
        let reply = CapabilityReply::for_spike_query(query).unwrap();
        let session = session_from_message(Some(ControlMessage::CapabilityReply(reply)));
        let Session::Rich(grant) = &session else {
            panic!("expected rich");
        };
        assert!(!grant.has_viewport());
        assert!(!grant.has_rich_focus());
        let grant = encode_focus_reply(3, FocusStatus::Granted).unwrap();
        let body = std::str::from_utf8(&grant[2..grant.len() - 2]).unwrap();
        assert_eq!(decode_event(&session, body), Incoming::Ignored);
    }

    #[test]
    fn negotiate_skips_stale_and_unrelated_apc() {
        let expected = default_query();
        let stale = encode_capability_reply(
            &CapabilityReply::for_v1_query(CapabilityQuery {
                request_id: RequestId::new(9).unwrap(),
                max_version: ProtocolVersion::new(0, 2),
            })
            .unwrap(),
        )
        .unwrap();
        let focus = encode_focus_query(3).unwrap();
        let grant =
            encode_capability_reply(&CapabilityReply::for_v1_query(v2_query()).unwrap()).unwrap();
        let mut wire = stale;
        wire.extend_from_slice(&focus);
        wire.extend_from_slice(&grant);
        let session =
            await_session_from_query(std::io::Cursor::new(wire), NEGOTIATE_TIMEOUT, expected);
        assert!(session.is_rich());
        assert_eq!(
            session_from_negotiating_body(expected, "Prismattyc;upd;id=2;text=x"),
            None
        );
        assert_eq!(
            session_from_negotiating_body(expected, "Prismattyc;cap;r;id=1;v=nope"),
            Some(Session::Classic {
                reason: ClassicReason::Malformed
            })
        );
    }

    #[test]
    fn collection_responses_require_current_generation_and_collection_grant() {
        let rich = session_from_message(Some(ControlMessage::CapabilityReply(
            CapabilityReply::for_surface_query(surface_query()).unwrap(),
        )));
        let legacy = session_from_message(Some(ControlMessage::CapabilityReply(
            CapabilityReply::for_v1_query(v2_query()).unwrap(),
        )));
        let classic = Session::Classic {
            reason: ClassicReason::Unsupported,
        };
        for generation in [0, 1, 2] {
            let responses = [
                (
                    ControlMessage::CollectionAck {
                        surface_generation: generation,
                        collection_id: "tasks".into(),
                        rev: 7,
                    },
                    Incoming::CollectionAck {
                        collection_id: "tasks".into(),
                        rev: 7,
                    },
                ),
                (
                    ControlMessage::CollectionReject {
                        surface_generation: generation,
                        collection_id: "tasks".into(),
                        reason: CollectionRejectReason::Stale,
                    },
                    Incoming::CollectionReject {
                        collection_id: "tasks".into(),
                        reason: CollectionRejectReason::Stale,
                    },
                ),
                (
                    ControlMessage::CollectionResnapshot {
                        surface_generation: generation,
                        collection_id: "tasks".into(),
                    },
                    Incoming::CollectionResnapshot {
                        collection_id: "tasks".into(),
                    },
                ),
            ];
            for (message, expected) in responses {
                assert_eq!(
                    validate_event(&rich, message.clone()),
                    if generation == 1 {
                        expected
                    } else {
                        Incoming::Ignored
                    }
                );
                assert_eq!(validate_event(&legacy, message.clone()), Incoming::Ignored);
                assert_eq!(validate_event(&classic, message.clone()), Incoming::Ignored);
                let mut detached = rich.clone();
                let Session::Rich(grant) = &mut detached else {
                    unreachable!()
                };
                grant.generation = 0;
                assert_eq!(validate_event(&detached, message), Incoming::Ignored);
            }
        }
    }

    #[test]
    fn surface_grant_builds_monotonic_workspace_snapshots() {
        let reply = CapabilityReply::for_surface_query(surface_query()).unwrap();
        let mut session = session_from_message(Some(ControlMessage::CapabilityReply(reply)));
        let Session::Rich(grant) = &mut session else {
            panic!("expected rich");
        };
        assert!(grant.has_workspace());
        let rows = WorkspaceRows {
            min: 5,
            preferred: 8,
            max: 12,
        };
        let nodes = vec![TreeNode {
            id: 1,
            parent: 0,
            kind: TreeNodeKind::Text,
            min: 1,
            preferred: 1,
            fill: 1,
            show_min_cols: 0,
            show_max_cols: 0,
            action_id: 0,
            text: "runbook".into(),
        }];
        let first = grant.workspace_snapshot(rows, nodes.clone()).unwrap();
        let second = grant.workspace_snapshot(rows, nodes).unwrap();
        assert_eq!(first.surface_generation, 1);
        assert_eq!(first.scene_rev, 1);
        assert_eq!(second.scene_rev, 2);
        assert!(grant.has_collections());
        let snapshot = grant
            .collection_snapshot(
                "tasks",
                vec![CollectionItem {
                    id: 1,
                    replaceable: true,
                    text: "check ready".into(),
                }],
            )
            .unwrap();
        let append = grant
            .collection_append(
                "tasks",
                vec![CollectionItem {
                    id: 2,
                    replaceable: false,
                    text: "ran".into(),
                }],
            )
            .unwrap();
        assert_eq!(snapshot.rev, 1);
        assert_eq!(append.base, 1);
        assert_eq!(append.next, 2);
        assert!(grant.has_status());
        let status = grant
            .status_snapshot(vec![StatusItem {
                node_id: 1,
                tone: prismattyc_protocol::StatusTone::Success,
                visual: prismattyc_protocol::StatusVisual::Badge,
            }])
            .unwrap();
        assert_eq!(status.scene_rev, 2);
        assert_eq!(status.rev, 1);
        assert_eq!(grant.status_snapshot(Vec::new()).unwrap().rev, 2);
    }

    #[test]
    fn structured_input_requires_focus_current_scene_binding_and_sequence() {
        let reply = CapabilityReply::for_surface_query(surface_query()).unwrap();
        let mut session = session_from_message(Some(ControlMessage::CapabilityReply(reply)));
        let Session::Rich(grant) = &mut session else {
            panic!("expected rich");
        };
        let rows = WorkspaceRows {
            min: 5,
            preferred: 5,
            max: 5,
        };
        grant
            .workspace_snapshot(
                rows,
                vec![TreeNode {
                    id: 4,
                    parent: 0,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 8,
                    text: "tasks".into(),
                }],
            )
            .unwrap();
        let viewer = ViewerId::from_bytes([0x22; 16]);
        let other = ViewerId::from_bytes([0x33; 16]);
        let body = |event: &InputEvent| {
            let encoded = encode_input_event(event).unwrap();
            std::str::from_utf8(&encoded[2..encoded.len() - 2])
                .unwrap()
                .to_string()
        };
        let focus = InputEvent {
            viewer_id: viewer,
            request_seq: 1,
            surface_generation: 1,
            scene_rev: 1,
            node_id: 4,
            action_id: 8,
            collection_id: None,
            collection_rev: 0,
            kind: InputEventKind::Focus { focused: true },
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&focus)),
            Incoming::StructuredInput(focus.clone())
        );

        let key = InputEvent {
            viewer_id: viewer,
            request_seq: 2,
            kind: InputEventKind::Key {
                key: "Enter".into(),
                modifiers: InputModifiers::default(),
            },
            ..focus.clone()
        };
        let wrong_viewer = InputEvent {
            viewer_id: other,
            ..key.clone()
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&wrong_viewer)),
            Incoming::Ignored
        );
        assert_eq!(
            decode_event_mut(&mut session, &body(&key)),
            Incoming::StructuredInput(key.clone())
        );
        assert_eq!(
            decode_event_mut(&mut session, &body(&key)),
            Incoming::Ignored
        );

        let stale_scene = InputEvent {
            request_seq: 3,
            scene_rev: 2,
            ..key.clone()
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&stale_scene)),
            Incoming::Ignored
        );
        let wrong_action = InputEvent {
            request_seq: 3,
            action_id: 9,
            ..key.clone()
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&wrong_action)),
            Incoming::Ignored
        );

        let revoke = InputEvent {
            request_seq: 3,
            kind: InputEventKind::Focus { focused: false },
            ..focus
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&revoke)),
            Incoming::StructuredInput(revoke)
        );
        let after_revoke = InputEvent {
            request_seq: 4,
            ..key
        };
        assert_eq!(
            decode_event_mut(&mut session, &body(&after_revoke)),
            Incoming::Ignored
        );
    }

    #[test]
    fn negotiation_rejects_reply_newer_than_requested_max() {
        let reply = CapabilityReply::for_surface_query(surface_query()).unwrap();
        let wire = encode_capability_reply(&reply).unwrap();
        assert_eq!(
            await_session_from_query(
                std::io::Cursor::new(wire),
                NEGOTIATE_TIMEOUT,
                default_query()
            ),
            Session::Classic {
                reason: ClassicReason::Malformed
            }
        );
    }

    #[test]
    fn negotiation_stops_at_reply_terminator_without_eating_input() {
        let reply =
            encode_capability_reply(&CapabilityReply::for_surface_query(surface_query()).unwrap())
                .unwrap();
        let mut wire = reply.clone();
        wire.extend_from_slice(b"q");
        let mut cursor = std::io::Cursor::new(wire);
        assert!(
            await_session_from_query(&mut cursor, NEGOTIATE_TIMEOUT, surface_query()).is_rich()
        );
        assert_eq!(cursor.position(), reply.len() as u64);
        let mut trailing = [0u8; 1];
        cursor.read_exact(&mut trailing).unwrap();
        assert_eq!(trailing, *b"q");
    }
}
