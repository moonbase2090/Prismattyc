//! Per-pane experimental rich session.
//!
//! Windowed host rich peer: frozen 0.1/0.2 overlays plus the bounded 0.3
//! reserved-workspace/tree slice. Grant-after-flush is unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use prismattyc_emulator::Emulator;
use prismattyc_protocol::{
    apply_collection_patch, apply_collection_snapshot, decode_body, encode_capability_reply,
    encode_collection_ack, encode_collection_reject, encode_collection_resnapshot,
    encode_focus_key, encode_focus_reply, encode_input_event, CapabilityReply, CollectedApc,
    CollectionPatch, CollectionRejectReason, CollectionSnapshot, ControlMessage, Feature,
    FocusStatus, InputEvent, InputEventKind, InputModifiers, PointerPhase, RegionKind,
    StatusSnapshot, StyledRun, TreeNodeKind, ViewerId, WorkspaceSnapshot,
    DEFAULT_LIMIT_COLLECTIONS, DEFAULT_LIMIT_EVENT_RATE, DEFAULT_LIMIT_REGIONS, LIMIT_REGIONS,
    MAX_CELL_RECT_ATTACHMENTS,
};
use prismattyc_render::{
    layout_workspace_with_status, CellRectOverlay, OverlayRun, WorkspaceLayout,
};

/// Bytes for the child PTY writer thread (keys, paste, DSR, capability replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildWrite {
    pub bytes: Vec<u8>,
    /// Applied to [`RichSession`] only after a successful PTY write.
    pub capability_grant: Option<CapabilityGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapabilityGrant {
    pub features: BTreeSet<Feature>,
    pub region_limit: usize,
}

impl ChildWrite {
    pub(crate) fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            capability_grant: None,
        }
    }

    fn capability(bytes: Vec<u8>, grant: CapabilityGrant) -> Self {
        Self {
            bytes,
            capability_grant: Some(grant),
        }
    }
}

impl From<Vec<u8>> for ChildWrite {
    fn from(bytes: Vec<u8>) -> Self {
        Self::bytes(bytes)
    }
}

impl AsRef<[u8]> for ChildWrite {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Host-side experimental rich session state (negotiation + attachments).
#[derive(Debug, Default)]
pub(crate) struct RichSession {
    /// Features successfully written to the child as a capability reply.
    pub granted: BTreeSet<Feature>,
    region_limit: usize,
    attachments: BTreeMap<u32, HostAttachment>,
    last_scrolled_lines: u64,
    /// Region ids the client asked to make focus-eligible. Never auto-granted.
    focus_requested: BTreeSet<u32>,
    /// Region currently holding host-granted rich focus, if any.
    focus_id: Option<u32>,
    structured_focus: Option<StructuredFocus>,
    workspace: Option<WorkspaceSnapshot>,
    status: Option<StatusSnapshot>,
    status_rev: u64,
    collections: BTreeMap<String, CollectionSnapshot>,
    semantics: BTreeMap<String, prismattyc_protocol::SemanticDocument>,
    pending_semantic_copy: Option<String>,
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

impl RichSession {
    pub(crate) fn apply_grant(&mut self, grant: CapabilityGrant) {
        self.granted = grant.features;
        self.region_limit = grant.region_limit;
        if !self.granted.contains(&Feature::InputRichFocus) {
            self.focus_requested.clear();
            self.focus_id = None;
            self.structured_focus = None;
        }
        if !self.granted.contains(&Feature::HybridReserveRows)
            || !self.granted.contains(&Feature::RichTreeV1)
        {
            self.workspace = None;
            self.status = None;
            self.status_rev = 0;
            self.structured_focus = None;
        }
        if !self.granted.contains(&Feature::RichStatusV1) {
            self.status = None;
            self.status_rev = 0;
        }
        if !self.granted.contains(&Feature::InputRichKeyboardV1)
            && !self.granted.contains(&Feature::InputRichPointerV1)
            && !self.granted.contains(&Feature::InputRichScrollV1)
        {
            self.structured_focus = None;
        }
        if !self.granted.contains(&Feature::RichCollectionV1) {
            self.collections.clear();
        }
    }

    fn can_insert(&self, id: u32) -> bool {
        self.attachments.contains_key(&id) || self.attachments.len() < self.region_limit
    }

    pub(crate) fn focus_id(&self) -> Option<u32> {
        self.focus_id
    }

    pub(crate) fn focused_attachment(&self) -> Option<(u32, i32, u16, u16, u16)> {
        let id = self.focus_id?;
        let attach = self.attachments.get(&id)?;
        Some((id, attach.row, attach.col, attach.rows, attach.cols))
    }

    /// Record a client request. Does not grant; the host chord does that.
    fn note_focus_request(&mut self, id: u32) {
        if !self.granted.contains(&Feature::InputRichFocus) {
            return;
        }
        if self.attachments.contains_key(&id) {
            self.focus_requested.insert(id);
        }
    }

    /// Toggle grant for the lowest requested attached region. Returns the
    /// reply to write to the child, if any.
    pub(crate) fn toggle_focus(&mut self) -> Option<Vec<u8>> {
        if !self.granted.contains(&Feature::InputRichFocus) {
            return None;
        }
        if let Some(id) = self.focus_id.take() {
            return encode_focus_reply(id, FocusStatus::Revoked).ok();
        }
        let id = self
            .focus_requested
            .iter()
            .copied()
            .find(|id| self.attachments.contains_key(id))?;
        self.focus_id = Some(id);
        encode_focus_reply(id, FocusStatus::Granted).ok()
    }

    pub(crate) fn revoke_focus(&mut self) -> Option<Vec<u8>> {
        let id = self.focus_id.take()?;
        encode_focus_reply(id, FocusStatus::Revoked).ok()
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
            let hit = WorkspaceHit {
                node_id: focus.node_id,
                action_id: focus.action_id,
                row: 0,
                col: 0,
            };
            let encoded =
                self.encode_structured(viewer_id, hit, InputEventKind::Focus { focused: false });
            self.structured_focus = None;
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
        encoded
    }

    pub(crate) fn structured_focus_active(&self, viewer_id: ViewerId) -> bool {
        self.structured_focus
            .is_some_and(|focus| focus.viewer_id == viewer_id)
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

    pub(crate) fn encode_structured_pointer(
        &mut self,
        viewer_id: ViewerId,
        hit: WorkspaceHit,
        phase: PointerPhase,
    ) -> Option<Vec<u8>> {
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

    pub(crate) fn encode_structured_scroll(
        &mut self,
        viewer_id: ViewerId,
        hit: WorkspaceHit,
        delta: i16,
    ) -> Option<Vec<u8>> {
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
    }

    pub(crate) fn take_semantic_copy(&mut self) -> Option<String> {
        self.pending_semantic_copy.take()
    }

    #[cfg_attr(not(test), allow(dead_code))]
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

    pub(crate) fn encode_focused_key(&self, key: &str) -> Option<Vec<u8>> {
        let id = self.focus_id?;
        encode_focus_key(id, key).ok()
    }

    fn drop_region(&mut self, id: u32) -> Option<Vec<u8>> {
        self.attachments.remove(&id);
        self.focus_requested.remove(&id);
        if self.focus_id == Some(id) {
            self.focus_id = None;
            return encode_focus_reply(id, FocusStatus::Revoked).ok();
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn attachments_len_for_test(&self) -> usize {
        self.attachments.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachKind {
    CellRect,
    Viewport,
}

/// Attachment stored with a signed row (cell-rect translates; viewport does not).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostAttachment {
    kind: AttachKind,
    row: i32,
    col: u16,
    rows: u16,
    cols: u16,
    text: String,
    runs: Vec<StyledRun>,
}

pub(crate) fn drain_emulator_replies(
    emulator: &mut Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
) {
    for reply in emulator.take_pending_replies() {
        let _ = to_child.try_send(ChildWrite::bytes(reply));
    }
}

/// Feed child output, preserving scroll-vs-APC order inside one PTY chunk.
///
/// Plain-text floods (no ESC, collector idle) are fed as a single slice so
/// `--experimental-rich` cannot stall the event loop. Control-bearing
/// slices still split on ESC so a completed APC and a grid scroll cannot share
/// one `feed` (collector runs before the VT parser).
pub(crate) fn process_rich_chunk(
    emulator: &mut Emulator,
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<ChildWrite>,
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
    to_child: &mpsc::SyncSender<ChildWrite>,
    slice: &[u8],
) {
    if slice.is_empty() {
        return;
    }
    let before = emulator.screen().scrolled_lines();
    let events = emulator.feed(slice);
    drain_emulator_replies(emulator, to_child);
    let after = emulator.screen().scrolled_lines();
    if after > before {
        if let Some(bytes) = translate_attachments_for_scroll(rich, after) {
            let _ = to_child.try_send(ChildWrite::bytes(bytes));
        }
    }
    if !events.is_empty() {
        handle_control_events(&events, rich, to_child);
    }
}

fn handle_control_events(
    events: &[CollectedApc],
    rich: &mut RichSession,
    to_child: &mpsc::SyncSender<ChildWrite>,
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
                                let _ = to_child.try_send(ChildWrite::capability(bytes, grant));
                            }
                        }
                    }
                    Ok(ControlMessage::AttachCellRect(attach)) => {
                        insert_attachment(
                            rich,
                            Feature::HybridAttachCellRect,
                            HostAttachment {
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
                            HostAttachment {
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
                            HostAttachment {
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
                            let _ = to_child.try_send(ChildWrite::bytes(bytes));
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
                                        rich.pending_semantic_copy = Some(text);
                                    }
                                }
                            }
                        }
                    }
                    Ok(ControlMessage::CollectionSnapshot(snapshot)) => {
                        for bytes in apply_host_collection_snapshot(rich, snapshot) {
                            let _ = to_child.try_send(ChildWrite::bytes(bytes));
                        }
                    }
                    Ok(ControlMessage::CollectionPatch(patch)) => {
                        for bytes in apply_host_collection_patch(rich, patch) {
                            let _ = to_child.try_send(ChildWrite::bytes(bytes));
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
                                let _ = to_child.try_send(ChildWrite::bytes(bytes));
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

fn apply_host_collection_snapshot(
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

fn apply_host_collection_patch(rich: &mut RichSession, patch: CollectionPatch) -> Vec<Vec<u8>> {
    if !rich.granted.contains(&Feature::RichCollectionV1) {
        return Vec::new();
    }
    let generation = patch.surface_generation;
    let id = patch.collection_id.clone();
    let result = apply_collection_patch(rich.collections.get(&id), patch).map(Some);
    rich.collection_reply(generation, &id, result)
}

fn insert_attachment(rich: &mut RichSession, feature: Feature, attach: HostAttachment, id: u32) {
    if !rich.granted.contains(&feature) {
        return;
    }
    if !rich.can_insert(id) {
        return;
    }
    rich.attachments.insert(id, attach);
}

/// Translate cell-rect attachments with primary-grid scroll; detach when fully
/// outside the visible viewport.
fn translate_attachments_for_scroll(
    rich: &mut RichSession,
    scrolled_lines: u64,
) -> Option<Vec<u8>> {
    if scrolled_lines <= rich.last_scrolled_lines {
        rich.last_scrolled_lines = scrolled_lines;
        return None;
    }
    let delta = scrolled_lines - rich.last_scrolled_lines;
    rich.last_scrolled_lines = scrolled_lines;
    let delta_i = i32::try_from(delta).unwrap_or(i32::MAX);
    let mut gone = Vec::new();
    rich.attachments.retain(|id, attach| {
        if attach.kind == AttachKind::Viewport {
            return true;
        }
        attach.row = attach.row.saturating_sub(delta_i);
        let height = i32::from(attach.rows);
        let keep = attach.row.saturating_add(height) > 0;
        if !keep {
            gone.push(*id);
        }
        keep
    });
    let mut dropped_focus = None;
    for id in gone {
        rich.focus_requested.remove(&id);
        if rich.focus_id == Some(id) {
            rich.focus_id = None;
            dropped_focus = Some(id);
        }
    }
    dropped_focus.and_then(|id| encode_focus_reply(id, FocusStatus::Revoked).ok())
}

fn overlay_from(attach: &HostAttachment) -> CellRectOverlay {
    CellRectOverlay {
        row: attach.row,
        col: usize::from(attach.col),
        rows: usize::from(attach.rows).max(1),
        cols: usize::from(attach.cols).max(1),
        text: attach.text.clone(),
        runs: attach
            .runs
            .iter()
            .map(|run| OverlayRun {
                text: run.text.clone(),
                fg: run.fg,
                bg: run.bg,
                bold: run.bold,
                italic: run.italic,
                underline: run.underline,
                inverse: run.inverse,
            })
            .collect(),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct VisibleOverlays {
    pub cell_rect: Vec<CellRectOverlay>,
    pub viewport: Vec<CellRectOverlay>,
}

impl VisibleOverlays {
    pub(crate) fn is_empty(&self) -> bool {
        self.cell_rect.is_empty() && self.viewport.is_empty()
    }
}

/// Overlays never compose while the child is on the alternate screen.
pub(crate) fn visible_overlays(emulator: &Emulator, rich: &RichSession) -> VisibleOverlays {
    if emulator.screen().alt_active() {
        return VisibleOverlays::default();
    }
    let screen_rows_i = i32::try_from(emulator.screen().rows()).unwrap_or(i32::MAX);
    let mut out = VisibleOverlays::default();
    for attach in rich.attachments.values() {
        if attach.row >= screen_rows_i {
            continue;
        }
        let overlay = overlay_from(attach);
        match attach.kind {
            AttachKind::CellRect => out.cell_rect.push(overlay),
            AttachKind::Viewport => out.viewport.push(overlay),
        }
    }
    out
}

/// Scrollback view policy (ruling): `viewport` overlays are pinned
/// to the viewport and keep painting while the user pans history;
/// `cell_rect` overlays are grid-anchored and stay hidden until the view
/// returns to the live tail (translate-through-history is out of scope).
pub(crate) fn overlays_for_scroll(
    mut overlays: VisibleOverlays,
    view_scroll: usize,
) -> VisibleOverlays {
    if view_scroll > 0 {
        overlays.cell_rect.clear();
    }
    overlays
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::test_time_budget;
    use prismattyc_protocol::{
        encode_attach_cell_rect, encode_attach_viewport, encode_capability_query,
        encode_capability_reply, encode_detach, encode_focus_query, encode_status_snapshot,
        encode_update, encode_workspace_snapshot, AttachCellRect, AttachViewport, CapabilityQuery,
        CapabilityReply, ControlMessage, FocusStatus, ProtocolVersion, RequestId, StatusItem,
        StatusSnapshot, StatusTone, StatusVisual, TreeNode, TreeNodeKind, UpdateAttachment,
        WorkspaceRows, WorkspaceSnapshot,
    };

    fn apply_queued_grants(rx: &mpsc::Receiver<ChildWrite>, rich: &mut RichSession) {
        while let Ok(msg) = rx.try_recv() {
            if let Some(grant) = msg.capability_grant {
                rich.apply_grant(grant);
            }
        }
    }

    fn negotiate_v1(
        emulator: &mut Emulator,
        rich: &mut RichSession,
        tx: &mpsc::SyncSender<ChildWrite>,
    ) {
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        process_rich_chunk(emulator, rich, tx, &query);
    }

    fn negotiate_surface(
        emulator: &mut Emulator,
        rich: &mut RichSession,
        tx: &mpsc::SyncSender<ChildWrite>,
        rx: &mpsc::Receiver<ChildWrite>,
    ) {
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 3),
        })
        .unwrap();
        process_rich_chunk(emulator, rich, tx, &query);
        apply_queued_grants(rx, rich);
    }

    fn workspace(rev: u64) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            surface_generation: 1,
            scene_rev: rev,
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
    fn workspace_is_grant_gated_ordered_and_malformed_drops_only_surface() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        let frame = encode_workspace_snapshot(&workspace(1)).unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &frame);
        assert!(rich.workspace_layout(24, 80).unwrap().is_none());

        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        process_rich_chunk(&mut emulator, &mut rich, &tx, &frame);
        assert_eq!(rich.workspace_layout(24, 80).unwrap().unwrap().rows, 8);
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
    fn status_is_scene_bound_static_and_malformed_drops_only_status() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace(1)).unwrap(),
        );
        let status = StatusSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rev: 1,
            items: vec![StatusItem {
                node_id: 1,
                tone: StatusTone::Success,
                visual: StatusVisual::Badge,
            }],
        };
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&status).unwrap(),
        );
        let layout = rich.workspace_layout(24, 80).unwrap().unwrap();
        assert!(layout.lines[0].starts_with("[Runbook]"));
        assert_eq!(layout.status_runs[0].tone, StatusTone::Success);

        let mut invalid_ref = status.clone();
        invalid_ref.rev = 2;
        invalid_ref.items[0].node_id = 999;
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_status_snapshot(&invalid_ref).unwrap(),
        );
        let layout = rich.workspace_layout(24, 80).unwrap().unwrap();
        assert!(layout.status_runs.is_empty());
        assert!(layout.lines[0].starts_with("Runbook"));
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

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            b"\x1b_Prismattyc;status;snapshot;generation=1;scene=1;rev=3;items=1,meter,info,6,5\x1b\\",
        );
        let layout = rich.workspace_layout(24, 80).unwrap().unwrap();
        assert!(layout.status_runs.is_empty());
        assert!(layout.lines[0].starts_with("Runbook"));

        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace(2)).unwrap(),
        );
        assert!(rich
            .workspace_layout(24, 80)
            .unwrap()
            .unwrap()
            .status_runs
            .is_empty());
    }

    #[test]
    fn structured_focus_mints_ordered_bound_workspace_events() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace(1)).unwrap(),
        );
        let viewer = ViewerId::from_bytes([0x44; 16]);
        let other = ViewerId::from_bytes([0x55; 16]);
        let decode = |bytes: &[u8]| {
            decode_body(std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap()).unwrap()
        };
        let grant = rich.toggle_structured_focus(viewer).unwrap();
        let ControlMessage::InputEvent(grant) = decode(&grant) else {
            panic!("expected input event");
        };
        assert_eq!(grant.request_seq, 1);
        assert_eq!(grant.node_id, 1);
        assert_eq!(grant.action_id, 7);
        assert_eq!(grant.kind, InputEventKind::Focus { focused: true });
        assert!(rich.structured_focus_active(viewer));
        assert!(!rich.structured_focus_active(other));
        assert!(rich
            .encode_structured_key(other, "Enter".into(), InputModifiers::default())
            .is_none());

        let key = rich
            .encode_structured_key(viewer, "Enter".into(), InputModifiers::default())
            .unwrap();
        let ControlMessage::InputEvent(key) = decode(&key) else {
            panic!("expected key event");
        };
        assert_eq!(key.request_seq, 2);
        assert!(matches!(key.kind, InputEventKind::Key { .. }));

        let hit = rich.workspace_hit(24, 80, 2, 3).unwrap();
        let pointer = rich
            .encode_structured_pointer(viewer, hit, PointerPhase::Activate)
            .unwrap();
        let ControlMessage::InputEvent(pointer) = decode(&pointer) else {
            panic!("expected pointer event");
        };
        assert_eq!(pointer.request_seq, 3);
        assert_eq!(pointer.node_id, 1);
        assert_eq!(
            pointer.kind,
            InputEventKind::Pointer {
                phase: PointerPhase::Activate,
                row: 2,
                col: 3,
            }
        );

        let revoke = rich.revoke_structured_focus(viewer).unwrap();
        let ControlMessage::InputEvent(revoke) = decode(&revoke) else {
            panic!("expected revoke event");
        };
        assert_eq!(revoke.request_seq, 4);
        assert_eq!(revoke.kind, InputEventKind::Focus { focused: false });
        assert!(!rich.structured_focus_active(viewer));
    }

    #[test]
    fn workspace_hit_ignores_other_actionable_placements() {
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
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
    fn collection_gap_drops_only_that_collection_and_requests_resnapshot() {
        use prismattyc_protocol::{
            encode_collection_patch, encode_collection_snapshot, CollectionItem, CollectionPatch,
            CollectionPatchKind, CollectionSnapshot,
        };
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(16);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        let snapshot = encode_collection_snapshot(&CollectionSnapshot {
            surface_generation: 1,
            collection_id: "diag".into(),
            rev: 1,
            items: vec![CollectionItem {
                id: 1,
                replaceable: false,
                text: "error".into(),
            }],
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &snapshot);
        assert_eq!(rich.collections.get("diag").map(|c| c.rev), Some(1));
        let gap = encode_collection_patch(&CollectionPatch {
            surface_generation: 1,
            collection_id: "diag".into(),
            base: 4,
            next: 5,
            kind: CollectionPatchKind::Append,
            items: vec![CollectionItem {
                id: 2,
                replaceable: false,
                text: "later".into(),
            }],
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &gap);
        assert!(!rich.collections.contains_key("diag"));
        let replies: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|msg| String::from_utf8_lossy(&msg.bytes).into_owned())
            .collect();
        assert!(replies
            .iter()
            .any(|body| body.contains("collection;reject")));
        assert!(replies
            .iter()
            .any(|body| body.contains("collection;resnapshot")));
        let _ = emulator.feed(b"classic-still-live");
        assert!(emulator.screen().content_epoch() > 0);
    }

    #[test]
    fn cached_collection_does_not_overwrite_workspace_tree() {
        use prismattyc_protocol::{encode_collection_snapshot, CollectionItem, CollectionSnapshot};
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(16);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace(1)).unwrap(),
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
                    text: "clippy failed".into(),
                }],
            })
            .unwrap(),
        );
        assert_eq!(
            rich.collections
                .get("diag")
                .and_then(|collection| collection.items.first())
                .map(|item| item.text.as_str()),
            Some("clippy failed")
        );
        let layout = rich.workspace_layout(24, 80).unwrap().unwrap();
        let joined = layout.lines.join("\n");
        assert!(joined.contains("Runbook"));
        assert!(joined.contains("> check ready"));
        assert!(!joined.contains("cache:diag"));
    }

    #[test]
    fn semantic_snapshot_copies_location_without_decoration() {
        use prismattyc_protocol::{
            encode_semantic_snapshot, SemanticDocument, SemanticRole, SemanticSpan,
        };
        let mut emulator = Emulator::new_experimental(80, 24, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(16);
        negotiate_surface(&mut emulator, &mut rich, &tx, &rx);
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &encode_workspace_snapshot(&workspace(1)).unwrap(),
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
                selection: Some(prismattyc_protocol::SemanticRange {
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
            &prismattyc_protocol::encode_semantic_copy(&prismattyc_protocol::SemanticCopy {
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
        process_rich_chunk(
            &mut emulator,
            &mut rich,
            &tx,
            &prismattyc_protocol::encode_semantic_copy(&prismattyc_protocol::SemanticCopy {
                surface_generation: 99,
                document_id: "diag".into(),
                rev: 2,
                start: 6,
                end: 24,
            })
            .unwrap(),
        );
        assert!(rich.take_semantic_copy().is_none());
        assert_eq!(
            rich.semantic_copy_text("diag").as_deref(),
            Some("crates/foo.rs:10:1")
        );
        assert!(rich
            .semantic_copy_text("diag")
            .unwrap()
            .chars()
            .all(|ch| ch != '│' && ch != '●'));
    }

    fn negotiate(
        emulator: &mut Emulator,
        rich: &mut RichSession,
        tx: &mpsc::SyncSender<ChildWrite>,
    ) {
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        process_rich_chunk(emulator, rich, tx, &query);
    }

    fn attach_stat(
        emulator: &mut Emulator,
        rich: &mut RichSession,
        tx: &mpsc::SyncSender<ChildWrite>,
    ) {
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(emulator, rich, tx, &attach);
    }

    #[test]
    fn grant_required_before_attach() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        attach_stat(&mut emulator, &mut rich, &tx);
        assert!(rich.attachments.is_empty());
        assert!(rx.try_recv().is_err());

        negotiate(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));
        attach_stat(&mut emulator, &mut rich, &tx);
        assert_eq!(rich.attachments.len(), 1);
    }

    #[test]
    fn paint_policy_skips_primary_overlays_on_alt() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        assert_eq!(rich.attachments.len(), 1);
        let _ = emulator.feed(b"\x1b[?1049h");
        assert!(emulator.screen().alt_active());
        assert!(visible_overlays(&emulator, &rich).is_empty());
        assert_eq!(rich.attachments.len(), 1);
        let _ = emulator.feed(b"\x1b[?1049l");
        let resumed = visible_overlays(&emulator, &rich);
        assert_eq!(resumed.cell_rect.len(), 1);
        assert_eq!(resumed.cell_rect[0].text, "STAT");
    }

    #[test]
    fn scroll_translates_then_detaches_fully_above() {
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        assert_eq!(rich.attachments.get(&1).map(|a| a.row), Some(0));

        process_rich_chunk(&mut emulator, &mut rich, &tx, b"aaaaaaaa\r\nbbbbbbbb\r\n");
        assert!(
            rich.attachments.is_empty(),
            "fully scrolled-off attachment must detach"
        );
    }

    #[test]
    fn detach_removes_attachment() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let detach = encode_detach(1).unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &detach);
        assert!(rich.attachments.is_empty());
    }

    #[test]
    fn process_rich_chunk_flood_stays_responsive() {
        let mut emulator = Emulator::new_experimental(80, 24, 1000);
        let mut rich = RichSession::default();
        let (tx, _rx) = mpsc::sync_channel(8);
        let flood = "y\n".repeat(32 * 1024);
        let start = std::time::Instant::now();
        process_rich_chunk(&mut emulator, &mut rich, &tx, flood.as_bytes());
        let elapsed = start.elapsed();
        // Original freeze was indefinite; 1s still catches a hang.
        // 250ms was runner-noise-sensitive (264ms on a loaded CI box).
        assert!(
            elapsed < test_time_budget(std::time::Duration::from_secs(1)),
            "plain flood under experimental-rich stalled ({elapsed:?})"
        );
        assert!(rich.attachments.is_empty());
        let text = emulator
            .screen()
            .viewport_range()
            .map(|range| emulator.screen().extract_text(range))
            .unwrap_or_default();
        assert!(
            text.contains('y'),
            "flood must still paint the classic grid"
        );
    }

    #[test]
    fn v1_negotiate_grants_viewport_and_limits() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        assert!(rich.granted.contains(&Feature::HybridAttachCellRect));
        assert!(rich.granted.contains(&Feature::HybridOverlayViewport));
        assert!(rich.granted.contains(&Feature::InputRichFocus));
        assert_eq!(rich.region_limit, DEFAULT_LIMIT_REGIONS as usize);
    }

    #[test]
    fn v01_query_reply_is_byte_identical_to_spike() {
        let query = CapabilityQuery {
            request_id: RequestId::new(3).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        };
        let spike = CapabilityReply::for_spike_query(query).unwrap();
        let v1 = CapabilityReply::for_v1_query(query).unwrap();
        assert_eq!(spike, v1);
        assert_eq!(
            encode_capability_reply(&spike).unwrap(),
            encode_capability_reply(&v1).unwrap()
        );
    }

    #[test]
    fn update_mutates_text_without_geometry() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let update = encode_update(&UpdateAttachment {
            id: 1,
            text: "NEXT".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &update);
        let attach = rich.attachments.get(&1).unwrap();
        assert_eq!(attach.text, "NEXT");
        assert_eq!(attach.row, 0);
        assert_eq!(attach.cols, 4);
    }

    #[test]
    fn viewport_stays_pinned_while_cell_rect_translates() {
        let mut emulator = Emulator::new_experimental(8, 2, 10);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let viewport = encode_attach_viewport(&AttachViewport {
            id: 2,
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
            text: "HUD".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &viewport);
        process_rich_chunk(&mut emulator, &mut rich, &tx, b"aaaaaaaa\r\nbbbbbbbb\r\n");
        assert!(!rich.attachments.contains_key(&1));
        let hud = rich.attachments.get(&2).unwrap();
        assert_eq!(hud.kind, AttachKind::Viewport);
        assert_eq!(hud.row, 0);
        assert_eq!(hud.text, "HUD");
    }

    fn request_focus(
        emulator: &mut Emulator,
        rich: &mut RichSession,
        tx: &mpsc::SyncSender<ChildWrite>,
        id: u32,
    ) {
        let bytes = encode_focus_query(id).unwrap();
        process_rich_chunk(emulator, rich, tx, &bytes);
    }

    #[test]
    fn rich_focus_grant_revoke_and_detach() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        request_focus(&mut emulator, &mut rich, &tx, 1);

        let grant = rich.toggle_focus().expect("grant");
        let body = std::str::from_utf8(&grant[2..grant.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::FocusReply {
                id: 1,
                status: FocusStatus::Granted
            }
        );
        assert_eq!(rich.focus_id(), Some(1));

        let key = rich.encode_focused_key("C-S-;").expect("key");
        let body = std::str::from_utf8(&key[2..key.len() - 2]).unwrap();
        assert!(body.contains("%3B"));
        assert!(key.len() < 64);

        let revoke = rich.toggle_focus().expect("revoke");
        let body = std::str::from_utf8(&revoke[2..revoke.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::FocusReply {
                id: 1,
                status: FocusStatus::Revoked
            }
        );
        assert_eq!(rich.focus_id(), None);

        let grant = rich.toggle_focus().expect("re-grant");
        let _ = grant;
        let detach = encode_detach(1).unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &detach);
        assert_eq!(rich.focus_id(), None);
        let mut saw_revoked = false;
        while let Ok(msg) = rx.try_recv() {
            if let Ok(body) = std::str::from_utf8(&msg.bytes) {
                if body.contains("status=revoked") {
                    saw_revoked = true;
                }
            }
        }
        assert!(saw_revoked, "detach must emit a revoke reply");
    }

    #[test]
    fn rich_focus_does_not_auto_grant_and_flag_off_has_no_focus() {
        let mut emulator = Emulator::new_experimental(12, 2, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        request_focus(&mut emulator, &mut rich, &tx, 1);
        assert_eq!(rich.focus_id(), None, "request must not grant");

        let mut off = RichSession::default();
        negotiate(&mut emulator, &mut off, &tx);
        apply_queued_grants(&rx, &mut off);
        assert!(!off.granted.contains(&Feature::InputRichFocus));
        assert!(off.toggle_focus().is_none());
        assert!(off.encode_focused_key("a").is_none());
    }

    #[test]
    fn limit_regions_enforced_for_v1() {
        let mut emulator = Emulator::new_experimental(20, 4, 0);
        let mut rich = RichSession::default();
        rich.apply_grant(CapabilityGrant {
            features: Feature::V1.into_iter().collect(),
            region_limit: 2,
        });
        let (tx, _rx) = mpsc::sync_channel(8);
        for id in 1..=3 {
            let attach = encode_attach_cell_rect(&AttachCellRect {
                id,
                row: 0,
                col: 0,
                rows: 1,
                cols: 1,
                text: "X".into(),
            })
            .unwrap();
            process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        }
        assert_eq!(rich.attachments.len(), 2);
        assert!(rich.attachments.contains_key(&1));
        assert!(rich.attachments.contains_key(&2));
        assert!(!rich.attachments.contains_key(&3));
    }

    #[test]
    fn scrollback_view_keeps_viewport_and_hides_cell_rect() {
        let mut emulator = Emulator::new_experimental(16, 4, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let viewport = encode_attach_viewport(&AttachViewport {
            id: 2,
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
            text: "HUD".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &viewport);

        let live = overlays_for_scroll(visible_overlays(&emulator, &rich), 0);
        assert_eq!(live.cell_rect.len(), 1, "live tail paints cell_rect");
        assert_eq!(live.viewport.len(), 1, "live tail paints viewport");

        let scrolled = overlays_for_scroll(visible_overlays(&emulator, &rich), 3);
        assert!(
            scrolled.cell_rect.is_empty(),
            "grid-anchored overlays hide while panning history"
        );
        assert_eq!(
            scrolled.viewport.len(),
            1,
            "viewport-pinned overlays keep painting while panning history"
        );
    }

    #[test]
    fn two_regions_keep_independent_z_and_damage() {
        let mut emulator = Emulator::new_experimental(16, 4, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let viewport = encode_attach_viewport(&AttachViewport {
            id: 2,
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
            text: "HUD".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &viewport);
        let vis = visible_overlays(&emulator, &rich);
        assert_eq!(vis.cell_rect.len(), 1);
        assert_eq!(vis.viewport.len(), 1);
        assert_eq!(vis.cell_rect[0].text, "STAT");
        assert_eq!(vis.viewport[0].text, "HUD");

        let update = encode_update(&UpdateAttachment {
            id: 1,
            text: "NEXT".into(),
            runs: Vec::new(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &update);
        let vis = visible_overlays(&emulator, &rich);
        assert_eq!(vis.cell_rect[0].text, "NEXT");
        assert_eq!(
            vis.viewport[0].text, "HUD",
            "viewport must not take cell-rect damage"
        );
    }

    #[test]
    fn resize_below_region_hides_rows_past_grid() {
        let mut emulator = Emulator::new_experimental(8, 4, 0);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        let attach = encode_attach_cell_rect(&AttachCellRect {
            id: 1,
            row: 2,
            col: 0,
            rows: 2,
            cols: 4,
            text: "STAT".into(),
        })
        .unwrap();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &attach);
        assert_eq!(visible_overlays(&emulator, &rich).cell_rect.len(), 1);
        emulator.resize(8, 2);
        assert!(
            visible_overlays(&emulator, &rich).cell_rect.is_empty(),
            "region starting at row 2 must not paint on a 2-row grid"
        );
        assert_eq!(
            rich.attachments.len(),
            1,
            "resize hides, it does not detach"
        );
    }

    #[test]
    fn flood_with_live_region_updates_stays_responsive() {
        let mut emulator = Emulator::new_experimental(80, 24, 1000);
        let mut rich = RichSession::default();
        let (tx, rx) = mpsc::sync_channel(8);
        negotiate_v1(&mut emulator, &mut rich, &tx);
        apply_queued_grants(&rx, &mut rich);
        attach_stat(&mut emulator, &mut rich, &tx);
        let update = encode_update(&UpdateAttachment {
            id: 1,
            text: "TICK".into(),
            runs: Vec::new(),
        })
        .unwrap();
        let mut flood = Vec::new();
        for _ in 0..256 {
            flood.extend_from_slice(&update);
        }
        // Grid traffic without enough newlines to scroll the region off.
        flood.extend_from_slice(b"yyyy");
        let start = std::time::Instant::now();
        process_rich_chunk(&mut emulator, &mut rich, &tx, &flood);
        let elapsed = start.elapsed();
        assert!(
            elapsed < test_time_budget(std::time::Duration::from_secs(1)),
            "region-update flood stalled ({elapsed:?})"
        );
        assert_eq!(
            rich.attachments.get(&1).map(|a| a.text.as_str()),
            Some("TICK")
        );
        let text = emulator
            .screen()
            .viewport_range()
            .map(|range| emulator.screen().extract_text(range))
            .unwrap_or_default();
        assert!(text.contains('y'), "classic grid must keep painting");
    }
}
