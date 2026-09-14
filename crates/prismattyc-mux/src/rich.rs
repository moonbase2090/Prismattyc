//! Server-side experimental rich sidecar.
//!
//! Flag-gated APC session on each live pane. Grant-after-flush. Overlays
//! land on [`PaneStyled::overlays`] for attach to paint. This
//! module does not paint. Markup/animation/canvas stay unadvertised.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use prismattyc_emulator::Emulator;
use prismattyc_protocol::{
    apply_collection_patch, apply_collection_snapshot, decode_body, encode_capability_reply,
    encode_collection_ack, encode_collection_reject, encode_collection_resnapshot,
    encode_focus_reply, encode_input_event, CapabilityReply, CollectedApc, CollectionPatch,
    CollectionRejectReason, CollectionSnapshot, ControlMessage, Feature, FocusStatus, InputEvent,
    InputEventKind, InputModifiers, PointerPhase, RegionKind, StatusSnapshot, StyledRun,
    TreeNodeKind, ViewerId, WorkspaceSnapshot, DEFAULT_LIMIT_COLLECTIONS, DEFAULT_LIMIT_EVENT_RATE,
    DEFAULT_LIMIT_REGIONS, LIMIT_REGIONS, MAX_CELL_RECT_ATTACHMENTS,
};
use prismattyc_render::{layout_workspace_with_status, WorkspaceLayout};

use crate::control::{OverlayKind, OverlayRun, PaneOverlay};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityGrant {
    features: BTreeSet<Feature>,
    region_limit: usize,
}

#[derive(Debug, Default)]
pub(crate) struct RichSession {
    granted: BTreeSet<Feature>,
    region_limit: usize,
    attachments: BTreeMap<u32, Attachment>,
    last_scrolled_lines: u64,
    /// Region ids the client asked to make focus-eligible. Never auto-granted.
    focus_requested: BTreeSet<u32>,
    focus_id: Option<u32>,
    focus_viewer: Option<ViewerId>,
    structured_focus: Option<StructuredFocus>,
    pointer_gestures: BTreeMap<ViewerId, PointerGesture>,
    workspace: Option<WorkspaceSnapshot>,
    status: Option<StatusSnapshot>,
    status_rev: u64,
    collections: BTreeMap<String, CollectionSnapshot>,
    semantics: BTreeMap<String, prismattyc_protocol::SemanticDocument>,
    pending_copies: VecDeque<PendingCopy>,
    copy_requests: VecDeque<u64>,
    next_copy_seq: u64,
}

/// Bounded FIFO of unmatched y/Y requests and of unacked copy payloads.
pub(crate) const MAX_PENDING_COPIES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCopy {
    seq: u64,
    owner: u64,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StructuredFocus {
    viewer_id: ViewerId,
    node_id: u32,
    action_id: u32,
    last_seq: u64,
    rate_started: Instant,
    rate_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkspaceHit {
    pub node_id: u32,
    pub action_id: u32,
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PointerGesture {
    hit: WorkspaceHit,
    start_row: u16,
    start_col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachKind {
    CellRect,
    Viewport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attachment {
    kind: AttachKind,
    row: i32,
    col: u16,
    rows: u16,
    cols: u16,
    text: String,
    runs: Vec<StyledRun>,
}

impl RichSession {
    fn apply_grant(&mut self, grant: CapabilityGrant) {
        self.granted = grant.features;
        self.region_limit = grant.region_limit;
        if !self.granted.contains(&Feature::InputRichFocus) {
            self.focus_requested.clear();
            self.focus_id = None;
            self.focus_viewer = None;
            self.structured_focus = None;
            self.pointer_gestures.clear();
        }
        if !self.granted.contains(&Feature::HybridReserveRows)
            || !self.granted.contains(&Feature::RichTreeV1)
        {
            self.workspace = None;
            self.status = None;
            self.status_rev = 0;
            self.structured_focus = None;
            self.pointer_gestures.clear();
        }
        if !self.granted.contains(&Feature::InputRichKeyboardV1)
            && !self.granted.contains(&Feature::InputRichPointerV1)
            && !self.granted.contains(&Feature::InputRichScrollV1)
        {
            self.structured_focus = None;
            self.pointer_gestures.clear();
        }
        if !self.granted.contains(&Feature::RichCollectionV1) {
            self.collections.clear();
        }
        if !self.granted.contains(&Feature::RichStatusV1) {
            self.status = None;
            self.status_rev = 0;
        }
    }

    fn can_insert(&self, id: u32) -> bool {
        self.attachments.contains_key(&id) || self.attachments.len() < self.region_limit
    }

    #[cfg(test)]
    pub(crate) fn focus_id(&self) -> Option<u32> {
        self.focus_id
    }

    fn note_focus_request(&mut self, id: u32) {
        if !self.granted.contains(&Feature::InputRichFocus) {
            return;
        }
        if self.attachments.contains_key(&id) {
            self.focus_requested.insert(id);
        }
    }

    /// Toggle grant for the lowest requested attached region.
    pub(crate) fn toggle_focus(&mut self, viewer_id: ViewerId) -> Option<Vec<u8>> {
        if !self.granted.contains(&Feature::InputRichFocus) {
            return None;
        }
        if let Some(id) = self.focus_id.take() {
            if self.focus_viewer != Some(viewer_id) {
                self.focus_id = Some(id);
                return None;
            }
            self.focus_viewer = None;
            return encode_focus_reply(id, FocusStatus::Revoked).ok();
        }
        let id = self
            .focus_requested
            .iter()
            .copied()
            .find(|id| self.attachments.contains_key(id))?;
        self.focus_id = Some(id);
        self.focus_viewer = Some(viewer_id);
        encode_focus_reply(id, FocusStatus::Granted).ok()
    }

    pub(crate) fn revoke_focus(&mut self, viewer_id: ViewerId) -> Option<Vec<u8>> {
        if self.focus_viewer != Some(viewer_id) {
            return None;
        }
        let id = self.focus_id.take()?;
        self.focus_viewer = None;
        encode_focus_reply(id, FocusStatus::Revoked).ok()
    }

    pub(crate) fn focus_id_for(&self, viewer_id: ViewerId) -> Option<u32> {
        (self.focus_viewer == Some(viewer_id))
            .then_some(self.focus_id)
            .flatten()
    }

    pub(crate) fn has_any_structured_focus(&self) -> bool {
        self.structured_focus.is_some()
    }

    fn has_structured_input(&self) -> bool {
        self.granted.contains(&Feature::InputRichFocus)
            && self.granted.contains(&Feature::HybridReserveRows)
            && self.granted.contains(&Feature::RichTreeV1)
            && (self.granted.contains(&Feature::InputRichKeyboardV1)
                || self.granted.contains(&Feature::InputRichPointerV1)
                || self.granted.contains(&Feature::InputRichScrollV1))
            && self.workspace.is_some()
    }

    fn first_workspace_action(&self) -> Option<(u32, u32)> {
        self.workspace
            .as_ref()?
            .nodes
            .iter()
            .find_map(|node| (node.action_id != 0).then_some((node.id, node.action_id)))
    }

    pub(crate) fn workspace_hit(
        &self,
        pane_rows: u16,
        pane_cols: u16,
        row: u16,
        col: u16,
    ) -> Option<WorkspaceHit> {
        let workspace = self.workspace.as_ref()?;
        let layout = self.workspace_layout(pane_rows, pane_cols).ok().flatten()?;
        workspace
            .nodes
            .iter()
            .filter(|node| node.action_id != 0)
            .filter_map(|node| {
                let placement = layout.placements.get(&node.id)?;
                let inside = row >= placement.row
                    && col >= placement.col
                    && row < placement.row.saturating_add(placement.rows)
                    && col < placement.col.saturating_add(placement.cols);
                inside.then(|| {
                    (
                        u32::from(placement.rows) * u32::from(placement.cols),
                        WorkspaceHit {
                            node_id: node.id,
                            action_id: node.action_id,
                            row: row - placement.row,
                            col: col - placement.col,
                        },
                    )
                })
            })
            .min_by_key(|(area, _)| *area)
            .map(|(_, hit)| hit)
    }

    fn structured_kind_granted(&self, kind: &InputEventKind) -> bool {
        match kind {
            InputEventKind::Focus { .. } => self.has_structured_input(),
            InputEventKind::Key { .. } => self.granted.contains(&Feature::InputRichKeyboardV1),
            InputEventKind::Pointer { .. } => self.granted.contains(&Feature::InputRichPointerV1),
            InputEventKind::Scroll { .. } | InputEventKind::Viewport { .. } => {
                self.granted.contains(&Feature::InputRichScrollV1)
            }
        }
    }

    fn encode_structured(
        &mut self,
        viewer_id: ViewerId,
        hit: WorkspaceHit,
        kind: InputEventKind,
    ) -> Option<Vec<u8>> {
        if !self.structured_kind_granted(&kind) {
            return None;
        }
        let workspace = self.workspace.as_ref()?;
        if workspace
            .nodes
            .iter()
            .find(|node| node.id == hit.node_id)
            .map(|node| node.action_id)
            != Some(hit.action_id)
        {
            return None;
        }
        let focus = self.structured_focus.as_mut()?;
        if focus.viewer_id != viewer_id {
            return None;
        }
        let rate_limited = !matches!(kind, InputEventKind::Focus { .. });
        if rate_limited {
            let now = Instant::now();
            if now.duration_since(focus.rate_started) >= Duration::from_secs(1) {
                focus.rate_started = now;
                focus.rate_count = 0;
            }
            if focus.rate_count >= DEFAULT_LIMIT_EVENT_RATE {
                return None;
            }
        }
        let request_seq = focus.last_seq.checked_add(1)?;
        let event = InputEvent {
            viewer_id,
            request_seq,
            surface_generation: workspace.surface_generation,
            scene_rev: workspace.scene_rev,
            node_id: hit.node_id,
            action_id: hit.action_id,
            collection_id: None,
            collection_rev: 0,
            kind,
        };
        let encoded = encode_input_event(&event).ok()?;
        focus.last_seq = request_seq;
        if rate_limited {
            focus.rate_count = focus.rate_count.saturating_add(1);
        }
        focus.node_id = hit.node_id;
        focus.action_id = hit.action_id;
        Some(encoded)
    }

    pub(crate) fn toggle_structured_focus(&mut self, viewer_id: ViewerId) -> Option<Vec<u8>> {
        if !self.has_structured_input() {
            return None;
        }
        if let Some(focus) = self.structured_focus {
            if focus.viewer_id != viewer_id {
                return None;
            }
            let encoded = self.encode_structured(
                viewer_id,
                WorkspaceHit {
                    node_id: focus.node_id,
                    action_id: focus.action_id,
                    row: 0,
                    col: 0,
                },
                InputEventKind::Focus { focused: false },
            );
            self.structured_focus = None;
            self.pointer_gestures.remove(&viewer_id);
            return encoded;
        }
        let (node_id, action_id) = self.first_workspace_action()?;
        self.structured_focus = Some(StructuredFocus {
            viewer_id,
            node_id,
            action_id,
            last_seq: 0,
            rate_started: Instant::now(),
            rate_count: 0,
        });
        self.encode_structured(
            viewer_id,
            WorkspaceHit {
                node_id,
                action_id,
                row: 0,
                col: 0,
            },
            InputEventKind::Focus { focused: true },
        )
    }

    pub(crate) fn revoke_structured_focus(&mut self, viewer_id: ViewerId) -> Option<Vec<u8>> {
        let focus = self.structured_focus?;
        if focus.viewer_id != viewer_id {
            return None;
        }
        let encoded = self.encode_structured(
            viewer_id,
            WorkspaceHit {
                node_id: focus.node_id,
                action_id: focus.action_id,
                row: 0,
                col: 0,
            },
            InputEventKind::Focus { focused: false },
        );
        self.structured_focus = None;
        self.pointer_gestures.remove(&viewer_id);
        encoded
    }

    pub(crate) fn structured_focus_node(&self, viewer_id: ViewerId) -> Option<u32> {
        self.structured_focus
            .filter(|focus| focus.viewer_id == viewer_id)
            .map(|focus| focus.node_id)
    }

    pub(crate) fn encode_structured_key(
        &mut self,
        viewer_id: ViewerId,
        key: String,
        modifiers: InputModifiers,
    ) -> Option<Vec<u8>> {
        let focus = self.structured_focus?;
        self.encode_structured(
            viewer_id,
            WorkspaceHit {
                node_id: focus.node_id,
                action_id: focus.action_id,
                row: 0,
                col: 0,
            },
            InputEventKind::Key { key, modifiers },
        )
    }

    pub(crate) fn handle_structured_pointer(
        &mut self,
        viewer_id: ViewerId,
        pane_rows: u16,
        pane_cols: u16,
        phase: PointerPhase,
        row: u16,
        col: u16,
    ) -> Option<Vec<u8>> {
        self.structured_focus_node(viewer_id)?;
        match phase {
            PointerPhase::Press => {
                let hit = self.workspace_hit(pane_rows, pane_cols, row, col)?;
                self.pointer_gestures.insert(
                    viewer_id,
                    PointerGesture {
                        hit,
                        start_row: row,
                        start_col: col,
                    },
                );
                self.encode_structured(
                    viewer_id,
                    hit,
                    InputEventKind::Pointer {
                        phase,
                        row: hit.row,
                        col: hit.col,
                    },
                )
            }
            PointerPhase::Move => {
                let gesture = self.pointer_gestures.get(&viewer_id).copied()?;
                if gesture.start_row != row || gesture.start_col != col {
                    self.pointer_gestures.remove(&viewer_id);
                }
                None
            }
            PointerPhase::Release => {
                let gesture = self.pointer_gestures.remove(&viewer_id)?;
                let hit = self.workspace_hit(pane_rows, pane_cols, row, col)?;
                if hit.node_id != gesture.hit.node_id || hit.action_id != gesture.hit.action_id {
                    return None;
                }
                self.encode_structured(
                    viewer_id,
                    hit,
                    InputEventKind::Pointer {
                        phase: PointerPhase::Activate,
                        row: hit.row,
                        col: hit.col,
                    },
                )
            }
            PointerPhase::Activate => None,
        }
    }

    pub(crate) fn encode_structured_scroll(
        &mut self,
        viewer_id: ViewerId,
        pane_rows: u16,
        pane_cols: u16,
        row: u16,
        col: u16,
        delta: i16,
    ) -> Option<Vec<u8>> {
        let hit = self.workspace_hit(pane_rows, pane_cols, row, col)?;
        self.encode_structured(viewer_id, hit, InputEventKind::Scroll { delta })
    }

    pub(crate) fn workspace_layout(
        &self,
        pane_rows: u16,
        pane_cols: u16,
    ) -> Result<Option<WorkspaceLayout>, prismattyc_render::WorkspaceLayoutError> {
        self.workspace
            .as_ref()
            .map(|snapshot| {
                layout_workspace_with_status(snapshot, self.status.as_ref(), pane_rows, pane_cols)
            })
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn drop_workspace(&mut self) {
        self.workspace = None;
        self.status = None;
        self.status_rev = 0;
        self.collections.clear();
        self.semantics.clear();
        self.structured_focus = None;
        self.pointer_gestures.clear();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_semantic_copy(&mut self) -> Option<String> {
        self.pending_copies.pop_front().map(|copy| copy.text)
    }

    pub(crate) fn peek_copy_for(&self, client_id: u64) -> Option<(u64, String)> {
        self.pending_copies
            .iter()
            .find(|copy| copy.owner == client_id)
            .map(|copy| (copy.seq, copy.text.clone()))
    }

    pub(crate) fn ack_copy_for(&mut self, client_id: u64, seq: u64) -> Option<String> {
        let index = self
            .pending_copies
            .iter()
            .position(|copy| copy.owner == client_id && copy.seq == seq)?;
        self.pending_copies.remove(index).map(|copy| copy.text)
    }

    pub(crate) fn queue_copy_request(&mut self, owner: u64) {
        if self.copy_requests.len() >= MAX_PENDING_COPIES {
            self.copy_requests.pop_front();
        }
        self.copy_requests.push_back(owner);
    }

    fn push_copy(&mut self, owner: u64, text: String) {
        let seq = self.next_copy_seq.max(1);
        self.next_copy_seq = seq.saturating_add(1);
        if self.pending_copies.len() >= MAX_PENDING_COPIES {
            self.pending_copies.pop_front();
        }
        self.pending_copies
            .push_back(PendingCopy { seq, owner, text });
    }

    pub(crate) fn semantic_copy_text(&self, document_id: &str) -> Option<String> {
        let doc = self.semantics.get(document_id)?;
        if let Some(range) = doc.selection {
            return doc.project(range).ok().filter(|text| !text.is_empty());
        }
        if let Some(span) = doc
            .spans
            .iter()
            .rev()
            .find(|span| span.role == prismattyc_protocol::SemanticRole::Location)
        {
            return doc.span_text(*span).ok();
        }
        Some(doc.text.clone()).filter(|text| !text.is_empty())
    }

    fn collection_reply(
        &mut self,
        generation: u64,
        collection_id: &str,
        result: Result<Option<CollectionSnapshot>, CollectionRejectReason>,
    ) -> Vec<Vec<u8>> {
        match result {
            Ok(Some(snapshot)) => {
                let rev = snapshot.rev;
                self.collections
                    .insert(snapshot.collection_id.clone(), snapshot);
                encode_collection_ack(generation, collection_id, rev)
                    .ok()
                    .into_iter()
                    .collect()
            }
            Ok(None) => encode_collection_ack(
                generation,
                collection_id,
                self.collections
                    .get(collection_id)
                    .map(|snapshot| snapshot.rev)
                    .unwrap_or(1),
            )
            .ok()
            .into_iter()
            .collect(),
            Err(reason) => {
                self.collections.remove(collection_id);
                let mut out = Vec::new();
                if let Ok(bytes) = encode_collection_reject(generation, collection_id, reason) {
                    out.push(bytes);
                }
                if let Ok(bytes) = encode_collection_resnapshot(generation, collection_id) {
                    out.push(bytes);
                }
                out
            }
        }
    }

    fn accept_workspace(&mut self, snapshot: WorkspaceSnapshot) {
        if !self.granted.contains(&Feature::HybridReserveRows)
            || !self.granted.contains(&Feature::RichTreeV1)
        {
            return;
        }
        match &self.workspace {
            Some(current) if current.surface_generation != snapshot.surface_generation => {
                self.workspace = None;
                self.status = None;
                self.status_rev = 0;
            }
            Some(current) if snapshot.scene_rev <= current.scene_rev => {}
            _ => {
                self.status = None;
                self.status_rev = 0;
                self.workspace = Some(snapshot);
                if let Some(focus) = self.structured_focus {
                    let binding = self.workspace.as_ref().and_then(|workspace| {
                        workspace
                            .nodes
                            .iter()
                            .find(|node| node.id == focus.node_id)
                            .map(|node| node.action_id)
                    });
                    if binding != Some(focus.action_id) {
                        self.structured_focus = None;
                        self.pointer_gestures.clear();
                    }
                }
            }
        }
    }

    fn accept_status(&mut self, snapshot: StatusSnapshot) {
        if !self.granted.contains(&Feature::RichStatusV1)
            || prismattyc_protocol::validate_status_snapshot(&snapshot).is_err()
        {
            return;
        }
        let Some(workspace) = self.workspace.as_ref() else {
            return;
        };
        if snapshot.surface_generation != workspace.surface_generation
            || snapshot.scene_rev != workspace.scene_rev
            || snapshot.rev <= self.status_rev
        {
            return;
        }
        self.status_rev = snapshot.rev;
        let valid_nodes = snapshot.items.iter().all(|item| {
            workspace
                .nodes
                .iter()
                .any(|node| node.id == item.node_id && node.kind == TreeNodeKind::Text)
        });
        self.status = valid_nodes.then_some(snapshot);
    }

    fn drop_region(&mut self, id: u32) -> Option<Vec<u8>> {
        self.attachments.remove(&id);
        self.focus_requested.remove(&id);
        if self.focus_id == Some(id) {
            self.focus_id = None;
            self.focus_viewer = None;
            return encode_focus_reply(id, FocusStatus::Revoked).ok();
        }
        None
    }
}

fn send_bytes(
    to_child: &mpsc::SyncSender<Vec<u8>>,
    rich: &mut RichSession,
    bytes: Vec<u8>,
    grant: Option<CapabilityGrant>,
) {
    if to_child.try_send(bytes).is_ok() {
        if let Some(grant) = grant {
            rich.apply_grant(grant);
        }
    }
}

/// Feed child output, preserving scroll-vs-APC order.
pub(crate) fn process_rich_chunk(
    emulator: &mut Emulator,
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<Vec<u8>>,
    bytes: &[u8],
) {
    let mut index = 0;
    while index < bytes.len() {
        let end = if emulator.apc_pending() || bytes[index] == 0x1b {
            index + 1
        } else {
            bytes[index..]
                .iter()
                .position(|&b| b == 0x1b)
                .map_or(bytes.len(), |rel| index + rel)
        };
        feed_rich_slice(emulator, rich, to_child, &bytes[index..end]);
        index = end;
    }
}

fn feed_rich_slice(
    emulator: &mut Emulator,
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<Vec<u8>>,
    slice: &[u8],
) {
    if slice.is_empty() {
        return;
    }
    let before = emulator.screen().scrolled_lines();
    let events = emulator.feed(slice);
    for reply in emulator.take_pending_replies() {
        send_bytes(to_child, rich, reply, None);
    }
    let after = emulator.screen().scrolled_lines();
    if after > before {
        translate_attachments_for_scroll(rich, after);
    }
    if !events.is_empty() {
        handle_control_events(&events, rich, to_child);
    }
}

fn handle_control_events(
    events: &[CollectedApc],
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<Vec<u8>>,
) {
    for event in events {
        match event {
            CollectedApc::Discarded => {}
            CollectedApc::Body(body) => {
                match decode_body(body) {
                    Ok(ControlMessage::CapabilityQuery(query)) => {
                        if let Some(reply) = CapabilityReply::for_surface_query(query) {
                            if let Ok(bytes) = encode_capability_reply(&reply) {
                                let region_limit = if reply.limits.is_empty() {
                                    MAX_CELL_RECT_ATTACHMENTS
                                } else {
                                    reply
                                        .limits
                                        .get(LIMIT_REGIONS)
                                        .copied()
                                        .unwrap_or(DEFAULT_LIMIT_REGIONS)
                                        as usize
                                };
                                let grant = CapabilityGrant {
                                    features: reply.features.clone(),
                                    region_limit,
                                };
                                send_bytes(to_child, rich, bytes, Some(grant));
                            }
                        }
                    }
                    Ok(ControlMessage::AttachCellRect(attach)) => {
                        insert_attachment(
                            rich,
                            Feature::HybridAttachCellRect,
                            Attachment {
                                kind: AttachKind::CellRect,
                                row: i32::from(attach.row),
                                col: attach.col,
                                rows: attach.rows,
                                cols: attach.cols,
                                text: attach.text,
                                runs: Vec::new(),
                            },
                            attach.id,
                        );
                    }
                    Ok(ControlMessage::AttachViewport(attach)) => {
                        insert_attachment(
                            rich,
                            Feature::HybridOverlayViewport,
                            Attachment {
                                kind: AttachKind::Viewport,
                                row: i32::from(attach.row),
                                col: attach.col,
                                rows: attach.rows,
                                cols: attach.cols,
                                text: attach.text,
                                runs: attach.runs,
                            },
                            attach.id,
                        );
                    }
                    Ok(ControlMessage::AttachStyled(attach)) => {
                        let (kind, feature) = match attach.kind {
                            RegionKind::CellRect => {
                                (AttachKind::CellRect, Feature::HybridAttachCellRect)
                            }
                            RegionKind::Viewport => {
                                (AttachKind::Viewport, Feature::HybridOverlayViewport)
                            }
                        };
                        insert_attachment(
                            rich,
                            feature,
                            Attachment {
                                kind,
                                row: i32::from(attach.row),
                                col: attach.col,
                                rows: attach.rows,
                                cols: attach.cols,
                                text: String::new(),
                                runs: attach.runs,
                            },
                            attach.id,
                        );
                    }
                    Ok(ControlMessage::Update(update)) => {
                        let Some(existing) = rich.attachments.get_mut(&update.id) else {
                            continue;
                        };
                        let feature = match existing.kind {
                            AttachKind::CellRect => Feature::HybridAttachCellRect,
                            AttachKind::Viewport => Feature::HybridOverlayViewport,
                        };
                        if !rich.granted.contains(&feature) {
                            continue;
                        }
                        existing.text = update.text;
                        existing.runs = update.runs;
                    }
                    Ok(ControlMessage::Detach { id }) => {
                        if let Some(bytes) = rich.drop_region(id) {
                            send_bytes(to_child, rich, bytes, None);
                        }
                    }
                    Ok(ControlMessage::FocusQuery { id }) => {
                        rich.note_focus_request(id);
                    }
                    Ok(ControlMessage::WorkspaceSnapshot(snapshot)) => {
                        rich.accept_workspace(snapshot);
                    }
                    Ok(ControlMessage::WorkspaceDrop { surface_generation }) => {
                        if rich.workspace.as_ref().is_some_and(|snapshot| {
                            snapshot.surface_generation == surface_generation
                        }) {
                            rich.drop_workspace();
                        }
                    }
                    Ok(ControlMessage::StatusSnapshot(snapshot)) => {
                        rich.accept_status(snapshot);
                    }
                    Ok(ControlMessage::StatusDrop { surface_generation }) => {
                        if rich.workspace.as_ref().is_some_and(|snapshot| {
                            snapshot.surface_generation == surface_generation
                        }) {
                            rich.status = None;
                            rich.status_rev = 0;
                        }
                    }
                    Ok(ControlMessage::SemanticSnapshot(document)) => {
                        if rich.granted.contains(&Feature::RichSemanticTextV1)
                            && rich.workspace.as_ref().is_some_and(|snapshot| {
                                snapshot.surface_generation == document.surface_generation
                            })
                        {
                            if let Ok(Some(next)) = prismattyc_protocol::apply_semantic_snapshot(
                                rich.semantics.get(&document.document_id),
                                document,
                            ) {
                                rich.semantics.insert(next.document_id.clone(), next);
                            }
                        }
                    }
                    Ok(ControlMessage::SemanticCopy(copy)) => {
                        let workspace_ok = rich.workspace.as_ref().is_some_and(|snapshot| {
                            snapshot.surface_generation == copy.surface_generation
                        });
                        if workspace_ok {
                            if let Some(doc) = rich.semantics.get(&copy.document_id) {
                                if let Ok(text) = doc.project_copy(&copy) {
                                    if !text.is_empty() {
                                        if let Some(owner) = rich.copy_requests.pop_front() {
                                            rich.push_copy(owner, text);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(ControlMessage::CollectionSnapshot(snapshot)) => {
                        for bytes in apply_mux_collection_snapshot(rich, snapshot) {
                            send_bytes(to_child, rich, bytes, None);
                        }
                    }
                    Ok(ControlMessage::CollectionPatch(patch)) => {
                        for bytes in apply_mux_collection_patch(rich, patch) {
                            send_bytes(to_child, rich, bytes, None);
                        }
                    }
                    Ok(ControlMessage::CollectionDrop {
                        surface_generation,
                        collection_id,
                    }) => {
                        if rich.granted.contains(&Feature::RichCollectionV1) {
                            rich.collections.remove(&collection_id);
                            if let Ok(bytes) =
                                encode_collection_ack(surface_generation, &collection_id, 1)
                            {
                                send_bytes(to_child, rich, bytes, None);
                            }
                        }
                    }
                    Ok(ControlMessage::FocusReply { .. })
                    | Ok(ControlMessage::FocusKey { .. })
                    | Ok(ControlMessage::InputEvent(_))
                    | Ok(ControlMessage::CapabilityReply(_))
                    | Ok(ControlMessage::CollectionAck { .. })
                    | Ok(ControlMessage::CollectionReject { .. })
                    | Ok(ControlMessage::CollectionResnapshot { .. }) => {}
                    Err(_) => {
                        if prismattyc_protocol::body_has_prefix(body, "workspace;") {
                            rich.drop_workspace();
                        } else if prismattyc_protocol::body_has_prefix(body, "collection;") {
                            if let Some(id) = collection_id_from_body(body) {
                                rich.collections.remove(&id);
                            } else {
                                rich.collections.clear();
                            }
                        } else if prismattyc_protocol::body_has_prefix(body, "status;") {
                            rich.status = None;
                        }
                    }
                }
            }
        }
    }
}

fn collection_id_from_body(body: &str) -> Option<String> {
    body.split(';').find_map(|part| {
        part.strip_prefix("id=")
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .map(str::to_string)
    })
}

fn apply_mux_collection_snapshot(
    rich: &mut RichSession,
    snapshot: CollectionSnapshot,
) -> Vec<Vec<u8>> {
    if !rich.granted.contains(&Feature::RichCollectionV1) {
        return Vec::new();
    }
    let generation = snapshot.surface_generation;
    let id = snapshot.collection_id.clone();
    if !rich.collections.contains_key(&id)
        && rich.collections.len() >= DEFAULT_LIMIT_COLLECTIONS as usize
    {
        return rich.collection_reply(generation, &id, Err(CollectionRejectReason::Backpressure));
    }
    let result = apply_collection_snapshot(rich.collections.get(&id), snapshot);
    rich.collection_reply(generation, &id, result)
}

fn apply_mux_collection_patch(rich: &mut RichSession, patch: CollectionPatch) -> Vec<Vec<u8>> {
    if !rich.granted.contains(&Feature::RichCollectionV1) {
        return Vec::new();
    }
    let generation = patch.surface_generation;
    let id = patch.collection_id.clone();
    let result = apply_collection_patch(rich.collections.get(&id), patch).map(Some);
    rich.collection_reply(generation, &id, result)
}

fn insert_attachment(rich: &mut RichSession, feature: Feature, attach: Attachment, id: u32) {
    if !rich.granted.contains(&feature) {
        return;
    }
    if !rich.can_insert(id) {
        return;
    }
    rich.attachments.insert(id, attach);
}

fn translate_attachments_for_scroll(rich: &mut RichSession, scrolled_lines: u64) {
    if scrolled_lines <= rich.last_scrolled_lines {
        rich.last_scrolled_lines = scrolled_lines;
        return;
    }
    let delta = scrolled_lines - rich.last_scrolled_lines;
    rich.last_scrolled_lines = scrolled_lines;
    let delta_i = i32::try_from(delta).unwrap_or(i32::MAX);
    rich.attachments.retain(|_, attach| {
        if attach.kind == AttachKind::Viewport {
            return true;
        }
        attach.row = attach.row.saturating_sub(delta_i);
        attach.row.saturating_add(i32::from(attach.rows)) > 0
    });
}

fn overlay_run(run: &StyledRun) -> OverlayRun {
    OverlayRun {
        text: run.text.clone(),
        fg: run.fg,
        bg: run.bg,
        bold: run.bold,
        italic: run.italic,
        underline: run.underline,
        inverse: run.inverse,
    }
}

/// Overlays never compose on the alternate screen. History pan hides
/// cell_rect (grid-anchored) and keeps viewport.
pub(crate) fn snapshot_overlays(
    emulator: &Emulator,
    rich: &RichSession,
    view_offset: u32,
) -> Vec<PaneOverlay> {
    if emulator.screen().alt_active() {
        return Vec::new();
    }
    let screen_rows_i = i32::try_from(emulator.screen().rows()).unwrap_or(i32::MAX);
    let mut out = Vec::new();
    for (id, attach) in &rich.attachments {
        if attach.kind == AttachKind::CellRect && view_offset > 0 {
            continue;
        }
        if attach.row >= screen_rows_i {
            continue;
        }
        out.push(PaneOverlay {
            id: *id,
            kind: match attach.kind {
                AttachKind::CellRect => OverlayKind::CellRect,
                AttachKind::Viewport => OverlayKind::Viewport,
            },
            row: attach.row,
            col: attach.col,
            rows: attach.rows,
            cols: attach.cols,
            text: attach.text.clone(),
            runs: attach.runs.iter().map(overlay_run).collect(),
        });
    }
    out
}

pub(crate) fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_protocol::{
        encode_attach_cell_rect, encode_capability_query, encode_status_snapshot,
        encode_workspace_snapshot, CapabilityQuery, ProtocolVersion, RequestId, StatusItem,
        StatusSnapshot, StatusTone, StatusVisual, TreeNode, TreeNodeKind, WorkspaceRows,
        WorkspaceSnapshot,
    };

    fn encode_query() -> Vec<u8> {
        encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .expect("query")
    }

    fn workspace() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: WorkspaceRows {
                min: 5,
                preferred: 8,
                max: 12,
            },
            nodes: vec![TreeNode {
                id: 1,
                parent: 0,
                kind: TreeNodeKind::Text,
                min: 5,
                preferred: 8,
                fill: 1,
                show_min_cols: 0,
                show_max_cols: 0,
                action_id: 7,
                text: "Runbook\n> check ready".into(),
            }],
        }
    }

    #[test]
    fn workspace_snapshot_reserves_rows_and_malformed_tree_drops_surface() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok(), "capability reply flushed");
        let snapshot = encode_workspace_snapshot(&workspace()).unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &snapshot);
        assert_eq!(rich.workspace_layout(24, 80).unwrap().unwrap().rows, 8);

        process_rich_chunk(&mut emulator, &mut rich, &tx, &{
            use prismattyc_protocol::{
                encode_collection_snapshot, CollectionItem, CollectionSnapshot,
            };
            encode_collection_snapshot(&CollectionSnapshot {
                surface_generation: 1,
                collection_id: "diag".into(),
                rev: 1,
                items: vec![CollectionItem {
                    id: 1,
                    replaceable: false,
                    text: "reattach diag".into(),
                }],
            })
            .unwrap()
        });
        assert_eq!(
            rich.collections
                .get("diag")
                .and_then(|collection| collection.items.first())
                .map(|item| item.text.as_str()),
            Some("reattach diag")
        );
        let painted = rich
            .workspace_layout(24, 80)
            .unwrap()
            .unwrap()
            .lines
            .join("\n");
        assert!(painted.contains("Runbook"));
        assert!(painted.contains("> check ready"));
        assert!(!painted.contains("cache:diag"));
        assert!(!painted.contains("reattach diag"));

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            b"\x1b_Prismattyc;workspace;snapshot;generation=1;rev=2;min=5;preferred=8;max=12;nodes=1,0,future,5,8,1,0,0,x\x1b\\",
        );
        assert!(rich.workspace_layout(24, 80).unwrap().is_none());
        let _ = emulator.feed(b"classic-still-live");
        assert!(emulator.screen().content_epoch() > 0);
    }

    #[test]
    fn status_snapshot_survives_mux_layout_without_animation_state() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok());
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace()).unwrap(),
        );
        let status = StatusSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rev: 1,
            items: vec![StatusItem {
                node_id: 1,
                tone: StatusTone::Info,
                visual: StatusVisual::Meter {
                    current: 0,
                    total: None,
                },
            }],
        };
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&status).unwrap(),
        );
        let first = rich.workspace_layout(24, 80).unwrap().unwrap();
        let second = rich.workspace_layout(24, 80).unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.status_runs[0].tone, StatusTone::Info);

        let mut invalid_ref = status.clone();
        invalid_ref.rev = 2;
        invalid_ref.items[0].node_id = 999;
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&invalid_ref).unwrap(),
        );
        assert!(rich
            .workspace_layout(24, 80)
            .unwrap()
            .unwrap()
            .status_runs
            .is_empty());
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&status).unwrap(),
        );
        assert!(rich
            .workspace_layout(24, 80)
            .unwrap()
            .unwrap()
            .status_runs
            .is_empty());
    }

    #[test]
    fn workspace_hit_ignores_other_actionable_placements() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok(), "capability reply flushed");
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&two_action_workspace()).unwrap(),
        );
        let layout = rich.workspace_layout(24, 80).unwrap().unwrap();
        let first = *layout.placements.get(&2).expect("first action");
        let later = *layout.placements.get(&3).expect("later action");
        assert!(later.row > first.row, "later node must sit below first");
        let hit = rich
            .workspace_hit(24, 80, first.row, first.col)
            .expect("first placement");
        assert_eq!(hit.node_id, 2);
        assert_eq!(hit.action_id, 11);
        assert_eq!(hit.row, 0);
        assert_eq!(hit.col, 0);
        let beyond_row = later.row.saturating_add(later.rows);
        assert!(
            rich.workspace_hit(24, 80, beyond_row, later.col).is_none(),
            "pointer beyond the later placement must not overflow or steal"
        );
    }

    fn two_action_workspace() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rows: WorkspaceRows {
                min: 5,
                preferred: 8,
                max: 12,
            },
            nodes: vec![
                TreeNode {
                    id: 1,
                    parent: 0,
                    kind: TreeNodeKind::Column,
                    min: 5,
                    preferred: 8,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: String::new(),
                },
                TreeNode {
                    id: 2,
                    parent: 1,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 0,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 11,
                    text: "> first".into(),
                },
                TreeNode {
                    id: 3,
                    parent: 1,
                    kind: TreeNodeKind::Text,
                    min: 1,
                    preferred: 1,
                    fill: 0,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 22,
                    text: "> later".into(),
                },
            ],
        }
    }

    #[test]
    fn structured_pointer_is_viewer_bound_and_one_cell_motion_cancels_activation() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok(), "capability reply flushed");
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace()).unwrap(),
        );

        let viewer = ViewerId::from_bytes([0x44; 16]);
        let other = ViewerId::from_bytes([0x55; 16]);
        let decode = |bytes: &[u8]| {
            decode_body(std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap()).unwrap()
        };
        let ControlMessage::InputEvent(focus) =
            decode(&rich.toggle_structured_focus(viewer).unwrap())
        else {
            panic!("expected structured focus event");
        };
        assert_eq!(focus.request_seq, 1);
        assert_eq!(focus.action_id, 7);
        assert!(rich
            .encode_structured_key(other, "Enter".into(), InputModifiers::default())
            .is_none());

        let ControlMessage::InputEvent(press) = decode(
            &rich
                .handle_structured_pointer(viewer, 24, 80, PointerPhase::Press, 2, 3)
                .unwrap(),
        ) else {
            panic!("expected pointer press");
        };
        assert_eq!(press.request_seq, 2);
        assert_eq!(
            press.kind,
            InputEventKind::Pointer {
                phase: PointerPhase::Press,
                row: 2,
                col: 3,
            }
        );
        assert!(rich
            .handle_structured_pointer(viewer, 24, 80, PointerPhase::Move, 2, 4)
            .is_none());
        assert!(rich
            .handle_structured_pointer(viewer, 24, 80, PointerPhase::Release, 2, 4)
            .is_none());

        let ControlMessage::InputEvent(second_press) = decode(
            &rich
                .handle_structured_pointer(viewer, 24, 80, PointerPhase::Press, 2, 3)
                .unwrap(),
        ) else {
            panic!("expected second pointer press");
        };
        assert_eq!(second_press.request_seq, 3);
        let ControlMessage::InputEvent(activate) = decode(
            &rich
                .handle_structured_pointer(viewer, 24, 80, PointerPhase::Release, 2, 3)
                .unwrap(),
        ) else {
            panic!("expected pointer activation");
        };
        assert_eq!(activate.request_seq, 4);
        assert_eq!(
            activate.kind,
            InputEventKind::Pointer {
                phase: PointerPhase::Activate,
                row: 2,
                col: 3,
            }
        );
        rich.structured_focus.as_mut().unwrap().rate_count = DEFAULT_LIMIT_EVENT_RATE;
        assert!(rich
            .encode_structured_key(viewer, "Enter".into(), InputModifiers::default())
            .is_none());
        let ControlMessage::InputEvent(revoke) =
            decode(&rich.revoke_structured_focus(viewer).unwrap())
        else {
            panic!("expected rate-limit-independent revoke");
        };
        assert_eq!(revoke.request_seq, 5);
        assert_eq!(revoke.kind, InputEventKind::Focus { focused: false });
        rich.semantics.insert(
            "diag".into(),
            prismattyc_protocol::SemanticDocument {
                surface_generation: 1,
                document_id: "diag".into(),
                rev: 1,
                text: "error crates/foo.rs:10:1".into(),
                spans: vec![prismattyc_protocol::SemanticSpan {
                    start: 6,
                    end: 24,
                    role: prismattyc_protocol::SemanticRole::Location,
                }],
                selection: Some(prismattyc_protocol::SemanticRange {
                    rev: 1,
                    start: 6,
                    end: 24,
                }),
            },
        );
        assert_eq!(
            rich.semantic_copy_text("diag").as_deref(),
            Some("crates/foo.rs:10:1")
        );
    }

    #[test]
    fn grant_required_before_attach() {
        let (tx, _rx) = mpsc::sync_channel(8);
        let mut emulator = Emulator::new_experimental(40, 10, 0);
        let mut rich = RichSession::default();
        let attach = encode_attach_cell_rect(&prismattyc_protocol::AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .expect("attach");
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert!(snapshot_overlays(&emulator, &rich, 0).is_empty());
    }

    #[test]
    fn query_then_attach_lands_on_snapshot() {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut emulator = Emulator::new_experimental(40, 10, 0);
        let mut rich = RichSession::default();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &encode_query());
        assert!(rx.try_recv().is_ok(), "capability reply flushed");
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));
        let attach = encode_attach_cell_rect(&prismattyc_protocol::AttachCellRect {
            id: 7,
            row: 1,
            col: 2,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .expect("attach");
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        let overlays = snapshot_overlays(&emulator, &rich, 0);
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].id, 7);
        assert_eq!(overlays[0].kind, OverlayKind::CellRect);
        assert_eq!(overlays[0].text, "STAT");
        assert!(snapshot_overlays(&emulator, &rich, 3).is_empty());
    }

    #[test]
    fn control_events_detach_workspace_drop_and_collection_drop() {
        use prismattyc_protocol::{
            encode_collection_drop, encode_collection_snapshot, encode_detach, encode_status_drop,
            encode_workspace_drop, CollectionItem, CollectionSnapshot,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok(), "capability reply flushed");

        let attach = encode_attach_cell_rect(&prismattyc_protocol::AttachCellRect {
            id: 7,
            row: 1,
            col: 2,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .expect("attach");
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert_eq!(snapshot_overlays(&emulator, &rich, 0).len(), 1);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_detach(7).expect("detach"),
        );
        assert!(
            snapshot_overlays(&emulator, &rich, 0).is_empty(),
            "Detach must drop the cell-rect"
        );

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace()).unwrap(),
        );
        assert!(rich.workspace_layout(24, 80).unwrap().is_some());
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_drop(1).expect("workspace drop"),
        );
        assert!(
            rich.workspace_layout(24, 80).unwrap().is_none(),
            "WorkspaceDrop matching generation must clear the surface"
        );

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace()).unwrap(),
        );
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&StatusSnapshot {
                surface_generation: 1,
                scene_rev: 1,
                rev: 1,
                items: vec![StatusItem {
                    node_id: 1,
                    tone: StatusTone::Info,
                    visual: StatusVisual::Meter {
                        current: 0,
                        total: None,
                    },
                }],
            })
            .unwrap(),
        );
        assert!(!rich
            .workspace_layout(24, 80)
            .unwrap()
            .unwrap()
            .status_runs
            .is_empty());
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_drop(1).expect("status drop"),
        );
        assert!(
            rich.workspace_layout(24, 80)
                .unwrap()
                .unwrap()
                .status_runs
                .is_empty(),
            "StatusDrop matching workspace generation must clear status"
        );

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_collection_snapshot(&CollectionSnapshot {
                surface_generation: 1,
                collection_id: "diag".into(),
                rev: 1,
                items: vec![CollectionItem {
                    id: 1,
                    replaceable: false,
                    text: "reattach diag".into(),
                }],
            })
            .unwrap(),
        );
        assert!(rich.collections.contains_key("diag"));
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_collection_drop(1, "diag").expect("collection drop"),
        );
        assert!(
            !rich.collections.contains_key("diag"),
            "CollectionDrop must remove the collection"
        );
        assert!(rx.try_recv().is_ok(), "collection ack flushed");
    }

    #[test]
    fn flag_off_json_omits_overlays() {
        let styled = crate::control::PaneStyled {
            content: crate::control::PaneContent {
                pane_id: 1,
                revision: 1,
                cols: 8,
                rows: 1,
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                alt_active: false,
                child_alive: true,
                child_pid: None,
                lines: vec!["abcd".into()],
                cursor_shape: None,
            },
            runs: vec![vec![]],
            workspace: Vec::new(),
            view_offset: Some(0),
            max_view_scroll: Some(0),
            child_mouse_tracking: Some(false),
            child_mouse_sgr: Some(false),
            overlays: Vec::new(),
            experimental_rich: false,
            rich_focus_id: None,
            structured_focus: false,
            semantic_clipboard: None,
            semantic_clipboard_seq: None,
            workspace_inverse: Vec::new(),
            workspace_styles: Vec::new(),
        };
        let json = serde_json::to_string(&styled).unwrap();
        assert!(
            !json.contains("overlays"),
            "empty overlays must omit the key: {json}"
        );
        assert!(
            !json.contains("workspace"),
            "empty workspace must omit the key: {json}"
        );
        assert!(
            !json.contains("experimental_rich"),
            "flag-off must omit experimental_rich: {json}"
        );
        assert!(
            !json.contains("rich_focus_id"),
            "idle focus must omit rich_focus_id: {json}"
        );
    }

    #[test]
    fn focus_query_does_not_grant_until_toggle() {
        use prismattyc_protocol::encode_focus_query;
        let (tx, rx) = mpsc::sync_channel(8);
        let mut emulator = Emulator::new_experimental(40, 10, 0);
        let mut rich = RichSession::default();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &encode_query());
        let _ = rx.try_recv();
        let attach = encode_attach_cell_rect(&prismattyc_protocol::AttachCellRect {
            id: 3,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .expect("attach");
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_focus_query(3).expect("query"),
        );
        assert!(rich.focus_id().is_none(), "never auto-grant");
        let viewer = ViewerId::from_bytes([0x33; 16]);
        let grant = rich.toggle_focus(viewer).expect("grant bytes");
        assert_eq!(rich.focus_id(), Some(3));
        assert!(!grant.is_empty());
        let revoke = rich.toggle_focus(viewer).expect("revoke bytes");
        assert!(rich.focus_id().is_none());
        assert!(!revoke.is_empty());
    }

    #[test]
    fn semantic_copy_rejects_stale_generation() {
        use prismattyc_protocol::{
            encode_semantic_copy, encode_semantic_snapshot, SemanticCopy, SemanticDocument,
            SemanticRange, SemanticRole, SemanticSpan,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &query);
        assert!(rx.try_recv().is_ok(), "capability reply flushed");
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace()).unwrap(),
        );
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_semantic_snapshot(&SemanticDocument {
                surface_generation: 1,
                document_id: "diag".into(),
                rev: 2,
                text: "error crates/foo.rs:10:1".into(),
                spans: vec![SemanticSpan {
                    start: 6,
                    end: 24,
                    role: SemanticRole::Location,
                }],
                selection: Some(SemanticRange {
                    rev: 2,
                    start: 6,
                    end: 24,
                }),
            })
            .unwrap(),
        );
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_semantic_copy(&SemanticCopy {
                surface_generation: 99,
                document_id: "diag".into(),
                rev: 2,
                start: 6,
                end: 24,
            })
            .unwrap(),
        );
        assert!(rich.take_semantic_copy().is_none());
        rich.queue_copy_request(7);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_semantic_copy(&SemanticCopy {
                surface_generation: 1,
                document_id: "diag".into(),
                rev: 2,
                start: 6,
                end: 24,
            })
            .unwrap(),
        );
        assert_eq!(
            rich.take_semantic_copy().as_deref(),
            Some("crates/foo.rs:10:1")
        );
    }

    #[test]
    fn mux_server_stores_inline_image() {
        const B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        // Use the plain constructor: graphics must not depend on the experimental path.
        let mut emulator = Emulator::new(80, 24, 10);
        let seq = format!("\x1b_Ga=T,t=d,f=100,i=11;{B64}\x1b\\");
        let _ = emulator.feed(seq.as_bytes());
        assert_eq!(emulator.images().len(), 1);
        assert_eq!(emulator.images()[0].id, 11);
    }
}
