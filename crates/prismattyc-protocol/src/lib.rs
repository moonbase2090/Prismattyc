//! Dependency-light Prismattyc control protocol types and bounded wire codecs.
//!
//! Streaming APC **body** collection lives here so the classic emulator can
//! hand off complete bodies without depending on a rich document model.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

mod semantics;
pub use semantics::{
    apply_semantic_snapshot, encode_semantic_copy, encode_semantic_snapshot, validate_document_id,
    SemanticCopy, SemanticDocument, SemanticError, SemanticRange, SemanticRole, SemanticSpan,
    MAX_DOCUMENT_ID_BYTES, MAX_SEMANTIC_SPANS, MAX_SEMANTIC_TEXT_CHARS,
};

#[cfg(test)]
mod collection_codec_tests;
mod graphics_apc;
pub use graphics_apc::{
    GraphicsApc, GraphicsApcCollector, GraphicsApcEvent, MAX_GRAPHICS_APC_BYTES,
};

/// Seven-bit Application Program Command introducer (`ESC _`).
pub const APC_INTRODUCER: &[u8] = b"\x1b_";
/// Seven-bit string terminator (`ESC \\`).
pub const STRING_TERMINATOR: &[u8] = b"\x1b\\";
/// Case-sensitive namespace at the start of every Prismattyc APC body.
pub const NAMESPACE: &str = "Prismattyc";

/// True when `ns` is the control-protocol namespace.
#[must_use]
pub fn is_control_namespace(ns: &str) -> bool {
    ns == NAMESPACE
}

/// True when `body` starts with `NAMESPACE;rest`.
#[must_use]
pub fn body_has_prefix(body: &str, rest: &str) -> bool {
    body.starts_with(&format!("{NAMESPACE};{rest}"))
}
/// Maximum accepted APC body size for control messages.
pub const MAX_CONTROL_BODY_BYTES: usize = 4096;
/// Host protocol version implemented by the Phase 0B spike.
pub const HOST_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };
/// Host protocol version for Phase 3 (styled runs + viewport + update).
pub const HOST_V1_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 2 };
/// Rich-surface-v2 protocol version (reserved workspace + keyed tree).
pub const HOST_V2_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 3 };
/// Capability reply key: max concurrent rich regions.
pub const LIMIT_REGIONS: &str = "limit.regions";
/// Capability reply key: max APC body bytes the host will decode.
pub const LIMIT_BODY: &str = "limit.body";
/// Default `limit.regions` advertised at protocol 0.2.
pub const DEFAULT_LIMIT_REGIONS: u32 = 64;
pub const LIMIT_SURFACES: &str = "limit.surfaces";
pub const LIMIT_NODES: &str = "limit.nodes";
pub const LIMIT_TREE_DEPTH: &str = "limit.tree_depth";
pub const LIMIT_COLLECTIONS: &str = "limit.collections";
pub const LIMIT_COLLECTION_ITEMS: &str = "limit.collection_items";
pub const LIMIT_PATCH_OPS: &str = "limit.patch_ops";
pub const LIMIT_QUEUE: &str = "limit.queue";
pub const LIMIT_RETAINED_TEXT: &str = "limit.retained_text";
pub const LIMIT_EVENT_RATE: &str = "limit.event_rate";
pub const LIMIT_DOCK_ROWS: &str = "limit.dock_rows";
pub const DEFAULT_LIMIT_SURFACES: u32 = 1;
pub const DEFAULT_LIMIT_NODES: u32 = 128;
pub const DEFAULT_LIMIT_TREE_DEPTH: u32 = 16;
/// Status records are keyed to tree nodes, so they share the node ceiling.
pub const DEFAULT_LIMIT_STATUS_ITEMS: u32 = DEFAULT_LIMIT_NODES;
/// A sparkline is deliberately a tiny recent-history hint, not a graph surface.
pub const DEFAULT_LIMIT_SPARKLINE_SAMPLES: u32 = 16;
pub const DEFAULT_LIMIT_COLLECTIONS: u32 = 16;
pub const DEFAULT_LIMIT_COLLECTION_ITEMS: u32 = 2048;
pub const DEFAULT_LIMIT_PATCH_OPS: u32 = 128;
pub const DEFAULT_LIMIT_QUEUE: u32 = 256;
pub const DEFAULT_LIMIT_RETAINED_TEXT: u32 = 2_097_152;
pub const DEFAULT_LIMIT_EVENT_RATE: u32 = 240;
pub const DEFAULT_LIMIT_DOCK_ROWS: u32 = 24;
pub const MIN_WORKSPACE_ROWS: u16 = 5;
pub const MIN_TRANSCRIPT_ROWS: u16 = 8;
pub const MIN_WORKSPACE_COLS: u16 = 40;
/// Maximum attachment text length for the spike.
pub const MAX_ATTACHMENT_TEXT_BYTES: usize = 256;
/// Maximum concurrent cell-rect attachments in the spike host.
pub const MAX_CELL_RECT_ATTACHMENTS: usize = 8;

/// A major/minor Prismattyc protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (major, minor) = value.split_once('.')?;
        if major.is_empty()
            || minor.is_empty()
            || major.bytes().any(|b| !b.is_ascii_digit())
            || minor.bytes().any(|b| !b.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// A non-zero correlation id echoed by a capability reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u32);

impl RequestId {
    pub const fn new(value: u32) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(value))
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Capability identifiers frozen by the first cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    Markup,
    Style,
    Animation,
    Canvas,
    HybridAttachCellRect,
    HybridOverlayViewport,
    HybridReserveRows,
    InputRichFocus,
    InputRichKeyboardV1,
    InputRichPointerV1,
    InputRichScrollV1,
    RichCollectionV1,
    RichSemanticTextV1,
    RichStatusV1,
    RichTreeV1,
}

impl Feature {
    pub const ALL: [Self; 15] = [
        Self::Markup,
        Self::Style,
        Self::Animation,
        Self::Canvas,
        Self::HybridAttachCellRect,
        Self::HybridOverlayViewport,
        Self::HybridReserveRows,
        Self::InputRichFocus,
        Self::InputRichKeyboardV1,
        Self::InputRichPointerV1,
        Self::InputRichScrollV1,
        Self::RichCollectionV1,
        Self::RichSemanticTextV1,
        Self::RichStatusV1,
        Self::RichTreeV1,
    ];

    /// Features the Phase 0B experimental host may advertise.
    pub const SPIKE: [Self; 1] = [Self::HybridAttachCellRect];
    /// Features advertised at protocol 0.2 (styled-run payload; no markup/canvas).
    /// `input.rich_focus` is capability + input plumbing, not a content kind.
    pub const V1: [Self; 3] = [
        Self::HybridAttachCellRect,
        Self::HybridOverlayViewport,
        Self::InputRichFocus,
    ];
    /// Implemented 0.3 slice. Later vertical slices append their independently
    /// tested grants; registry membership alone is never an advertisement.
    pub const V2: [Self; 11] = [
        Self::HybridAttachCellRect,
        Self::HybridOverlayViewport,
        Self::HybridReserveRows,
        Self::InputRichFocus,
        Self::InputRichKeyboardV1,
        Self::InputRichPointerV1,
        Self::InputRichScrollV1,
        Self::RichCollectionV1,
        Self::RichSemanticTextV1,
        Self::RichStatusV1,
        Self::RichTreeV1,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Markup => "markup",
            Self::Style => "style",
            Self::Animation => "animation",
            Self::Canvas => "canvas",
            Self::HybridAttachCellRect => "hybrid.attach.cell_rect",
            Self::HybridOverlayViewport => "hybrid.overlay.viewport",
            Self::HybridReserveRows => "hybrid.reserve.rows",
            Self::InputRichFocus => "input.rich_focus",
            Self::InputRichKeyboardV1 => "input.rich_keyboard.v1",
            Self::InputRichPointerV1 => "input.rich_pointer.v1",
            Self::InputRichScrollV1 => "input.rich_scroll.v1",
            Self::RichCollectionV1 => "rich.collection.v1",
            Self::RichSemanticTextV1 => "rich.semantic_text.v1",
            Self::RichStatusV1 => "rich.status.v1",
            Self::RichTreeV1 => "rich.tree.v1",
        }
    }

    /// Protocol version that first introduced this feature.
    pub const fn introduced_in(self) -> ProtocolVersion {
        match self {
            // Spike feature ships with host 0.1; nothing is available at 0.0.
            Self::HybridAttachCellRect => ProtocolVersion::new(0, 1),
            Self::Markup | Self::Style | Self::Animation | Self::Canvas => {
                ProtocolVersion::new(0, 1)
            }
            Self::HybridOverlayViewport | Self::InputRichFocus => ProtocolVersion::new(0, 2),
            Self::HybridReserveRows
            | Self::InputRichKeyboardV1
            | Self::InputRichPointerV1
            | Self::InputRichScrollV1
            | Self::RichCollectionV1
            | Self::RichSemanticTextV1
            | Self::RichStatusV1
            | Self::RichTreeV1 => ProtocolVersion::new(0, 3),
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Feature {
    type Err = UnknownFeature;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|feature| feature.as_str() == value)
            .ok_or_else(|| UnknownFeature(value.to_owned()))
    }
}

/// Returned when a feature identifier is not in this crate's known registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFeature(String);

impl UnknownFeature {
    pub fn identifier(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UnknownFeature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown Prismattyc feature {:?}", self.0)
    }
}

impl Error for UnknownFeature {}

/// Semantic capability request before wire encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityQuery {
    pub request_id: RequestId,
    pub max_version: ProtocolVersion,
}

/// Semantic capability response after bounded wire decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityReply {
    pub request_id: RequestId,
    pub version: ProtocolVersion,
    pub features: BTreeSet<Feature>,
    /// Numeric limits (`limit.regions`, `limit.body`). Empty on 0.1 replies.
    pub limits: BTreeMap<String, u32>,
}

impl CapabilityReply {
    pub fn supports(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }

    /// Build the Phase 0B experimental reply for a compatible query.
    ///
    /// Features are filtered by the selected protocol version so a downgrade
    /// to `0.0` never advertises `0.1` capabilities.
    pub fn for_spike_query(query: CapabilityQuery) -> Option<Self> {
        if query.max_version.major != HOST_PROTOCOL_VERSION.major {
            return None;
        }
        let version = ProtocolVersion::new(
            HOST_PROTOCOL_VERSION.major,
            HOST_PROTOCOL_VERSION.minor.min(query.max_version.minor),
        );
        let features = Feature::SPIKE
            .into_iter()
            .filter(|feature| version >= feature.introduced_in())
            .collect();
        Some(Self {
            request_id: query.request_id,
            version,
            features,
            limits: BTreeMap::new(),
        })
    }

    /// Build the Phase 3 host reply. `max=0.1` is byte-identical to the 0B set.
    pub fn for_v1_query(query: CapabilityQuery) -> Option<Self> {
        if query.max_version.major != HOST_V1_PROTOCOL_VERSION.major {
            return None;
        }
        let version = ProtocolVersion::new(
            HOST_V1_PROTOCOL_VERSION.major,
            HOST_V1_PROTOCOL_VERSION.minor.min(query.max_version.minor),
        );
        if version <= HOST_PROTOCOL_VERSION {
            return Self::for_spike_query(query);
        }
        let features = Feature::V1
            .into_iter()
            .filter(|feature| version >= feature.introduced_in())
            .collect();
        let mut limits = BTreeMap::new();
        limits.insert(LIMIT_REGIONS.to_string(), DEFAULT_LIMIT_REGIONS);
        limits.insert(LIMIT_BODY.to_string(), MAX_CONTROL_BODY_BYTES as u32);
        Some(Self {
            request_id: query.request_id,
            version,
            features,
            limits,
        })
    }

    /// Build the protocol 0.3 reply while preserving exact 0.1/0.2 bytes on
    /// downgrade. Only implemented and tested feature slices are advertised.
    pub fn for_surface_query(query: CapabilityQuery) -> Option<Self> {
        if query.max_version.major != HOST_V2_PROTOCOL_VERSION.major {
            return None;
        }
        let version = ProtocolVersion::new(
            HOST_V2_PROTOCOL_VERSION.major,
            HOST_V2_PROTOCOL_VERSION.minor.min(query.max_version.minor),
        );
        if version <= HOST_V1_PROTOCOL_VERSION {
            return Self::for_v1_query(query);
        }
        let features = Feature::V2
            .into_iter()
            .filter(|feature| version >= feature.introduced_in())
            .collect();
        let mut limits = BTreeMap::new();
        limits.insert(LIMIT_BODY.to_string(), MAX_CONTROL_BODY_BYTES as u32);
        limits.insert(
            LIMIT_COLLECTION_ITEMS.to_string(),
            DEFAULT_LIMIT_COLLECTION_ITEMS,
        );
        limits.insert(LIMIT_COLLECTIONS.to_string(), DEFAULT_LIMIT_COLLECTIONS);
        limits.insert(LIMIT_DOCK_ROWS.to_string(), DEFAULT_LIMIT_DOCK_ROWS);
        limits.insert(LIMIT_EVENT_RATE.to_string(), DEFAULT_LIMIT_EVENT_RATE);
        limits.insert(LIMIT_NODES.to_string(), DEFAULT_LIMIT_NODES);
        limits.insert(LIMIT_PATCH_OPS.to_string(), DEFAULT_LIMIT_PATCH_OPS);
        limits.insert(LIMIT_QUEUE.to_string(), DEFAULT_LIMIT_QUEUE);
        limits.insert(LIMIT_REGIONS.to_string(), DEFAULT_LIMIT_REGIONS);
        limits.insert(LIMIT_RETAINED_TEXT.to_string(), DEFAULT_LIMIT_RETAINED_TEXT);
        limits.insert(LIMIT_SURFACES.to_string(), DEFAULT_LIMIT_SURFACES);
        limits.insert(LIMIT_TREE_DEPTH.to_string(), DEFAULT_LIMIT_TREE_DEPTH);
        Some(Self {
            request_id: query.request_id,
            version,
            features,
            limits,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeNodeKind {
    Row,
    Column,
    Stack,
    Text,
    Border,
    Spacer,
}

impl TreeNodeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Column => "column",
            Self::Stack => "stack",
            Self::Text => "text",
            Self::Border => "border",
            Self::Spacer => "spacer",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "row" => Some(Self::Row),
            "column" => Some(Self::Column),
            "stack" => Some(Self::Stack),
            "text" => Some(Self::Text),
            "border" => Some(Self::Border),
            "spacer" => Some(Self::Spacer),
            _ => None,
        }
    }
}

/// One keyed node in a full protocol 0.3 workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub id: u32,
    /// Zero denotes the single root.
    pub parent: u32,
    pub kind: TreeNodeKind,
    /// Main-axis sizing hints interpreted by the parent row/column.
    pub min: u16,
    pub preferred: u16,
    pub fill: u16,
    /// Width visibility range. `show_max_cols=0` means unbounded.
    pub show_min_cols: u16,
    pub show_max_cols: u16,
    /// Opaque application-owned action binding. Zero marks an inert node.
    /// The encoder omits this field for zero so pre-tree bytes stay
    /// identical; decoders accept both the frozen 9-field and new 10-field
    /// record shapes.
    pub action_id: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceRows {
    pub min: u16,
    pub preferred: u16,
    pub max: u16,
}

/// Atomic, bounded shared workspace tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub surface_generation: u64,
    pub scene_rev: u64,
    pub rows: WorkspaceRows,
    pub nodes: Vec<TreeNode>,
}

/// Semantic color intent for a status primitive. Hosts resolve this through
/// their active theme; applications never send palette indexes or RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Info,
    Success,
    Warning,
    Danger,
}

impl StatusTone {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }

    /// Portable terminal color token for this semantic tone. `None` means
    /// the active theme's default foreground; hosts resolve the remaining
    /// values through that theme's ANSI palette.
    pub const fn ansi_index(self) -> Option<u8> {
        match self {
            Self::Neutral => None,
            Self::Info => Some(4),
            Self::Success => Some(2),
            Self::Warning => Some(3),
            Self::Danger => Some(1),
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "neutral" => Some(Self::Neutral),
            "info" => Some(Self::Info),
            "success" => Some(Self::Success),
            "warning" => Some(Self::Warning),
            "danger" => Some(Self::Danger),
            _ => None,
        }
    }
}

/// One static visual treatment layered over an authoritative text node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusVisual {
    Badge,
    /// `total=None` is a static indeterminate meter. Prismattyc never animates it.
    Meter {
        current: u64,
        total: Option<u64>,
    },
    /// Application-reported, unitless values. The host only normalizes the
    /// supplied bounded set to choose block heights.
    Sparkline {
        samples: Vec<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItem {
    pub node_id: u32,
    pub tone: StatusTone,
    pub visual: StatusVisual,
}

/// A complete bounded status layer for one accepted workspace scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub surface_generation: u64,
    pub scene_rev: u64,
    pub rev: u64,
    pub items: Vec<StatusItem>,
}

/// One ordered collection record. `replaceable` marks a status slot that
/// may coalesce; append-only diagnostics must keep this false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionItem {
    pub id: u32,
    pub replaceable: bool,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionPatchKind {
    Append,
    Replace,
}

impl CollectionPatchKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionRejectReason {
    Gap,
    Stale,
    Conflict,
    Backpressure,
}

impl CollectionRejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gap => "gap",
            Self::Stale => "stale",
            Self::Conflict => "conflict",
            Self::Backpressure => "backpressure",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "gap" => Some(Self::Gap),
            "stale" => Some(Self::Stale),
            "conflict" => Some(Self::Conflict),
            "backpressure" => Some(Self::Backpressure),
            _ => None,
        }
    }
}

/// Full shared collection content for one `(generation, collection_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSnapshot {
    pub surface_generation: u64,
    pub collection_id: String,
    pub rev: u64,
    pub items: Vec<CollectionItem>,
}

/// Ordered append or explicit replaceable update. Next must be base + 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionPatch {
    pub surface_generation: u64,
    pub collection_id: String,
    pub base: u64,
    pub next: u64,
    pub kind: CollectionPatchKind,
    pub items: Vec<CollectionItem>,
}

/// One styled text run (ADR-0013). Colors are optional 0–255 palette indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// One bounded cell-rect attachment for the Phase 0B spike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachCellRect {
    pub id: u32,
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    pub text: String,
}

/// Viewport-local overlay (second frozen kind, protocol 0.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachViewport {
    pub id: u32,
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    pub text: String,
    pub runs: Vec<StyledRun>,
}

/// Cell-rect attach that carries styled runs instead of plain `text=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachStyled {
    pub kind: RegionKind,
    pub id: u32,
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    pub runs: Vec<StyledRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    CellRect,
    Viewport,
}

/// Update an existing attachment's content without changing geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAttachment {
    pub id: u32,
    pub text: String,
    pub runs: Vec<StyledRun>,
}

/// Host decision on a region-scoped focus request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusStatus {
    Granted,
    Denied,
    Revoked,
}

impl FocusStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "granted" => Some(Self::Granted),
            "denied" => Some(Self::Denied),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Maximum unescaped key-token bytes in a `focus;k` frame.
pub const MAX_FOCUS_KEY_BYTES: usize = 32;

/// A host-minted, generation-scoped viewer identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewerId([u8; 16]);

impl ViewerId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }
}

impl fmt::Display for ViewerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Structured key modifier bits. Unknown bits are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputModifiers(u8);

impl InputModifiers {
    pub const SHIFT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;
    const ALL: u8 = Self::SHIFT | Self::CONTROL | Self::ALT | Self::SUPER;

    pub const fn new(bits: u8) -> Option<Self> {
        if bits & !Self::ALL == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn shift(self) -> bool {
        self.0 & Self::SHIFT != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Press,
    Move,
    Release,
    Activate,
}

impl PointerPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Move => "move",
            Self::Release => "release",
            Self::Activate => "activate",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "press" => Some(Self::Press),
            "move" => Some(Self::Move),
            "release" => Some(Self::Release),
            "activate" => Some(Self::Activate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEventKind {
    Focus {
        focused: bool,
    },
    Key {
        key: String,
        modifiers: InputModifiers,
    },
    Pointer {
        phase: PointerPhase,
        row: u16,
        col: u16,
    },
    Scroll {
        delta: i16,
    },
    Viewport {
        first: u32,
        count: u16,
    },
}

/// One host-validated structured event delivered to the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    pub viewer_id: ViewerId,
    pub request_seq: u64,
    pub surface_generation: u64,
    pub scene_rev: u64,
    pub node_id: u32,
    pub action_id: u32,
    /// Empty for non-collection events.
    pub collection_id: Option<String>,
    /// Zero when no collection participates.
    pub collection_rev: u64,
    pub kind: InputEventKind,
}

/// Decoded Prismattyc control message (capability + spike attachment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    CapabilityQuery(CapabilityQuery),
    CapabilityReply(CapabilityReply),
    AttachCellRect(AttachCellRect),
    AttachViewport(AttachViewport),
    AttachStyled(AttachStyled),
    Update(UpdateAttachment),
    Detach {
        id: u32,
    },
    /// Client asks to become eligible for a later host gesture grant.
    FocusQuery {
        id: u32,
    },
    /// Host grant / deny / revoke after a gesture or lifecycle event.
    FocusReply {
        id: u32,
        status: FocusStatus,
    },
    /// Host → client key while a region holds rich focus.
    FocusKey {
        id: u32,
        key: String,
    },
    /// Full protocol 0.3 reserved-workspace tree snapshot.
    WorkspaceSnapshot(WorkspaceSnapshot),
    /// Explicitly remove the current workspace for this generation.
    WorkspaceDrop {
        surface_generation: u64,
    },
    CollectionSnapshot(CollectionSnapshot),
    CollectionPatch(CollectionPatch),
    CollectionDrop {
        surface_generation: u64,
        collection_id: String,
    },
    CollectionAck {
        surface_generation: u64,
        collection_id: String,
        rev: u64,
    },
    CollectionReject {
        surface_generation: u64,
        collection_id: String,
        reason: CollectionRejectReason,
    },
    CollectionResnapshot {
        surface_generation: u64,
        collection_id: String,
    },
    InputEvent(InputEvent),
    SemanticSnapshot(crate::semantics::SemanticDocument),
    SemanticCopy(crate::semantics::SemanticCopy),
    StatusSnapshot(StatusSnapshot),
    StatusDrop {
        surface_generation: u64,
    },
}

/// Why a control body was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    NonPrintableAscii,
    Oversized,
    BadNamespace,
    UnknownFamily,
    MissingField(&'static str),
    DuplicateField(&'static str),
    InvalidField(&'static str),
    ZeroId,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "empty control body"),
            Self::NonPrintableAscii => write!(formatter, "non-printable ASCII in control body"),
            Self::Oversized => write!(
                formatter,
                "control body exceeds {MAX_CONTROL_BODY_BYTES} bytes"
            ),
            Self::BadNamespace => write!(formatter, "missing or wrong Prismattyc namespace"),
            Self::UnknownFamily => write!(formatter, "unknown Prismattyc message family"),
            Self::MissingField(field) => write!(formatter, "missing required field {field}"),
            Self::DuplicateField(field) => write!(formatter, "duplicate required field {field}"),
            Self::InvalidField(field) => write!(formatter, "invalid field {field}"),
            Self::ZeroId => write!(formatter, "id must be non-zero"),
        }
    }
}

impl Error for DecodeError {}

/// Decode a complete APC body (without introducer/terminator).
pub fn decode_body(body: &str) -> Result<ControlMessage, DecodeError> {
    if body.is_empty() {
        return Err(DecodeError::Empty);
    }
    if body.len() > MAX_CONTROL_BODY_BYTES {
        return Err(DecodeError::Oversized);
    }
    if !body.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(DecodeError::NonPrintableAscii);
    }

    let mut parts = body.split(';');
    let ns = parts.next().ok_or(DecodeError::BadNamespace)?;
    if !is_control_namespace(ns) {
        return Err(DecodeError::BadNamespace);
    }
    let family = parts.next().ok_or(DecodeError::UnknownFamily)?;
    match family {
        "cap" => decode_capability(parts),
        "attach" => decode_attach(parts),
        "update" => decode_update(parts),
        "detach" => decode_detach(parts),
        "focus" => decode_focus(parts),
        "workspace" => decode_workspace(parts),
        "collection" => decode_collection(parts),
        "status" => decode_status(parts),
        "input" => decode_input(parts),
        "semantics" => crate::semantics::decode_semantics(parts),
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_capability<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    match kind {
        "q" => {
            let id = require_u32(&fields, "id")?;
            let max = require_version(&fields, "max")?;
            let request_id = RequestId::new(id).ok_or(DecodeError::ZeroId)?;
            Ok(ControlMessage::CapabilityQuery(CapabilityQuery {
                request_id,
                max_version: max,
            }))
        }
        "r" => {
            let id = require_u32(&fields, "id")?;
            let version = require_version(&fields, "v")?;
            let request_id = RequestId::new(id).ok_or(DecodeError::ZeroId)?;
            let features = match fields.get("features").copied() {
                None => BTreeSet::new(),
                Some("") => {
                    return Err(DecodeError::InvalidField("features"));
                }
                Some(raw) => {
                    let mut set = BTreeSet::new();
                    for item in raw.split(',') {
                        if item.is_empty() {
                            return Err(DecodeError::InvalidField("features"));
                        }
                        if let Ok(feature) = item.parse::<Feature>() {
                            set.insert(feature);
                        }
                    }
                    set
                }
            };
            let mut limits = BTreeMap::new();
            for (key, value) in &fields {
                if let Some(name) = key.strip_prefix("limit.") {
                    if name.is_empty() {
                        return Err(DecodeError::InvalidField("limit"));
                    }
                    let parsed = value
                        .parse::<u32>()
                        .map_err(|_| DecodeError::InvalidField("limit"))?;
                    limits.insert((*key).to_string(), parsed);
                }
            }
            Ok(ControlMessage::CapabilityReply(CapabilityReply {
                request_id,
                version,
                features,
                limits,
            }))
        }
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_attach<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    if kind != "cell_rect" && kind != "viewport" {
        return Err(DecodeError::UnknownFamily);
    }
    let (id, row, col, rows, cols, text, runs) = decode_region_fields(parts)?;
    if kind == "viewport" {
        Ok(ControlMessage::AttachViewport(AttachViewport {
            id,
            row,
            col,
            rows,
            cols,
            text,
            runs,
        }))
    } else if !runs.is_empty() {
        Ok(ControlMessage::AttachStyled(AttachStyled {
            kind: RegionKind::CellRect,
            id,
            row,
            col,
            rows,
            cols,
            runs,
        }))
    } else {
        Ok(ControlMessage::AttachCellRect(AttachCellRect {
            id,
            row,
            col,
            rows,
            cols,
            text,
        }))
    }
}

fn decode_update<'a>(parts: impl Iterator<Item = &'a str>) -> Result<ControlMessage, DecodeError> {
    let (id, _row, _col, rows, cols, text, runs) = decode_content_fields(parts, false)?;
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    if !runs.is_empty() {
        validate_runs(&runs, rows, cols)?;
    } else {
        validate_attach_text(&text, rows.max(1), cols.max(1))?;
    }
    Ok(ControlMessage::Update(UpdateAttachment { id, text, runs }))
}

type RegionFields = (u32, u16, u16, u16, u16, String, Vec<StyledRun>);

fn decode_region_fields<'a>(
    parts: impl Iterator<Item = &'a str>,
) -> Result<RegionFields, DecodeError> {
    let (id, row, col, rows, cols, text, runs) = decode_content_fields(parts, true)?;
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    if rows == 0 || cols == 0 {
        return Err(DecodeError::InvalidField("rows/cols"));
    }
    if !runs.is_empty() {
        validate_runs(&runs, rows, cols)?;
    } else {
        validate_attach_text(&text, rows, cols)?;
    }
    Ok((id, row, col, rows, cols, text, runs))
}

fn decode_content_fields<'a>(
    parts: impl Iterator<Item = &'a str>,
    require_geometry: bool,
) -> Result<RegionFields, DecodeError> {
    // `text=` / `runs=` must be last so values are not split on `;`.
    let mut fields = BTreeMap::new();
    let mut text: Option<String> = None;
    let mut runs_raw: Option<String> = None;
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if let Some(value) = part.strip_prefix("text=") {
            if text.is_some() || runs_raw.is_some() || fields.contains_key("text") {
                return Err(DecodeError::DuplicateField("text"));
            }
            text = Some(value.to_string());
            continue;
        }
        if let Some(value) = part.strip_prefix("runs=") {
            if text.is_some() || runs_raw.is_some() || fields.contains_key("runs") {
                return Err(DecodeError::DuplicateField("runs"));
            }
            runs_raw = Some(value.to_string());
            continue;
        }
        if text.is_some() || runs_raw.is_some() {
            return Err(DecodeError::InvalidField("text"));
        }
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.is_empty() {
            return Err(DecodeError::InvalidField("key"));
        }
        if key == "text" {
            return Err(DecodeError::InvalidField("text"));
        }
        if key == "runs" {
            return Err(DecodeError::InvalidField("runs"));
        }
        if fields.insert(key, value).is_some() {
            return Err(duplicate_field(key));
        }
    }
    let id = require_u32(&fields, "id")?;
    let (row, col, rows, cols) = if require_geometry {
        (
            require_u16(&fields, "row")?,
            require_u16(&fields, "col")?,
            require_u16(&fields, "rows")?,
            require_u16(&fields, "cols")?,
        )
    } else {
        (
            fields
                .get("row")
                .map(|_| require_u16(&fields, "row"))
                .transpose()?
                .unwrap_or(0),
            fields
                .get("col")
                .map(|_| require_u16(&fields, "col"))
                .transpose()?
                .unwrap_or(0),
            fields
                .get("rows")
                .map(|_| require_u16(&fields, "rows"))
                .transpose()?
                .unwrap_or(1),
            fields
                .get("cols")
                .map(|_| require_u16(&fields, "cols"))
                .transpose()?
                .unwrap_or(u16::MAX),
        )
    };
    let runs = match runs_raw {
        Some(raw) => decode_runs(&raw)?,
        None => Vec::new(),
    };
    let text = match text {
        Some(text) => text,
        None if !runs.is_empty() => runs.iter().map(|run| run.text.as_str()).collect(),
        None if require_geometry => return Err(DecodeError::MissingField("text")),
        None => return Err(DecodeError::MissingField("text")),
    };
    Ok((id, row, col, rows, cols, text, runs))
}

fn duplicate_field(key: &str) -> DecodeError {
    match key {
        "id" => DecodeError::DuplicateField("id"),
        "max" => DecodeError::DuplicateField("max"),
        "v" => DecodeError::DuplicateField("v"),
        "features" => DecodeError::DuplicateField("features"),
        "row" => DecodeError::DuplicateField("row"),
        "col" => DecodeError::DuplicateField("col"),
        "rows" => DecodeError::DuplicateField("rows"),
        "cols" => DecodeError::DuplicateField("cols"),
        "text" => DecodeError::DuplicateField("text"),
        "runs" => DecodeError::DuplicateField("runs"),
        "limit.regions" => DecodeError::DuplicateField("limit.regions"),
        "limit.body" => DecodeError::DuplicateField("limit.body"),
        _ => DecodeError::InvalidField("duplicate"),
    }
}

/// Attachment text is printable ASCII without field delimiters (`;` / `=`).
/// Length is capped by `min(MAX_ATTACHMENT_TEXT_BYTES, rows * cols)`.
fn validate_attach_text(text: &str, rows: u16, cols: u16) -> Result<(), DecodeError> {
    let cell_cap = usize::from(rows).saturating_mul(usize::from(cols));
    let limit = MAX_ATTACHMENT_TEXT_BYTES.min(cell_cap);
    if text.len() > limit {
        return Err(DecodeError::InvalidField("text"));
    }
    // Reject field delimiters symmetrically: encode and decode share this rule
    // so printable ASCII that would split the wire grammar cannot round-trip
    // into a truncated value.
    if !text
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b) && b != b';' && b != b'=')
    {
        if text.bytes().any(|b| !(0x20..=0x7e).contains(&b)) {
            return Err(DecodeError::NonPrintableAscii);
        }
        return Err(DecodeError::InvalidField("text"));
    }
    Ok(())
}

/// Decoded run text is printable ASCII. Delimiters (`; = / + ,` and `%`)
/// are allowed here; they are percent-escaped on the wire.
fn validate_run_text(text: &str) -> Result<(), DecodeError> {
    if !text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    Ok(())
}

fn validate_runs(runs: &[StyledRun], rows: u16, cols: u16) -> Result<(), DecodeError> {
    if runs.is_empty() {
        return Err(DecodeError::MissingField("runs"));
    }
    let joined: String = runs.iter().map(|run| run.text.as_str()).collect();
    let cell_cap = usize::from(rows).saturating_mul(usize::from(cols));
    let limit = MAX_ATTACHMENT_TEXT_BYTES.min(cell_cap);
    if joined.len() > limit {
        return Err(DecodeError::InvalidField("text"));
    }
    if !joined.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    for run in runs {
        validate_run_text(&run.text)?;
        if run.text.is_empty() {
            return Err(DecodeError::InvalidField("runs"));
        }
    }
    Ok(())
}

const RUN_ESCAPE: &[(u8, &str)] = &[
    (b'%', "%25"),
    (b';', "%3B"),
    (b'=', "%3D"),
    (b'/', "%2F"),
    (b'+', "%2B"),
    (b',', "%2C"),
];

fn escape_run_text(text: &str) -> Result<String, DecodeError> {
    validate_run_text(text)?;
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match RUN_ESCAPE.iter().find(|(ch, _)| *ch == b) {
            Some((_, esc)) => out.push_str(esc),
            None => out.push(b as char),
        }
    }
    Ok(out)
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Decode percent-escaped run text. Accepts `%XX` only for
/// `% ; = / + ,`. Rejects bare delimiters, truncated/`%zz`/`%00` sequences.
fn unescape_run_text(wire: &str) -> Result<String, DecodeError> {
    let bytes = wire.as_bytes();
    if bytes.iter().any(|b| !(0x20..=0x7e).contains(b)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    let mut out = String::with_capacity(wire.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b';' | b'=' | b'/' | b'+' | b',' => {
                return Err(DecodeError::InvalidField("runs"));
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(DecodeError::InvalidField("runs"));
                }
                let Some(val) = from_hex(bytes[i + 1])
                    .and_then(|hi| from_hex(bytes[i + 2]).map(|lo| (hi << 4) | lo))
                else {
                    return Err(DecodeError::InvalidField("runs"));
                };
                if !RUN_ESCAPE.iter().any(|(ch, _)| *ch == val) {
                    return Err(DecodeError::InvalidField("runs"));
                }
                out.push(val as char);
                i += 3;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    validate_run_text(&out)?;
    Ok(out)
}

fn validate_tree_text(text: &str) -> Result<(), DecodeError> {
    if text
        .chars()
        .all(|ch| ch == '\n' || (!ch.is_control() && ch != '\u{7f}'))
    {
        Ok(())
    } else {
        Err(DecodeError::NonPrintableAscii)
    }
}

fn escape_tree_text(text: &str) -> Result<String, DecodeError> {
    validate_tree_text(text)?;
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'\n' => out.push_str("%0A"),
            b'%' => out.push_str("%25"),
            b';' => out.push_str("%3B"),
            b'=' => out.push_str("%3D"),
            b'/' => out.push_str("%2F"),
            b'+' => out.push_str("%2B"),
            b',' => out.push_str("%2C"),
            0x20..=0x7e => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(out)
}

fn unescape_tree_text(wire: &str) -> Result<String, DecodeError> {
    let bytes = wire.as_bytes();
    if bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    let mut raw = Vec::with_capacity(wire.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b';' | b'=' | b'/' | b'+' | b',' => {
                return Err(DecodeError::InvalidField("nodes"));
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(DecodeError::InvalidField("nodes"));
                }
                let value = from_hex(bytes[index + 1])
                    .and_then(|high| from_hex(bytes[index + 2]).map(|low| (high << 4) | low))
                    .ok_or(DecodeError::InvalidField("nodes"))?;
                raw.push(value);
                index += 3;
            }
            byte => {
                raw.push(byte);
                index += 1;
            }
        }
    }
    let out = String::from_utf8(raw).map_err(|_| DecodeError::InvalidField("nodes"))?;
    validate_tree_text(&out)?;
    Ok(out)
}

/// `t:Hello+fg:7+b:1/t:World+i:1`
fn decode_runs(raw: &str) -> Result<Vec<StyledRun>, DecodeError> {
    if raw.is_empty() {
        return Err(DecodeError::InvalidField("runs"));
    }
    let mut runs = Vec::new();
    for chunk in raw.split('/') {
        if chunk.is_empty() {
            return Err(DecodeError::InvalidField("runs"));
        }
        let mut run = StyledRun {
            text: String::new(),
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        };
        let mut seen_text = false;
        for field in chunk.split('+') {
            let Some((key, value)) = field.split_once(':') else {
                return Err(DecodeError::InvalidField("runs"));
            };
            match key {
                "t" => {
                    if seen_text {
                        return Err(DecodeError::DuplicateField("runs"));
                    }
                    seen_text = true;
                    run.text = unescape_run_text(value)?;
                }
                "fg" => run.fg = Some(parse_u8(value)?),
                "bg" => run.bg = Some(parse_u8(value)?),
                "b" => run.bold = parse_flag(value)?,
                "i" => run.italic = parse_flag(value)?,
                "u" => run.underline = parse_flag(value)?,
                "v" => run.inverse = parse_flag(value)?,
                _ => {}
            }
        }
        if !seen_text || run.text.is_empty() {
            return Err(DecodeError::InvalidField("runs"));
        }
        runs.push(run);
    }
    Ok(runs)
}

fn encode_runs(runs: &[StyledRun]) -> Result<String, DecodeError> {
    let mut out = String::new();
    for (i, run) in runs.iter().enumerate() {
        validate_run_text(&run.text)?;
        if i > 0 {
            out.push('/');
        }
        out.push_str("t:");
        out.push_str(&escape_run_text(&run.text)?);
        if let Some(fg) = run.fg {
            out.push_str(&format!("+fg:{fg}"));
        }
        if let Some(bg) = run.bg {
            out.push_str(&format!("+bg:{bg}"));
        }
        if run.bold {
            out.push_str("+b:1");
        }
        if run.italic {
            out.push_str("+i:1");
        }
        if run.underline {
            out.push_str("+u:1");
        }
        if run.inverse {
            out.push_str("+v:1");
        }
    }
    Ok(out)
}

fn parse_u8(raw: &str) -> Result<u8, DecodeError> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecodeError::InvalidField("runs"));
    }
    raw.parse().map_err(|_| DecodeError::InvalidField("runs"))
}

fn parse_flag(raw: &str) -> Result<bool, DecodeError> {
    match raw {
        "1" => Ok(true),
        "0" => Ok(false),
        _ => Err(DecodeError::InvalidField("runs")),
    }
}

fn decode_detach<'a>(parts: impl Iterator<Item = &'a str>) -> Result<ControlMessage, DecodeError> {
    let fields = parse_fields(parts)?;
    let id = require_u32(&fields, "id")?;
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    Ok(ControlMessage::Detach { id })
}

fn decode_focus<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    let id = require_u32(&fields, "id")?;
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    match kind {
        "q" => Ok(ControlMessage::FocusQuery { id }),
        "r" => {
            let raw = fields
                .get("status")
                .ok_or(DecodeError::MissingField("status"))?;
            let status = FocusStatus::parse(raw).ok_or(DecodeError::InvalidField("status"))?;
            Ok(ControlMessage::FocusReply { id, status })
        }
        "k" => {
            let raw = fields.get("k").ok_or(DecodeError::MissingField("k"))?;
            let key = unescape_run_text(raw).map_err(|_| DecodeError::InvalidField("k"))?;
            if key.is_empty() || key.len() > MAX_FOCUS_KEY_BYTES {
                return Err(DecodeError::InvalidField("k"));
            }
            Ok(ControlMessage::FocusKey { id, key })
        }
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_input<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    if parts.next() != Some("event") {
        return Err(DecodeError::UnknownFamily);
    }
    let fields = parse_fields(parts)?;
    let viewer_id = ViewerId::parse(
        fields
            .get("viewer")
            .ok_or(DecodeError::MissingField("viewer"))?,
    )
    .ok_or(DecodeError::InvalidField("viewer"))?;
    let modifiers = |fields: &BTreeMap<&str, &str>| {
        let raw = require_u32(fields, "mods")?;
        let bits = u8::try_from(raw).map_err(|_| DecodeError::InvalidField("mods"))?;
        InputModifiers::new(bits).ok_or(DecodeError::InvalidField("mods"))
    };
    let kind = match fields
        .get("kind")
        .copied()
        .ok_or(DecodeError::MissingField("kind"))?
    {
        "focus" => InputEventKind::Focus {
            focused: match fields
                .get("focused")
                .copied()
                .ok_or(DecodeError::MissingField("focused"))?
            {
                "0" => false,
                "1" => true,
                _ => return Err(DecodeError::InvalidField("focused")),
            },
        },
        "key" => {
            let raw = fields.get("key").ok_or(DecodeError::MissingField("key"))?;
            let key = unescape_run_text(raw).map_err(|_| DecodeError::InvalidField("key"))?;
            if key.is_empty() || key.len() > MAX_FOCUS_KEY_BYTES {
                return Err(DecodeError::InvalidField("key"));
            }
            InputEventKind::Key {
                key,
                modifiers: modifiers(&fields)?,
            }
        }
        "pointer" => InputEventKind::Pointer {
            phase: PointerPhase::parse(
                fields
                    .get("phase")
                    .ok_or(DecodeError::MissingField("phase"))?,
            )
            .ok_or(DecodeError::InvalidField("phase"))?,
            row: require_u16(&fields, "row")?,
            col: require_u16(&fields, "col")?,
        },
        "scroll" => {
            let raw = fields
                .get("delta")
                .ok_or(DecodeError::MissingField("delta"))?;
            let delta = raw
                .parse::<i16>()
                .map_err(|_| DecodeError::InvalidField("delta"))?;
            if delta == 0 {
                return Err(DecodeError::InvalidField("delta"));
            }
            InputEventKind::Scroll { delta }
        }
        "viewport" => {
            let count = require_u16(&fields, "count")?;
            if count == 0 {
                return Err(DecodeError::InvalidField("count"));
            }
            InputEventKind::Viewport {
                first: require_u32(&fields, "first")?,
                count,
            }
        }
        _ => return Err(DecodeError::InvalidField("kind")),
    };
    let collection_id = fields.get("collection").map(|value| value.to_string());
    if let Some(id) = &collection_id {
        validate_collection_id(id)?;
    }
    let event = InputEvent {
        viewer_id,
        request_seq: require_u64(&fields, "seq")?,
        surface_generation: require_u64(&fields, "generation")?,
        scene_rev: require_u64(&fields, "scene")?,
        node_id: require_u32(&fields, "node")?,
        action_id: require_u32(&fields, "action")?,
        collection_id,
        collection_rev: require_u64(&fields, "collection_rev")?,
        kind,
    };
    validate_input_event(&event)?;
    Ok(ControlMessage::InputEvent(event))
}

fn decode_workspace<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    let surface_generation = require_u64(&fields, "generation")?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    match kind {
        "snapshot" => {
            let scene_rev = require_u64(&fields, "rev")?;
            let raw_nodes = fields
                .get("nodes")
                .ok_or(DecodeError::MissingField("nodes"))?;
            let snapshot = WorkspaceSnapshot {
                surface_generation,
                scene_rev,
                rows: WorkspaceRows {
                    min: require_u16(&fields, "min")?,
                    preferred: require_u16(&fields, "preferred")?,
                    max: require_u16(&fields, "max")?,
                },
                nodes: decode_tree_nodes(raw_nodes)?,
            };
            validate_workspace_snapshot(&snapshot)?;
            Ok(ControlMessage::WorkspaceSnapshot(snapshot))
        }
        "drop" => Ok(ControlMessage::WorkspaceDrop { surface_generation }),
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_status<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    let surface_generation = require_u64(&fields, "generation")?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    match kind {
        "snapshot" => {
            let snapshot = StatusSnapshot {
                surface_generation,
                scene_rev: require_u64(&fields, "scene")?,
                rev: require_u64(&fields, "rev")?,
                items: decode_status_items(
                    fields
                        .get("items")
                        .ok_or(DecodeError::MissingField("items"))?,
                )?,
            };
            validate_status_snapshot(&snapshot)?;
            Ok(ControlMessage::StatusSnapshot(snapshot))
        }
        "drop" => Ok(ControlMessage::StatusDrop { surface_generation }),
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_status_items(raw: &str) -> Result<Vec<StatusItem>, DecodeError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for record in raw.split('/') {
        if items.len() >= DEFAULT_LIMIT_STATUS_ITEMS as usize {
            return Err(DecodeError::InvalidField("items"));
        }
        let fields: Vec<_> = record.split(',').collect();
        let node_id = fields
            .first()
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or(DecodeError::InvalidField("items"))?
            .parse::<u32>()
            .map_err(|_| DecodeError::InvalidField("items"))?;
        let tone = fields
            .get(2)
            .and_then(|value| StatusTone::parse(value))
            .ok_or(DecodeError::InvalidField("items"))?;
        let visual = match fields.get(1).copied() {
            Some("badge") if fields.len() == 3 => StatusVisual::Badge,
            Some("meter") if fields.len() == 5 => {
                let parse_value = |value: &str| {
                    value
                        .parse::<u64>()
                        .map_err(|_| DecodeError::InvalidField("items"))
                };
                if fields[3] == "-" && fields[4] == "-" {
                    StatusVisual::Meter {
                        current: 0,
                        total: None,
                    }
                } else {
                    StatusVisual::Meter {
                        current: parse_value(fields[3])?,
                        total: Some(parse_value(fields[4])?),
                    }
                }
            }
            Some("sparkline") if fields.len() == 4 => {
                let samples = fields[3]
                    .split('.')
                    .map(|value| {
                        value
                            .parse::<u16>()
                            .map_err(|_| DecodeError::InvalidField("items"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                StatusVisual::Sparkline { samples }
            }
            _ => return Err(DecodeError::InvalidField("items")),
        };
        items.push(StatusItem {
            node_id,
            tone,
            visual,
        });
    }
    Ok(items)
}

pub fn validate_status_snapshot(snapshot: &StatusSnapshot) -> Result<(), DecodeError> {
    if snapshot.surface_generation == 0 || snapshot.scene_rev == 0 || snapshot.rev == 0 {
        return Err(DecodeError::InvalidField("revision"));
    }
    if snapshot.items.len() > DEFAULT_LIMIT_STATUS_ITEMS as usize {
        return Err(DecodeError::InvalidField("items"));
    }
    let mut ids = BTreeSet::new();
    for item in &snapshot.items {
        if item.node_id == 0 || !ids.insert(item.node_id) {
            return Err(DecodeError::InvalidField("items"));
        }
        match &item.visual {
            StatusVisual::Badge => {}
            StatusVisual::Meter {
                current,
                total: Some(total),
            } if *total > 0 && current <= total => {}
            StatusVisual::Meter {
                current,
                total: None,
            } if *current == 0 => {}
            StatusVisual::Sparkline { samples }
                if !samples.is_empty()
                    && samples.len() <= DEFAULT_LIMIT_SPARKLINE_SAMPLES as usize => {}
            _ => return Err(DecodeError::InvalidField("items")),
        }
    }
    Ok(())
}

fn decode_tree_nodes(raw: &str) -> Result<Vec<TreeNode>, DecodeError> {
    if raw.is_empty() {
        return Err(DecodeError::InvalidField("nodes"));
    }
    let mut nodes = Vec::new();
    for record in raw.split('/') {
        if nodes.len() >= DEFAULT_LIMIT_NODES as usize {
            return Err(DecodeError::InvalidField("nodes"));
        }
        let fields: Vec<_> = record.splitn(10, ',').collect();
        if fields.len() != 9 && fields.len() != 10 {
            return Err(DecodeError::InvalidField("nodes"));
        }
        let parse_u32 = |value: &str| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(DecodeError::InvalidField("nodes"));
            }
            value
                .parse::<u32>()
                .map_err(|_| DecodeError::InvalidField("nodes"))
        };
        let parse_u16 = |value: &str| {
            parse_u32(value).and_then(|value| {
                u16::try_from(value).map_err(|_| DecodeError::InvalidField("nodes"))
            })
        };
        let (action_id, text) = if fields.len() == 10 {
            (parse_u32(fields[8])?, fields[9])
        } else {
            (0, fields[8])
        };
        nodes.push(TreeNode {
            id: parse_u32(fields[0])?,
            parent: parse_u32(fields[1])?,
            kind: TreeNodeKind::parse(fields[2]).ok_or(DecodeError::InvalidField("nodes"))?,
            min: parse_u16(fields[3])?,
            preferred: parse_u16(fields[4])?,
            fill: parse_u16(fields[5])?,
            show_min_cols: parse_u16(fields[6])?,
            show_max_cols: parse_u16(fields[7])?,
            action_id,
            text: unescape_tree_text(text).map_err(|_| DecodeError::InvalidField("nodes"))?,
        });
    }
    Ok(nodes)
}

/// Validate the bounded keyed-tree and reserved-row request independently of
/// any pane geometry. Hosts additionally resolve the request against H x W.
pub fn validate_workspace_snapshot(snapshot: &WorkspaceSnapshot) -> Result<(), DecodeError> {
    if snapshot.surface_generation == 0 || snapshot.scene_rev == 0 {
        return Err(DecodeError::InvalidField("revision"));
    }
    if snapshot.rows.min < MIN_WORKSPACE_ROWS
        || snapshot.rows.min > snapshot.rows.preferred
        || snapshot.rows.preferred > snapshot.rows.max
        || snapshot.rows.max > DEFAULT_LIMIT_DOCK_ROWS as u16
    {
        return Err(DecodeError::InvalidField("rows"));
    }
    if snapshot.nodes.is_empty() || snapshot.nodes.len() > DEFAULT_LIMIT_NODES as usize {
        return Err(DecodeError::InvalidField("nodes"));
    }
    let mut ids = BTreeSet::new();
    for node in &snapshot.nodes {
        if node.id == 0 || !ids.insert(node.id) {
            return Err(DecodeError::InvalidField("nodes"));
        }
        if node.min > node.preferred {
            return Err(DecodeError::InvalidField("nodes"));
        }
        if node.show_max_cols != 0 && node.show_min_cols > node.show_max_cols {
            return Err(DecodeError::InvalidField("nodes"));
        }
        validate_tree_text(&node.text).map_err(|_| DecodeError::InvalidField("nodes"))?;
    }
    let roots = snapshot
        .nodes
        .iter()
        .filter(|node| node.parent == 0)
        .count();
    if roots != 1 {
        return Err(DecodeError::InvalidField("nodes"));
    }
    let by_id: BTreeMap<_, _> = snapshot.nodes.iter().map(|node| (node.id, node)).collect();
    let mut child_counts = BTreeMap::<u32, usize>::new();
    for node in &snapshot.nodes {
        if node.parent != 0 && !by_id.contains_key(&node.parent) {
            return Err(DecodeError::InvalidField("nodes"));
        }
        *child_counts.entry(node.parent).or_default() += 1;
        let mut cursor = node;
        let mut path = BTreeSet::new();
        let mut depth = 1usize;
        while cursor.parent != 0 {
            if !path.insert(cursor.id) || depth >= DEFAULT_LIMIT_TREE_DEPTH as usize {
                return Err(DecodeError::InvalidField("nodes"));
            }
            cursor = by_id
                .get(&cursor.parent)
                .copied()
                .ok_or(DecodeError::InvalidField("nodes"))?;
            depth += 1;
        }
    }
    for node in &snapshot.nodes {
        let children = child_counts.get(&node.id).copied().unwrap_or(0);
        match node.kind {
            TreeNodeKind::Text | TreeNodeKind::Spacer if children != 0 => {
                return Err(DecodeError::InvalidField("nodes"));
            }
            TreeNodeKind::Border if children > 1 => {
                return Err(DecodeError::InvalidField("nodes"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn parse_fields<'a>(
    parts: impl Iterator<Item = &'a str>,
) -> Result<BTreeMap<&'a str, &'a str>, DecodeError> {
    let mut fields = BTreeMap::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            // Unknown bare token: ignore for forward compatibility only when
            // it is not a required-field shape we care about. Reject empties.
            continue;
        };
        if key.is_empty() {
            return Err(DecodeError::InvalidField("key"));
        }
        if fields.insert(key, value).is_some() {
            return Err(duplicate_field(key));
        }
    }
    Ok(fields)
}

fn require_u32(fields: &BTreeMap<&str, &str>, key: &'static str) -> Result<u32, DecodeError> {
    let raw = fields.get(key).ok_or(DecodeError::MissingField(key))?;
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecodeError::InvalidField(key));
    }
    raw.parse().map_err(|_| DecodeError::InvalidField(key))
}

pub(crate) fn require_u64(
    fields: &BTreeMap<&str, &str>,
    key: &'static str,
) -> Result<u64, DecodeError> {
    let raw = fields.get(key).ok_or(DecodeError::MissingField(key))?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DecodeError::InvalidField(key));
    }
    raw.parse().map_err(|_| DecodeError::InvalidField(key))
}

fn require_u16(fields: &BTreeMap<&str, &str>, key: &'static str) -> Result<u16, DecodeError> {
    let value = require_u32(fields, key)?;
    u16::try_from(value).map_err(|_| DecodeError::InvalidField(key))
}

fn require_version(
    fields: &BTreeMap<&str, &str>,
    key: &'static str,
) -> Result<ProtocolVersion, DecodeError> {
    let raw = fields.get(key).ok_or(DecodeError::MissingField(key))?;
    ProtocolVersion::parse(raw).ok_or(DecodeError::InvalidField(key))
}

/// Encode a complete 7-bit APC sequence for a body.
pub fn encode_apc(body: &str) -> Result<Vec<u8>, DecodeError> {
    if body.len() > MAX_CONTROL_BODY_BYTES {
        return Err(DecodeError::Oversized);
    }
    if !body.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    let mut out = Vec::with_capacity(APC_INTRODUCER.len() + body.len() + STRING_TERMINATOR.len());
    out.extend_from_slice(APC_INTRODUCER);
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(STRING_TERMINATOR);
    Ok(out)
}

/// Wrap `inner` in tmux's documented DCS passthrough (`ESC P tmux; … ST`).
///
/// Each `ESC` in `inner` is doubled so tmux forwards the payload to the outer
/// terminal. Used by apps inside tmux when `allow-passthrough` is on (matrix T3).
pub fn encode_tmux_passthrough(inner: &[u8]) -> Vec<u8> {
    let extra = inner.iter().filter(|&&b| b == 0x1b).count();
    let mut out = Vec::with_capacity(8 + inner.len() + extra + 2);
    out.extend_from_slice(b"\x1bPtmux;");
    for &byte in inner {
        if byte == 0x1b {
            out.push(0x1b);
        }
        out.push(byte);
    }
    out.extend_from_slice(STRING_TERMINATOR);
    out
}

/// Inverse of [`encode_tmux_passthrough`] for tests and hosts that see a wrapper.
pub fn decode_tmux_passthrough(wrapped: &[u8]) -> Option<Vec<u8>> {
    let rest = wrapped.strip_prefix(b"\x1bPtmux;")?;
    let body = rest.strip_suffix(STRING_TERMINATOR)?;
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == 0x1b {
            i += 1;
            if i >= body.len() {
                return None;
            }
        }
        out.push(body[i]);
        i += 1;
    }
    Some(out)
}

pub fn encode_capability_query(query: CapabilityQuery) -> Result<Vec<u8>, DecodeError> {
    encode_apc(&format!(
        "{NAMESPACE};cap;q;id={};max={}",
        query.request_id.get(),
        query.max_version
    ))
}

pub fn encode_capability_reply(reply: &CapabilityReply) -> Result<Vec<u8>, DecodeError> {
    // Omit `features` when empty so decode's "missing features => empty set"
    // path is used; `features=` (empty element list) is invalid.
    let mut body = format!(
        "{NAMESPACE};cap;r;id={};v={}",
        reply.request_id.get(),
        reply.version
    );
    if !reply.features.is_empty() {
        let features = reply
            .features
            .iter()
            .map(|feature| feature.as_str())
            .collect::<Vec<_>>()
            .join(",");
        body.push_str(";features=");
        body.push_str(&features);
    }
    for (key, value) in &reply.limits {
        if !key.starts_with("limit.") || key.len() <= "limit.".len() {
            return Err(DecodeError::InvalidField("limit"));
        }
        body.push(';');
        body.push_str(key);
        body.push('=');
        body.push_str(&value.to_string());
    }
    encode_apc(&body)
}

/// Resolve a top-dock row request against the full pane content geometry.
/// Returns `None` without reserving rows when the frozen minimum transcript
/// or width cannot be preserved.
pub fn resolve_workspace_rows(
    request: WorkspaceRows,
    pane_rows: u16,
    pane_cols: u16,
) -> Option<u16> {
    if request.min < MIN_WORKSPACE_ROWS
        || request.min > request.preferred
        || request.preferred > request.max
        || request.max > DEFAULT_LIMIT_DOCK_ROWS as u16
        || pane_rows < MIN_WORKSPACE_ROWS + MIN_TRANSCRIPT_ROWS
        || pane_cols < MIN_WORKSPACE_COLS
    {
        return None;
    }
    let geometry_max = pane_rows
        .saturating_mul(3)
        .checked_div(5)
        .unwrap_or(0)
        .min(pane_rows.saturating_sub(MIN_TRANSCRIPT_ROWS))
        .min(DEFAULT_LIMIT_DOCK_ROWS as u16)
        .min(request.max);
    if geometry_max < request.min {
        return None;
    }
    Some(request.preferred.min(geometry_max).max(request.min))
}

pub fn encode_workspace_snapshot(snapshot: &WorkspaceSnapshot) -> Result<Vec<u8>, DecodeError> {
    validate_workspace_snapshot(snapshot)?;
    let nodes = encode_tree_nodes(&snapshot.nodes)?;
    encode_apc(&format!(
        "{NAMESPACE};workspace;snapshot;generation={};rev={};min={};preferred={};max={};nodes={nodes}",
        snapshot.surface_generation,
        snapshot.scene_rev,
        snapshot.rows.min,
        snapshot.rows.preferred,
        snapshot.rows.max,
    ))
}

pub fn encode_workspace_drop(surface_generation: u64) -> Result<Vec<u8>, DecodeError> {
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    encode_apc(&format!(
        "{NAMESPACE};workspace;drop;generation={surface_generation}"
    ))
}

pub fn encode_status_snapshot(snapshot: &StatusSnapshot) -> Result<Vec<u8>, DecodeError> {
    validate_status_snapshot(snapshot)?;
    let mut records = Vec::with_capacity(snapshot.items.len());
    for item in &snapshot.items {
        let record = match &item.visual {
            StatusVisual::Badge => {
                format!("{},badge,{}", item.node_id, item.tone.as_str())
            }
            StatusVisual::Meter {
                current,
                total: Some(total),
            } => format!(
                "{},meter,{},{current},{total}",
                item.node_id,
                item.tone.as_str()
            ),
            StatusVisual::Meter { total: None, .. } => {
                format!("{},meter,{},-,-", item.node_id, item.tone.as_str())
            }
            StatusVisual::Sparkline { samples } => {
                let samples = samples
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(".");
                format!(
                    "{},sparkline,{},{}",
                    item.node_id,
                    item.tone.as_str(),
                    samples
                )
            }
        };
        records.push(record);
    }
    encode_apc(&format!(
        "{NAMESPACE};status;snapshot;generation={};scene={};rev={};items={}",
        snapshot.surface_generation,
        snapshot.scene_rev,
        snapshot.rev,
        records.join("/")
    ))
}

pub fn encode_status_drop(surface_generation: u64) -> Result<Vec<u8>, DecodeError> {
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    encode_apc(&format!(
        "{NAMESPACE};status;drop;generation={surface_generation}"
    ))
}

pub fn encode_collection_snapshot(snapshot: &CollectionSnapshot) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(&snapshot.collection_id)?;
    if snapshot.surface_generation == 0 || snapshot.rev == 0 {
        return Err(DecodeError::InvalidField("revision"));
    }
    if snapshot.items.len() > DEFAULT_LIMIT_COLLECTION_ITEMS as usize {
        return Err(DecodeError::InvalidField("items"));
    }
    let items = encode_collection_items(&snapshot.items)?;
    encode_apc(&format!(
        "{NAMESPACE};collection;snapshot;generation={};id={};rev={};items={items}",
        snapshot.surface_generation, snapshot.collection_id, snapshot.rev
    ))
}

pub fn encode_collection_patch(patch: &CollectionPatch) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(&patch.collection_id)?;
    if patch.surface_generation == 0
        || patch.base == 0
        || Some(patch.next) != patch.base.checked_add(1)
    {
        return Err(DecodeError::InvalidField("revision"));
    }
    if patch.items.is_empty() || patch.items.len() > DEFAULT_LIMIT_PATCH_OPS as usize {
        return Err(DecodeError::InvalidField("items"));
    }
    let items = encode_collection_items(&patch.items)?;
    encode_apc(&format!(
        "{NAMESPACE};collection;{};generation={};id={};base={};next={};items={items}",
        patch.kind.as_str(),
        patch.surface_generation,
        patch.collection_id,
        patch.base,
        patch.next
    ))
}

pub fn encode_collection_drop(
    surface_generation: u64,
    collection_id: &str,
) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(collection_id)?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    encode_apc(&format!(
        "{NAMESPACE};collection;drop;generation={surface_generation};id={collection_id}"
    ))
}

pub fn encode_collection_ack(
    surface_generation: u64,
    collection_id: &str,
    rev: u64,
) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(collection_id)?;
    if surface_generation == 0 || rev == 0 {
        return Err(DecodeError::InvalidField("revision"));
    }
    encode_apc(&format!(
        "{NAMESPACE};collection;ack;generation={surface_generation};id={collection_id};rev={rev}"
    ))
}

pub fn encode_collection_reject(
    surface_generation: u64,
    collection_id: &str,
    reason: CollectionRejectReason,
) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(collection_id)?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    encode_apc(&format!(
        "{NAMESPACE};collection;reject;generation={surface_generation};id={collection_id};reason={}",
        reason.as_str()
    ))
}

pub fn encode_collection_resnapshot(
    surface_generation: u64,
    collection_id: &str,
) -> Result<Vec<u8>, DecodeError> {
    validate_collection_id(collection_id)?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    encode_apc(&format!(
        "{NAMESPACE};collection;resnapshot;generation={surface_generation};id={collection_id}"
    ))
}

fn validate_collection_id(value: &str) -> Result<(), DecodeError> {
    if value.is_empty()
        || value.len() > 48
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DecodeError::InvalidField("id"));
    }
    Ok(())
}

fn encode_collection_items(items: &[CollectionItem]) -> Result<String, DecodeError> {
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        if item.id == 0 {
            return Err(DecodeError::ZeroId);
        }
        if index > 0 {
            out.push('/');
        }
        let text = escape_tree_text(&item.text).map_err(|_| DecodeError::InvalidField("items"))?;
        out.push_str(&format!(
            "{},{},{text}",
            item.id,
            u8::from(item.replaceable)
        ));
    }
    Ok(out)
}

fn decode_collection_items(raw: &str) -> Result<Vec<CollectionItem>, DecodeError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    for record in raw.split('/') {
        if items.len() >= DEFAULT_LIMIT_COLLECTION_ITEMS as usize {
            return Err(DecodeError::InvalidField("items"));
        }
        let fields: Vec<_> = record.splitn(3, ',').collect();
        if fields.len() != 3 {
            return Err(DecodeError::InvalidField("items"));
        }
        if fields[0].is_empty() || !fields[0].bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(DecodeError::InvalidField("items"));
        }
        let id = fields[0]
            .parse::<u32>()
            .map_err(|_| DecodeError::InvalidField("items"))?;
        if id == 0 {
            return Err(DecodeError::ZeroId);
        }
        let replaceable = match fields[1] {
            "0" => false,
            "1" => true,
            _ => return Err(DecodeError::InvalidField("items")),
        };
        items.push(CollectionItem {
            id,
            replaceable,
            text: unescape_tree_text(fields[2]).map_err(|_| DecodeError::InvalidField("items"))?,
        });
    }
    Ok(items)
}

fn decode_collection<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    let surface_generation = require_u64(&fields, "generation")?;
    if surface_generation == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    let collection_id = fields
        .get("id")
        .ok_or(DecodeError::MissingField("id"))?
        .to_string();
    validate_collection_id(&collection_id)?;
    match kind {
        "snapshot" => {
            let snapshot = CollectionSnapshot {
                surface_generation,
                collection_id,
                rev: require_u64(&fields, "rev")?,
                items: decode_collection_items(
                    fields
                        .get("items")
                        .ok_or(DecodeError::MissingField("items"))?,
                )?,
            };
            if snapshot.rev == 0 {
                return Err(DecodeError::InvalidField("revision"));
            }
            Ok(ControlMessage::CollectionSnapshot(snapshot))
        }
        "append" | "replace" => {
            let patch = CollectionPatch {
                surface_generation,
                collection_id,
                base: require_u64(&fields, "base")?,
                next: require_u64(&fields, "next")?,
                kind: if kind == "append" {
                    CollectionPatchKind::Append
                } else {
                    CollectionPatchKind::Replace
                },
                items: decode_collection_items(
                    fields
                        .get("items")
                        .ok_or(DecodeError::MissingField("items"))?,
                )?,
            };
            if patch.base == 0
                || Some(patch.next) != patch.base.checked_add(1)
                || patch.items.is_empty()
            {
                return Err(DecodeError::InvalidField("revision"));
            }
            if patch.items.len() > DEFAULT_LIMIT_PATCH_OPS as usize {
                return Err(DecodeError::InvalidField("items"));
            }
            Ok(ControlMessage::CollectionPatch(patch))
        }
        "drop" => Ok(ControlMessage::CollectionDrop {
            surface_generation,
            collection_id,
        }),
        "ack" => {
            let rev = require_u64(&fields, "rev")?;
            if rev == 0 {
                return Err(DecodeError::InvalidField("revision"));
            }
            Ok(ControlMessage::CollectionAck {
                surface_generation,
                collection_id,
                rev,
            })
        }
        "reject" => {
            let raw = fields
                .get("reason")
                .ok_or(DecodeError::MissingField("reason"))?;
            let reason =
                CollectionRejectReason::parse(raw).ok_or(DecodeError::InvalidField("reason"))?;
            Ok(ControlMessage::CollectionReject {
                surface_generation,
                collection_id,
                reason,
            })
        }
        "resnapshot" => Ok(ControlMessage::CollectionResnapshot {
            surface_generation,
            collection_id,
        }),
        _ => Err(DecodeError::UnknownFamily),
    }
}

/// Install a full collection snapshot. Duplicate revision+payload is a no-op ACK.
pub fn apply_collection_snapshot(
    current: Option<&CollectionSnapshot>,
    incoming: CollectionSnapshot,
) -> Result<Option<CollectionSnapshot>, CollectionRejectReason> {
    if incoming.rev == 0 || incoming.surface_generation == 0 {
        return Err(CollectionRejectReason::Conflict);
    }
    if incoming.items.len() > DEFAULT_LIMIT_COLLECTION_ITEMS as usize {
        return Err(CollectionRejectReason::Backpressure);
    }
    match current {
        Some(existing) if existing.surface_generation != incoming.surface_generation => {
            Err(CollectionRejectReason::Stale)
        }
        Some(existing) if existing.rev == incoming.rev && existing.items == incoming.items => {
            Ok(None)
        }
        Some(existing) if existing.rev == incoming.rev => Err(CollectionRejectReason::Conflict),
        Some(existing) if incoming.rev < existing.rev => Err(CollectionRejectReason::Stale),
        _ => Ok(Some(incoming)),
    }
}

/// Apply one ordered patch. A gap or stale base drops only this collection.
pub fn apply_collection_patch(
    current: Option<&CollectionSnapshot>,
    patch: CollectionPatch,
) -> Result<CollectionSnapshot, CollectionRejectReason> {
    if Some(patch.next) != patch.base.checked_add(1) || patch.items.is_empty() {
        return Err(CollectionRejectReason::Conflict);
    }
    let Some(existing) = current else {
        return Err(CollectionRejectReason::Gap);
    };
    if existing.surface_generation != patch.surface_generation {
        return Err(CollectionRejectReason::Stale);
    }
    if patch.base < existing.rev {
        return Err(CollectionRejectReason::Stale);
    }
    if patch.base > existing.rev {
        return Err(CollectionRejectReason::Gap);
    }
    let mut items = existing.items.clone();
    match patch.kind {
        CollectionPatchKind::Append => {
            if items.len() + patch.items.len() > DEFAULT_LIMIT_COLLECTION_ITEMS as usize {
                return Err(CollectionRejectReason::Backpressure);
            }
            items.extend(patch.items);
        }
        CollectionPatchKind::Replace => {
            for incoming in patch.items {
                if incoming.replaceable {
                    if let Some(slot) = items
                        .iter_mut()
                        .rev()
                        .find(|item| item.id == incoming.id && item.replaceable)
                    {
                        *slot = incoming;
                        continue;
                    }
                }
                if items.len() >= DEFAULT_LIMIT_COLLECTION_ITEMS as usize {
                    return Err(CollectionRejectReason::Backpressure);
                }
                items.push(incoming);
            }
        }
    }
    Ok(CollectionSnapshot {
        surface_generation: existing.surface_generation,
        collection_id: existing.collection_id.clone(),
        rev: patch.next,
        items,
    })
}

fn encode_tree_nodes(nodes: &[TreeNode]) -> Result<String, DecodeError> {
    let mut out = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        let text = escape_tree_text(&node.text).map_err(|_| DecodeError::InvalidField("nodes"))?;
        if node.action_id == 0 {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{}",
                node.id,
                node.parent,
                node.kind.as_str(),
                node.min,
                node.preferred,
                node.fill,
                node.show_min_cols,
                node.show_max_cols,
                text
            ));
        } else {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}",
                node.id,
                node.parent,
                node.kind.as_str(),
                node.min,
                node.preferred,
                node.fill,
                node.show_min_cols,
                node.show_max_cols,
                node.action_id,
                text
            ));
        }
    }
    Ok(out)
}

pub fn encode_attach_cell_rect(attach: &AttachCellRect) -> Result<Vec<u8>, DecodeError> {
    if attach.id == 0 {
        return Err(DecodeError::ZeroId);
    }
    if attach.rows == 0 || attach.cols == 0 {
        return Err(DecodeError::InvalidField("rows/cols"));
    }
    encode_attach_kind(
        "cell_rect",
        attach.id,
        attach.row,
        attach.col,
        attach.rows,
        attach.cols,
        &attach.text,
        &[],
    )
}

pub fn encode_attach_styled(attach: &AttachStyled) -> Result<Vec<u8>, DecodeError> {
    let kind = match attach.kind {
        RegionKind::CellRect => "cell_rect",
        RegionKind::Viewport => "viewport",
    };
    encode_attach_kind(
        kind,
        attach.id,
        attach.row,
        attach.col,
        attach.rows,
        attach.cols,
        "",
        &attach.runs,
    )
}

pub fn encode_attach_viewport(attach: &AttachViewport) -> Result<Vec<u8>, DecodeError> {
    encode_attach_kind(
        "viewport",
        attach.id,
        attach.row,
        attach.col,
        attach.rows,
        attach.cols,
        &attach.text,
        &attach.runs,
    )
}

#[allow(clippy::too_many_arguments)]
fn encode_attach_kind(
    kind: &str,
    id: u32,
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
    text: &str,
    runs: &[StyledRun],
) -> Result<Vec<u8>, DecodeError> {
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    if rows == 0 || cols == 0 {
        return Err(DecodeError::InvalidField("rows/cols"));
    }
    let payload = if runs.is_empty() {
        validate_attach_text(text, rows, cols)?;
        format!("text={text}")
    } else {
        validate_runs(runs, rows, cols)?;
        format!("runs={}", encode_runs(runs)?)
    };
    encode_apc(&format!(
        "{NAMESPACE};attach;{kind};id={id};row={row};col={col};rows={rows};cols={cols};{payload}"
    ))
}

pub fn encode_update(update: &UpdateAttachment) -> Result<Vec<u8>, DecodeError> {
    if update.id == 0 {
        return Err(DecodeError::ZeroId);
    }
    let payload = if update.runs.is_empty() {
        validate_attach_text(&update.text, 1, u16::MAX)?;
        format!("text={}", update.text)
    } else {
        validate_runs(&update.runs, 1, u16::MAX)?;
        format!("runs={}", encode_runs(&update.runs)?)
    };
    encode_apc(&format!("{NAMESPACE};update;id={};{payload}", update.id))
}

pub fn encode_detach(id: u32) -> Result<Vec<u8>, DecodeError> {
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    encode_apc(&format!("{NAMESPACE};detach;id={id}"))
}

pub fn encode_focus_query(id: u32) -> Result<Vec<u8>, DecodeError> {
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    encode_apc(&format!("{NAMESPACE};focus;q;id={id}"))
}

pub fn encode_focus_reply(id: u32, status: FocusStatus) -> Result<Vec<u8>, DecodeError> {
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    encode_apc(&format!(
        "{NAMESPACE};focus;r;id={id};status={}",
        status.as_str()
    ))
}

pub fn encode_focus_key(id: u32, key: &str) -> Result<Vec<u8>, DecodeError> {
    if id == 0 {
        return Err(DecodeError::ZeroId);
    }
    if key.is_empty() || key.len() > MAX_FOCUS_KEY_BYTES {
        return Err(DecodeError::InvalidField("k"));
    }
    if !key.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    let escaped = escape_run_text(key)?;
    encode_apc(&format!("{NAMESPACE};focus;k;id={id};k={escaped}"))
}

fn validate_input_event(event: &InputEvent) -> Result<(), DecodeError> {
    if event.request_seq == 0
        || event.surface_generation == 0
        || event.scene_rev == 0
        || event.node_id == 0
        || event.action_id == 0
    {
        return Err(DecodeError::InvalidField("input identity"));
    }
    match (&event.collection_id, event.collection_rev) {
        (Some(id), rev) if rev > 0 => validate_collection_id(id)?,
        (None, 0) => {}
        _ => return Err(DecodeError::InvalidField("collection_rev")),
    }
    match &event.kind {
        InputEventKind::Focus { .. } | InputEventKind::Pointer { .. } => {}
        InputEventKind::Key { key, modifiers } => {
            if key.is_empty()
                || key.len() > MAX_FOCUS_KEY_BYTES
                || !key.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                || InputModifiers::new(modifiers.bits()).is_none()
            {
                return Err(DecodeError::InvalidField("key"));
            }
        }
        InputEventKind::Scroll { delta } => {
            if *delta == 0 {
                return Err(DecodeError::InvalidField("delta"));
            }
        }
        InputEventKind::Viewport { count, .. } => {
            if *count == 0 {
                return Err(DecodeError::InvalidField("count"));
            }
        }
    }
    Ok(())
}

pub fn encode_input_event(event: &InputEvent) -> Result<Vec<u8>, DecodeError> {
    validate_input_event(event)?;
    let kind = match event.kind {
        InputEventKind::Focus { .. } => "focus",
        InputEventKind::Key { .. } => "key",
        InputEventKind::Pointer { .. } => "pointer",
        InputEventKind::Scroll { .. } => "scroll",
        InputEventKind::Viewport { .. } => "viewport",
    };
    let mut body = format!(
        "{NAMESPACE};input;event;kind={kind};viewer={};seq={};generation={};scene={};node={};action={};collection_rev={}",
        event.viewer_id,
        event.request_seq,
        event.surface_generation,
        event.scene_rev,
        event.node_id,
        event.action_id,
        event.collection_rev,
    );
    if let Some(collection_id) = &event.collection_id {
        body.push_str(";collection=");
        body.push_str(collection_id);
    }
    match &event.kind {
        InputEventKind::Focus { focused } => {
            body.push_str(&format!(";focused={}", u8::from(*focused)));
        }
        InputEventKind::Key { key, modifiers } => {
            body.push_str(";key=");
            body.push_str(&escape_run_text(key)?);
            body.push_str(&format!(";mods={}", modifiers.bits()));
        }
        InputEventKind::Pointer { phase, row, col } => {
            body.push_str(&format!(";phase={};row={row};col={col}", phase.as_str()));
        }
        InputEventKind::Scroll { delta } => body.push_str(&format!(";delta={delta}")),
        InputEventKind::Viewport { first, count } => {
            body.push_str(&format!(";first={first};count={count}"));
        }
    }
    encode_apc(&body)
}

/// Streaming collector for 7-bit APC bodies (`ESC _` … `ESC \`).
///
/// VTE 0.15 swallows APC in `SosPmApcString` without delivering the body to
/// `Perform`, so Prismattyc uses this sidecar on the same byte stream.
#[derive(Debug, Default)]
pub struct ApcCollector {
    state: CollectorState,
    buffer: Vec<u8>,
    overflow: bool,
    invalid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CollectorState {
    #[default]
    Ground,
    Esc,
    Body,
    BodyEsc,
}

/// Outcome of pushing bytes through [`ApcCollector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectedApc {
    /// A complete body ready for [`decode_body`].
    Body(String),
    /// Terminated but oversized or non-printable; discard without decoding.
    Discarded,
}

impl ApcCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a (possibly incomplete) APC sequence is in flight.
    pub const fn is_active(&self) -> bool {
        !matches!(self.state, CollectorState::Ground)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<CollectedApc> {
        let mut events = Vec::new();
        for &byte in bytes {
            if let Some(event) = self.push_byte(byte) {
                events.push(event);
            }
        }
        events
    }

    fn push_byte(&mut self, byte: u8) -> Option<CollectedApc> {
        match self.state {
            CollectorState::Ground => {
                if byte == 0x1b {
                    self.state = CollectorState::Esc;
                }
                None
            }
            CollectorState::Esc => {
                if byte == b'_' {
                    self.state = CollectorState::Body;
                    self.buffer.clear();
                    self.overflow = false;
                    self.invalid = false;
                } else {
                    self.state = CollectorState::Ground;
                }
                None
            }
            CollectorState::Body => match byte {
                0x1b => {
                    self.state = CollectorState::BodyEsc;
                    None
                }
                b if (0x20..=0x7e).contains(&b) => {
                    if self.buffer.len() >= MAX_CONTROL_BODY_BYTES {
                        self.overflow = true;
                    } else if !self.overflow {
                        self.buffer.push(b);
                    }
                    None
                }
                _ => {
                    // Non-printable inside APC body: mark invalid, keep
                    // draining until ST so the classic path stays in sync.
                    self.invalid = true;
                    None
                }
            },
            CollectorState::BodyEsc => {
                if byte == b'\\' {
                    self.state = CollectorState::Ground;
                    if self.overflow || self.invalid {
                        self.buffer.clear();
                        Some(CollectedApc::Discarded)
                    } else {
                        // SAFETY: buffer only contains printable ASCII.
                        let body =
                            String::from_utf8(std::mem::take(&mut self.buffer)).unwrap_or_default();
                        Some(CollectedApc::Body(body))
                    }
                } else if byte == b'_' {
                    // Nested APC introducer: restart collection.
                    self.buffer.clear();
                    self.overflow = false;
                    self.invalid = false;
                    self.state = CollectorState::Body;
                    None
                } else if byte == 0x1b {
                    // Stay in BodyEsc (ESC ESC \ would be odd); keep waiting.
                    None
                } else {
                    // ESC + non-ST: treat ESC as aborting body validity.
                    self.invalid = true;
                    self.state = CollectorState::Body;
                    if (0x20..=0x7e).contains(&byte) {
                        if self.buffer.len() >= MAX_CONTROL_BODY_BYTES {
                            self.overflow = true;
                        } else if !self.overflow {
                            self.buffer.push(byte);
                        }
                    } else {
                        self.invalid = true;
                    }
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_non_zero() {
        assert_eq!(RequestId::new(0), None);
        assert_eq!(RequestId::new(7).map(RequestId::get), Some(7));
    }

    #[test]
    fn version_display_is_wire_compatible() {
        assert_eq!(ProtocolVersion::new(0, 1).to_string(), "0.1");
        assert_eq!(
            ProtocolVersion::parse("0.1"),
            Some(ProtocolVersion::new(0, 1))
        );
        assert_eq!(
            ProtocolVersion::parse("01.2"),
            Some(ProtocolVersion::new(1, 2))
        );
        assert_eq!(ProtocolVersion::parse("0"), None);
        assert_eq!(ProtocolVersion::parse("a.b"), None);
    }

    #[test]
    fn every_known_feature_round_trips() {
        for feature in Feature::ALL {
            assert_eq!(feature.as_str().parse::<Feature>(), Ok(feature));
        }
    }

    #[test]
    fn unknown_feature_preserves_identifier_for_diagnostics() {
        let error = "future.widget".parse::<Feature>().unwrap_err();
        assert_eq!(error.identifier(), "future.widget");
    }

    #[test]
    fn reply_support_is_explicit() {
        let reply = CapabilityReply {
            request_id: RequestId::new(9).unwrap(),
            version: ProtocolVersion::new(0, 1),
            features: BTreeSet::from([Feature::Style, Feature::Canvas]),
            limits: BTreeMap::new(),
        };
        assert!(reply.supports(Feature::Style));
        assert!(!reply.supports(Feature::Animation));
    }

    #[test]
    fn capability_query_round_trip() {
        let query = CapabilityQuery {
            request_id: RequestId::new(7).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        };
        let bytes = encode_capability_query(query).unwrap();
        assert!(bytes.starts_with(APC_INTRODUCER));
        assert!(bytes.ends_with(STRING_TERMINATOR));
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::CapabilityQuery(query)
        );
    }

    #[test]
    fn capability_reply_round_trip_and_spike_builder() {
        let query = CapabilityQuery {
            request_id: RequestId::new(3).unwrap(),
            max_version: ProtocolVersion::new(0, 5),
        };
        let reply = CapabilityReply::for_spike_query(query).unwrap();
        assert_eq!(reply.version, ProtocolVersion::new(0, 1));
        assert!(reply.supports(Feature::HybridAttachCellRect));
        assert!(!reply.supports(Feature::Canvas));
        let bytes = encode_capability_reply(&reply).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::CapabilityReply(reply)
        );
    }

    #[test]
    fn major_mismatch_produces_no_spike_reply() {
        let query = CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(1, 0),
        };
        assert_eq!(CapabilityReply::for_spike_query(query), None);
    }

    #[test]
    fn version_zero_reply_does_not_advertise_spike_features() {
        let query = CapabilityQuery {
            request_id: RequestId::new(2).unwrap(),
            max_version: ProtocolVersion::new(0, 0),
        };
        let reply = CapabilityReply::for_spike_query(query).unwrap();
        assert_eq!(reply.version, ProtocolVersion::new(0, 0));
        assert!(reply.features.is_empty());
        assert!(!reply.supports(Feature::HybridAttachCellRect));
    }

    #[test]
    fn attach_rejects_delimiter_bytes_and_oversize_geometry_text() {
        assert_eq!(
            encode_attach_cell_rect(&AttachCellRect {
                id: 1,
                row: 0,
                col: 0,
                rows: 1,
                cols: 12,
                text: "status;detail=ok".into(),
            }),
            Err(DecodeError::InvalidField("text"))
        );
        assert_eq!(
            decode_body(
                "Prismattyc;attach;cell_rect;id=1;row=0;col=0;rows=1;cols=12;text=status;detail=ok"
            ),
            Err(DecodeError::InvalidField("text"))
        );
        assert_eq!(
            encode_attach_cell_rect(&AttachCellRect {
                id: 1,
                row: 0,
                col: 0,
                rows: 0,
                cols: 1,
                text: "x".into(),
            }),
            Err(DecodeError::InvalidField("rows/cols"))
        );
        assert_eq!(
            encode_attach_cell_rect(&AttachCellRect {
                id: 1,
                row: 0,
                col: 0,
                rows: 1,
                cols: 3,
                text: "toolong".into(),
            }),
            Err(DecodeError::InvalidField("text"))
        );
        assert_eq!(
            decode_body("Prismattyc;attach;cell_rect;id=1;row=0;col=0;rows=1;cols=3;text=toolong"),
            Err(DecodeError::InvalidField("text"))
        );
    }

    #[test]
    fn attach_text_printable_domain_round_trips_without_delimiters() {
        // Every allowed printable byte except field delimiters `;` and `=`.
        let mut text = String::new();
        for b in 0x20u8..=0x7eu8 {
            if b != b';' && b != b'=' {
                text.push(b as char);
            }
        }
        // Fit geometry to text length (chunked into one wide row).
        let cols = u16::try_from(text.len()).unwrap();
        let attach = AttachCellRect {
            id: 9,
            row: 0,
            col: 0,
            rows: 1,
            cols,
            text: text.clone(),
        };
        let bytes = encode_attach_cell_rect(&attach).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::AttachCellRect(attach)
        );
    }

    #[test]
    fn rejects_zero_id_duplicate_and_empty_feature_elements() {
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=0;max=0.1"),
            Err(DecodeError::ZeroId)
        );
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=1;id=2;max=0.1"),
            Err(DecodeError::DuplicateField("id"))
        );
        assert_eq!(
            decode_body("Prismattyc;cap;r;id=1;v=0.1;features=style,"),
            Err(DecodeError::InvalidField("features"))
        );
    }

    #[test]
    fn ignores_unknown_keys_and_unknown_features() {
        let message = decode_body(
            "Prismattyc;cap;r;id=1;v=0.1;features=hybrid.attach.cell_rect,future.widget;extra=1",
        )
        .unwrap();
        match message {
            ControlMessage::CapabilityReply(reply) => {
                assert!(reply.supports(Feature::HybridAttachCellRect));
                assert_eq!(reply.features.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_non_printable_and_oversized_bodies() {
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=1;max=0.1\n"),
            Err(DecodeError::NonPrintableAscii)
        );
        let huge = format!("Prismattyc;cap;q;id=1;max=0.1;pad={}", "x".repeat(4100));
        assert_eq!(decode_body(&huge), Err(DecodeError::Oversized));
    }

    #[test]
    fn attach_and_detach_round_trip() {
        let attach = AttachCellRect {
            id: 4,
            row: 0,
            col: 1,
            rows: 1,
            cols: 12,
            text: "status:ok".into(),
        };
        let bytes = encode_attach_cell_rect(&attach).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::AttachCellRect(attach)
        );
        let detach = encode_detach(4).unwrap();
        let body = std::str::from_utf8(&detach[2..detach.len() - 2]).unwrap();
        assert_eq!(decode_body(body).unwrap(), ControlMessage::Detach { id: 4 });
    }

    #[test]
    fn apc_collector_handles_fragmented_input() {
        let encoded = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(9).unwrap(),
            max_version: ProtocolVersion::new(0, 1),
        })
        .unwrap();
        let mut collector = ApcCollector::new();
        assert!(collector.push(&encoded[..3]).is_empty());
        assert!(collector.push(&encoded[3..8]).is_empty());
        let events = collector.push(&encoded[8..]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CollectedApc::Body(body) => {
                assert!(matches!(
                    decode_body(body).unwrap(),
                    ControlMessage::CapabilityQuery(_)
                ));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn apc_collector_discards_oversized_bodies() {
        let mut collector = ApcCollector::new();
        let mut bytes = APC_INTRODUCER.to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_CONTROL_BODY_BYTES + 8));
        bytes.extend_from_slice(STRING_TERMINATOR);
        let events = collector.push(&bytes);
        assert_eq!(events, vec![CollectedApc::Discarded]);
    }

    #[test]
    fn apc_collector_does_not_leak_into_ground_text() {
        let mut collector = ApcCollector::new();
        let events = collector.push(b"left\x1b_Prismattyc;cap;q;id=1;max=0.1\x1b\\right");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], CollectedApc::Body(_)));
    }

    #[test]
    fn v1_query_max_01_is_byte_identical_to_spike() {
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
        assert_eq!(v1.version, ProtocolVersion::new(0, 1));
        assert!(v1.limits.is_empty());
        assert!(!v1.supports(Feature::HybridOverlayViewport));
        assert!(!v1.supports(Feature::InputRichFocus));
    }

    #[test]
    fn v1_query_advertises_viewport_limits_and_not_markup() {
        let query = CapabilityQuery {
            request_id: RequestId::new(4).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        };
        let reply = CapabilityReply::for_v1_query(query).unwrap();
        assert_eq!(reply.version, ProtocolVersion::new(0, 2));
        assert!(reply.supports(Feature::HybridAttachCellRect));
        assert!(reply.supports(Feature::HybridOverlayViewport));
        assert!(reply.supports(Feature::InputRichFocus));
        assert!(!reply.supports(Feature::Markup));
        assert!(!reply.supports(Feature::Animation));
        assert!(!reply.supports(Feature::Canvas));
        assert_eq!(
            reply.limits.get(LIMIT_REGIONS),
            Some(&DEFAULT_LIMIT_REGIONS)
        );
        assert_eq!(
            reply.limits.get(LIMIT_BODY),
            Some(&(MAX_CONTROL_BODY_BYTES as u32))
        );
        let bytes = encode_capability_reply(&reply).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::CapabilityReply(reply)
        );
    }

    #[test]
    fn surface_query_preserves_frozen_01_and_02_replies() {
        for minor in [1, 2] {
            let query = CapabilityQuery {
                request_id: RequestId::new(44).unwrap(),
                max_version: ProtocolVersion::new(0, minor),
            };
            let frozen = CapabilityReply::for_v1_query(query).unwrap();
            let surface = CapabilityReply::for_surface_query(query).unwrap();
            assert_eq!(surface, frozen);
            assert_eq!(
                encode_capability_reply(&surface).unwrap(),
                encode_capability_reply(&frozen).unwrap()
            );
        }
    }

    #[test]
    fn surface_query_advertises_only_implemented_slice_and_bounds() {
        let reply = CapabilityReply::for_surface_query(CapabilityQuery {
            request_id: RequestId::new(45).unwrap(),
            max_version: HOST_V2_PROTOCOL_VERSION,
        })
        .unwrap();
        assert_eq!(reply.version, HOST_V2_PROTOCOL_VERSION);
        assert_eq!(reply.features, BTreeSet::from(Feature::V2));
        assert!(reply.supports(Feature::HybridReserveRows));
        assert!(reply.supports(Feature::RichTreeV1));
        assert!(reply.supports(Feature::RichCollectionV1));
        assert!(reply.supports(Feature::InputRichKeyboardV1));
        assert!(reply.supports(Feature::InputRichPointerV1));
        assert!(reply.supports(Feature::InputRichScrollV1));
        assert!(reply.supports(Feature::RichSemanticTextV1));
        assert!(reply.supports(Feature::RichStatusV1));
        assert_eq!(reply.limits.get(LIMIT_SURFACES), Some(&1));
        assert_eq!(reply.limits.get(LIMIT_NODES), Some(&128));
        assert_eq!(reply.limits.get(LIMIT_TREE_DEPTH), Some(&16));
        assert_eq!(reply.limits.get(LIMIT_DOCK_ROWS), Some(&24));
    }

    fn workspace_snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            surface_generation: 7,
            scene_rev: 3,
            rows: WorkspaceRows {
                min: 5,
                preferred: 9,
                max: 12,
            },
            nodes: vec![
                TreeNode {
                    id: 1,
                    parent: 0,
                    kind: TreeNodeKind::Column,
                    min: 5,
                    preferred: 9,
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
                    action_id: 9,
                    text: "task, one / safe".into(),
                },
                TreeNode {
                    id: 3,
                    parent: 1,
                    kind: TreeNodeKind::Spacer,
                    min: 4,
                    preferred: 8,
                    fill: 1,
                    show_min_cols: 0,
                    show_max_cols: 0,
                    action_id: 0,
                    text: String::new(),
                },
            ],
        }
    }

    #[test]
    fn workspace_snapshot_round_trips_and_drop_is_explicit() {
        let snapshot = workspace_snapshot();
        let encoded = encode_workspace_snapshot(&snapshot).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::WorkspaceSnapshot(snapshot)
        );

        let drop = encode_workspace_drop(7).unwrap();
        let body = std::str::from_utf8(&drop[2..drop.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::WorkspaceDrop {
                surface_generation: 7
            }
        );
    }

    #[test]
    fn status_snapshot_round_trips_static_primitives_and_drop() {
        assert_eq!(StatusTone::Neutral.ansi_index(), None);
        assert_eq!(StatusTone::Info.ansi_index(), Some(4));
        assert_eq!(StatusTone::Success.ansi_index(), Some(2));
        assert_eq!(StatusTone::Warning.ansi_index(), Some(3));
        assert_eq!(StatusTone::Danger.ansi_index(), Some(1));
        let snapshot = StatusSnapshot {
            surface_generation: 7,
            scene_rev: 3,
            rev: 4,
            items: vec![
                StatusItem {
                    node_id: 2,
                    tone: StatusTone::Success,
                    visual: StatusVisual::Badge,
                },
                StatusItem {
                    node_id: 3,
                    tone: StatusTone::Info,
                    visual: StatusVisual::Meter {
                        current: 3,
                        total: Some(5),
                    },
                },
                StatusItem {
                    node_id: 4,
                    tone: StatusTone::Warning,
                    visual: StatusVisual::Meter {
                        current: 0,
                        total: None,
                    },
                },
                StatusItem {
                    node_id: 5,
                    tone: StatusTone::Danger,
                    visual: StatusVisual::Sparkline {
                        samples: vec![2, 0, 2, 1],
                    },
                },
            ],
        };
        let encoded = encode_status_snapshot(&snapshot).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::StatusSnapshot(snapshot)
        );
        let drop = encode_status_drop(7).unwrap();
        let body = std::str::from_utf8(&drop[2..drop.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::StatusDrop {
                surface_generation: 7
            }
        );
    }

    #[test]
    fn status_snapshot_rejects_invented_or_unbounded_values() {
        let mut snapshot = StatusSnapshot {
            surface_generation: 1,
            scene_rev: 1,
            rev: 1,
            items: vec![StatusItem {
                node_id: 1,
                tone: StatusTone::Info,
                visual: StatusVisual::Meter {
                    current: 6,
                    total: Some(5),
                },
            }],
        };
        assert_eq!(
            validate_status_snapshot(&snapshot),
            Err(DecodeError::InvalidField("items"))
        );
        snapshot.items[0].visual = StatusVisual::Sparkline {
            samples: vec![1; DEFAULT_LIMIT_SPARKLINE_SAMPLES as usize + 1],
        };
        assert_eq!(
            validate_status_snapshot(&snapshot),
            Err(DecodeError::InvalidField("items"))
        );
        assert_eq!(
            decode_body(
                "Prismattyc;status;snapshot;generation=1;scene=1;rev=1;items=1,meter,info,-,5"
            ),
            Err(DecodeError::InvalidField("items"))
        );
    }

    #[test]
    fn workspace_tree_text_round_trips_wide_and_combining() {
        let mut snapshot = workspace_snapshot();
        snapshot.nodes[1].text = "> 警告 e\u{0301} crates/wide.rs:1:1".into();
        validate_workspace_snapshot(&snapshot).unwrap();
        let encoded = encode_workspace_snapshot(&snapshot).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        let ControlMessage::WorkspaceSnapshot(round) = decode_body(body).unwrap() else {
            panic!("expected workspace snapshot");
        };
        assert_eq!(round.nodes[1].text, "> 警告 e\u{0301} crates/wide.rs:1:1");
        assert!(body.is_ascii());
    }

    #[test]
    fn structured_input_round_trips_and_rejects_unbound_shapes() {
        let viewer_id = ViewerId::from_bytes([0x11; 16]);
        let key = InputEvent {
            viewer_id,
            request_seq: 2,
            surface_generation: 7,
            scene_rev: 3,
            node_id: 2,
            action_id: 9,
            collection_id: None,
            collection_rev: 0,
            kind: InputEventKind::Key {
                key: "Enter".into(),
                modifiers: InputModifiers::new(InputModifiers::CONTROL).unwrap(),
            },
        };
        let encoded = encode_input_event(&key).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        assert_eq!(decode_body(body).unwrap(), ControlMessage::InputEvent(key));
        assert!(body.contains("viewer=11111111111111111111111111111111"));

        let scroll = InputEvent {
            viewer_id,
            request_seq: 3,
            surface_generation: 7,
            scene_rev: 3,
            node_id: 2,
            action_id: 9,
            collection_id: Some("diag".into()),
            collection_rev: 4,
            kind: InputEventKind::Scroll { delta: -3 },
        };
        let encoded = encode_input_event(&scroll).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::InputEvent(scroll)
        );

        assert!(decode_body("Prismattyc;input;event;kind=key;viewer=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA;seq=1;generation=1;scene=1;node=1;action=1;collection_rev=0;key=a;mods=0").is_err());
        let mut invalid = InputEvent {
            viewer_id,
            request_seq: 0,
            surface_generation: 1,
            scene_rev: 1,
            node_id: 1,
            action_id: 1,
            collection_id: None,
            collection_rev: 0,
            kind: InputEventKind::Focus { focused: true },
        };
        assert!(encode_input_event(&invalid).is_err());
        invalid.request_seq = 1;
        invalid.kind = InputEventKind::Scroll { delta: 0 };
        assert!(encode_input_event(&invalid).is_err());
    }

    #[test]
    fn collection_snapshot_round_trips_and_patches_are_ordered() {
        let snapshot = CollectionSnapshot {
            surface_generation: 3,
            collection_id: "diag".into(),
            rev: 7,
            items: vec![CollectionItem {
                id: 1,
                replaceable: false,
                text: "error at src/lib.rs:1".into(),
            }],
        };
        let encoded = encode_collection_snapshot(&snapshot).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::CollectionSnapshot(snapshot.clone())
        );

        let patch = CollectionPatch {
            surface_generation: 3,
            collection_id: "diag".into(),
            base: 7,
            next: 8,
            kind: CollectionPatchKind::Append,
            items: vec![CollectionItem {
                id: 2,
                replaceable: false,
                text: "second".into(),
            }],
        };
        let applied = apply_collection_patch(Some(&snapshot), patch.clone()).unwrap();
        assert_eq!(applied.rev, 8);
        assert_eq!(applied.items.len(), 2);

        let gap = CollectionPatch {
            base: 8,
            next: 9,
            ..patch
        };
        assert_eq!(
            apply_collection_patch(Some(&snapshot), gap),
            Err(CollectionRejectReason::Gap)
        );
    }

    #[test]
    fn collection_backpressure_and_replaceable_update() {
        let snapshot = CollectionSnapshot {
            surface_generation: 1,
            collection_id: "status".into(),
            rev: 1,
            items: vec![CollectionItem {
                id: 1,
                replaceable: true,
                text: "running".into(),
            }],
        };
        let replace = CollectionPatch {
            surface_generation: 1,
            collection_id: "status".into(),
            base: 1,
            next: 2,
            kind: CollectionPatchKind::Replace,
            items: vec![CollectionItem {
                id: 1,
                replaceable: true,
                text: "passed".into(),
            }],
        };
        let applied = apply_collection_patch(Some(&snapshot), replace).unwrap();
        assert_eq!(applied.items[0].text, "passed");
        assert_eq!(applied.items.len(), 1);

        let mut full = snapshot.clone();
        full.items = (1..=DEFAULT_LIMIT_COLLECTION_ITEMS)
            .map(|id| CollectionItem {
                id,
                replaceable: false,
                text: "x".into(),
            })
            .collect();
        let overflow = CollectionPatch {
            surface_generation: 1,
            collection_id: "status".into(),
            base: 1,
            next: 2,
            kind: CollectionPatchKind::Append,
            items: vec![CollectionItem {
                id: 9,
                replaceable: false,
                text: "y".into(),
            }],
        };
        assert_eq!(
            apply_collection_patch(Some(&full), overflow),
            Err(CollectionRejectReason::Backpressure)
        );
    }

    #[test]
    fn workspace_rejects_unknown_cycle_and_node_overflow() {
        assert_eq!(
            decode_body("Prismattyc;workspace;snapshot;generation=1;rev=1;min=5;preferred=5;max=5;nodes=1,0,future,5,5,1,0,0,x"),
            Err(DecodeError::InvalidField("nodes"))
        );

        let mut cyclic = workspace_snapshot();
        cyclic.nodes[0].parent = 2;
        assert_eq!(
            validate_workspace_snapshot(&cyclic),
            Err(DecodeError::InvalidField("nodes"))
        );

        let mut over = workspace_snapshot();
        over.nodes = (1..=DEFAULT_LIMIT_NODES + 1)
            .map(|id| TreeNode {
                id,
                parent: if id == 1 { 0 } else { 1 },
                kind: if id == 1 {
                    TreeNodeKind::Column
                } else {
                    TreeNodeKind::Text
                },
                min: 0,
                preferred: 0,
                fill: 0,
                show_min_cols: 0,
                show_max_cols: 0,
                action_id: 0,
                text: String::new(),
            })
            .collect();
        assert_eq!(
            validate_workspace_snapshot(&over),
            Err(DecodeError::InvalidField("nodes"))
        );
    }

    #[test]
    fn workspace_geometry_preserves_transcript_and_too_small_falls_back() {
        let rows = WorkspaceRows {
            min: 5,
            preferred: 12,
            max: 24,
        };
        assert_eq!(resolve_workspace_rows(rows, 30, 100), Some(12));
        assert_eq!(resolve_workspace_rows(rows, 13, 40), Some(5));
        assert_eq!(resolve_workspace_rows(rows, 12, 100), None);
        assert_eq!(resolve_workspace_rows(rows, 30, 39), None);
    }

    #[test]
    fn focus_query_reply_and_key_round_trip() {
        let query = encode_focus_query(3).unwrap();
        let body = std::str::from_utf8(&query[2..query.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::FocusQuery { id: 3 }
        );

        let reply = encode_focus_reply(3, FocusStatus::Granted).unwrap();
        let body = std::str::from_utf8(&reply[2..reply.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::FocusReply {
                id: 3,
                status: FocusStatus::Granted
            }
        );

        let key = encode_focus_key(3, "C-S-;").unwrap();
        let body = std::str::from_utf8(&key[2..key.len() - 2]).unwrap();
        assert!(body.contains("%3B"), "semicolon must be percent-escaped");
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::FocusKey {
                id: 3,
                key: "C-S-;".into()
            }
        );
    }

    #[test]
    fn focus_key_rejects_empty_and_oversized() {
        assert!(encode_focus_key(1, "").is_err());
        assert!(encode_focus_key(1, &"x".repeat(MAX_FOCUS_KEY_BYTES + 1)).is_err());
        assert!(encode_focus_query(0).is_err());
        assert!(matches!(
            decode_body("Prismattyc;focus;k;id=1;k="),
            Err(DecodeError::InvalidField("k"))
        ));
    }

    #[test]
    fn update_and_viewport_round_trip_plain_and_styled() {
        let update = UpdateAttachment {
            id: 4,
            text: "hello".into(),
            runs: Vec::new(),
        };
        let bytes = encode_update(&update).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(decode_body(body).unwrap(), ControlMessage::Update(update));

        let styled = UpdateAttachment {
            id: 5,
            text: String::new(),
            runs: vec![
                StyledRun {
                    text: "Hi".into(),
                    fg: Some(7),
                    bg: None,
                    bold: true,
                    italic: false,
                    underline: false,
                    inverse: false,
                },
                StyledRun {
                    text: "there".into(),
                    fg: Some(1),
                    bg: Some(0),
                    bold: false,
                    italic: true,
                    underline: false,
                    inverse: false,
                },
            ],
        };
        let bytes = encode_update(&styled).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        match decode_body(body).unwrap() {
            ControlMessage::Update(got) => {
                assert_eq!(got.id, 5);
                assert_eq!(got.runs.len(), 2);
                assert_eq!(got.runs[0].text, "Hi");
                assert!(got.runs[0].bold);
                assert_eq!(got.runs[1].fg, Some(1));
                assert_eq!(got.text, "Hithere");
            }
            other => panic!("{other:?}"),
        }

        let viewport = AttachViewport {
            id: 8,
            row: 1,
            col: 2,
            rows: 1,
            cols: 8,
            text: "overlay".into(),
            runs: Vec::new(),
        };
        let bytes = encode_attach_viewport(&viewport).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert_eq!(
            decode_body(body).unwrap(),
            ControlMessage::AttachViewport(viewport)
        );
    }

    fn plain_run(text: &str) -> StyledRun {
        StyledRun {
            text: text.into(),
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }

    #[test]
    fn styled_run_text_percent_escapes_delimiters() {
        let special = "%;=/,+";
        let update = UpdateAttachment {
            id: 1,
            text: String::new(),
            runs: vec![plain_run(special)],
        };
        let bytes = encode_update(&update).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        assert!(body.contains("t:%25%3B%3D%2F%2C%2B"), "{body}");
        match decode_body(body).unwrap() {
            ControlMessage::Update(got) => {
                assert_eq!(got.runs.len(), 1);
                assert_eq!(got.runs[0].text, special);
                assert_eq!(got.text, special);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn styled_run_text_rejects_bad_percent_sequences() {
        for body in [
            "Prismattyc;update;id=1;runs=t:hello%",
            "Prismattyc;update;id=1;runs=t:hello%2",
            "Prismattyc;update;id=1;runs=t:hello%zz",
            "Prismattyc;update;id=1;runs=t:hello%00",
        ] {
            assert_eq!(
                decode_body(body),
                Err(DecodeError::InvalidField("runs")),
                "{body}"
            );
        }
    }

    #[test]
    fn styled_run_length_limit_uses_decoded_text() {
        // 5 decoded `;` exceed cols=4; escaped wire is longer still.
        let over = AttachStyled {
            kind: RegionKind::CellRect,
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            runs: vec![plain_run(";;;;;")],
        };
        assert_eq!(
            encode_attach_styled(&over),
            Err(DecodeError::InvalidField("text"))
        );
        let fits = AttachStyled {
            kind: RegionKind::CellRect,
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 4,
            runs: vec![plain_run(";;;;")],
        };
        let bytes = encode_attach_styled(&fits).unwrap();
        let body = std::str::from_utf8(&bytes[2..bytes.len() - 2]).unwrap();
        match decode_body(body).unwrap() {
            ControlMessage::AttachStyled(got) => assert_eq!(got.runs[0].text, ";;;;"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_text_field_still_rejects_semicolons() {
        let attach = AttachCellRect {
            id: 1,
            row: 0,
            col: 0,
            rows: 1,
            cols: 8,
            text: "a;b".into(),
        };
        assert_eq!(
            encode_attach_cell_rect(&attach),
            Err(DecodeError::InvalidField("text"))
        );
    }

    #[test]
    fn fuzz_corpus_adversarial_inputs_do_not_panic() {
        let seeds = [
            "",
            "Prismattyc",
            "Prismattyc;",
            "Prismattyc;update",
            "Prismattyc;update;id=1",
            "Prismattyc;update;id=0;text=x",
            "Prismattyc;update;id=1;id=2;text=x",
            "Prismattyc;attach;viewport;id=1;row=0;col=0;rows=1;cols=1;text=x;text=y",
            "Prismattyc;attach;viewport;id=1;row=0;col=0;rows=0;cols=1;text=x",
            "Prismattyc;attach;cell_rect;id=1;row=0;col=0;rows=1;cols=1;runs=",
            "Prismattyc;attach;cell_rect;id=1;row=0;col=0;rows=1;cols=4;runs=t:Hi+b:2",
            "Prismattyc;cap;r;id=1;v=0.2;limit.regions=64;limit.regions=8",
            "Prismattyc;cap;r;id=1;v=0.2;limit.regions=nope",
            &format!("Prismattyc;cap;q;id=1;max=0.1;pad={}", "x".repeat(5000)),
            "Prismattyc;cap;q;id=1;max=0.1\x7f",
        ];
        for body in seeds {
            let _ = decode_body(body);
        }
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..256 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let len = (rng % 96) as usize;
            let mut body = String::from("Prismattyc;");
            for i in 0..len {
                let b = 0x20 + ((rng.wrapping_add(i as u64 * 17) % 95) as u8);
                body.push(b as char);
            }
            let _ = decode_body(&body);
        }
    }

    #[test]
    fn tmux_passthrough_doubles_esc_and_round_trips() {
        let query = encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap();
        assert!(query.starts_with(APC_INTRODUCER));
        let wrapped = encode_tmux_passthrough(&query);
        assert!(wrapped.starts_with(b"\x1bPtmux;"));
        assert!(wrapped.ends_with(STRING_TERMINATOR));
        assert!(
            wrapped.windows(3).any(|w| w == b"\x1b\x1b_"),
            "APC ESC must be doubled: {wrapped:?}"
        );
        assert_eq!(
            decode_tmux_passthrough(&wrapped).as_deref(),
            Some(query.as_slice())
        );
        assert_eq!(decode_tmux_passthrough(b"not-dcs"), None);
    }
}
