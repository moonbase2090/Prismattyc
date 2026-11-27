//! Dependency-light Prismattyc control protocol types and bounded wire codecs.
//!
//! What is left after rich mode was removed: the capability query a native
//! session answers, streaming APC body collection for the emulator, viewer
//! ids, the tmux passthrough wrapper, graphics APC and the native grid wire.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

mod graphics_apc;
pub mod native_grid;
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

/// Semantic capability request before wire encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityQuery {
    pub request_id: RequestId,
    pub max_version: ProtocolVersion,
}

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
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
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

/// A decoded Prismattyc control message. Only the capability query is
/// still read; any other family is rejected as unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    CapabilityQuery(CapabilityQuery),
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
    match parts.next() {
        Some("cap") => decode_capability(parts),
        _ => Err(DecodeError::UnknownFamily),
    }
}

fn decode_capability<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<ControlMessage, DecodeError> {
    if parts.next() != Some("q") {
        return Err(DecodeError::UnknownFamily);
    }
    let fields = parse_fields(parts)?;
    let id = require_u32(&fields, "id")?;
    let max = require_version(&fields, "max")?;
    let request_id = RequestId::new(id).ok_or(DecodeError::ZeroId)?;
    Ok(ControlMessage::CapabilityQuery(CapabilityQuery {
        request_id,
        max_version: max,
    }))
}

fn duplicate_field(key: &str) -> DecodeError {
    match key {
        "id" => DecodeError::DuplicateField("id"),
        "max" => DecodeError::DuplicateField("max"),
        _ => DecodeError::InvalidField("duplicate"),
    }
}

fn parse_fields<'a>(
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
    fn rejects_non_printable_and_oversized_bodies() {
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=1;max=0.1\n"),
            Err(DecodeError::NonPrintableAscii)
        );
        let huge = format!("Prismattyc;cap;q;id=1;max=0.1;pad={}", "x".repeat(4100));
        assert_eq!(decode_body(&huge), Err(DecodeError::Oversized));
    }

    #[test]
    fn rejects_zero_and_duplicate_ids() {
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=0;max=0.1"),
            Err(DecodeError::ZeroId)
        );
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=1;id=2;max=0.1"),
            Err(DecodeError::DuplicateField("id"))
        );
        assert_eq!(
            decode_body("Prismattyc;cap;q;id=1;max=0.1;max=0.2"),
            Err(DecodeError::DuplicateField("max"))
        );
    }

    #[test]
    fn removed_rich_families_and_replies_are_unknown() {
        // A child still sending rich messages gets nothing decoded: the
        // native host only answers capability queries.
        for body in [
            "Prismattyc;cap;r;id=1;v=0.3;features=hybrid.attach.cell_rect",
            "Prismattyc;attach;cell_rect;id=1;row=0;col=0;rows=1;cols=4;text=STAT",
            "Prismattyc;update;id=1;text=x",
            "Prismattyc;detach;id=1",
            "Prismattyc;focus;q;id=1",
            "Prismattyc;workspace;drop;gen=1",
            "Prismattyc;collection;drop;gen=1;collection=c",
            "Prismattyc;status;drop;gen=1",
            "Prismattyc;input;id=1",
            "Prismattyc;semantics;copy;id=1",
        ] {
            assert_eq!(
                decode_body(body),
                Err(DecodeError::UnknownFamily),
                "{body:?}"
            );
        }
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
    fn apc_collector_recovers_after_invalid_and_nested_sequences() {
        // Malformed controls must drain through ST without becoming a command.
        // A nested introducer starts a fresh command, including after overflow.
        for bad in [b"a\x00b".as_slice(), b"a\x1bXb", b"a\x1b\x00b"] {
            let mut collector = ApcCollector::new();
            assert!(collector.push(b"\x1b_").is_empty());
            for byte in bad {
                assert!(collector.push(&[*byte]).is_empty());
            }
            assert_eq!(collector.push(b"\x1b\\"), vec![CollectedApc::Discarded]);
            assert!(!collector.is_active());
            assert_eq!(
                collector.push(b"\x1b_ok\x1b\\"),
                vec![CollectedApc::Body("ok".into())]
            );
        }
        let mut collector = ApcCollector::new();
        assert!(collector.push(b"\x1b[plain\x1b_").is_empty());
        assert!(collector
            .push(&vec![b'x'; MAX_CONTROL_BODY_BYTES + 1])
            .is_empty());
        assert!(collector.push(b"\x1b_").is_empty());
        assert_eq!(
            collector.push(b"fresh\x1b\x1b\\"),
            vec![CollectedApc::Body("fresh".into())]
        );
        assert!(!collector.is_active());
    }

    #[test]
    fn adversarial_inputs_do_not_panic() {
        let seeds = [
            ("", Some(DecodeError::Empty)),
            ("Prismattyc", Some(DecodeError::UnknownFamily)),
            ("Prismattyc;", Some(DecodeError::UnknownFamily)),
            ("Prismattyc;cap", Some(DecodeError::UnknownFamily)),
            ("Prismattyc;cap;q", Some(DecodeError::MissingField("id"))),
            (
                "Prismattyc;cap;q;id=1",
                Some(DecodeError::MissingField("max")),
            ),
            (
                "Prismattyc;cap;q;id=x;max=0.1",
                Some(DecodeError::InvalidField("id")),
            ),
            (
                "Prismattyc;cap;q;id=1;max=1",
                Some(DecodeError::InvalidField("max")),
            ),
            (
                "Prismattyc;cap;q;=1;id=1;max=0.1",
                Some(DecodeError::InvalidField("key")),
            ),
            (
                "prismattyc;cap;q;id=1;max=0.1",
                Some(DecodeError::BadNamespace),
            ),
            (
                &format!("Prismattyc;cap;q;id=1;max=0.1;pad={}", "x".repeat(5000)),
                Some(DecodeError::Oversized),
            ),
            (
                "Prismattyc;cap;q;id=1;max=0.1",
                Some(DecodeError::NonPrintableAscii),
            ),
            ("Prismattyc;cap;q;id=1;max=0.4;extra=1;bare", None),
        ];
        for (body, expected_error) in seeds {
            match expected_error {
                Some(error) => assert_eq!(decode_body(body), Err(error), "{body:?}"),
                None => assert!(decode_body(body).is_ok(), "expected a valid body: {body:?}"),
            }
        }
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for seed_index in 0..256 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let len = (rng % 96) as usize;
            let mut body = String::from("Prismattyc;");
            for i in 0..len {
                let b = 0x20 + ((rng.wrapping_add(i as u64 * 17) % 95) as u8);
                body.push(b as char);
            }
            assert!(
                decode_body(&body).is_err(),
                "generated adversarial seed {seed_index} unexpectedly decoded: {body:?}"
            );
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
