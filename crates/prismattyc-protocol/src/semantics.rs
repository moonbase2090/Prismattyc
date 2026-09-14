//! Logical semantic documents, ranges, and decoration-free projection.

use super::{parse_fields, DecodeError, MAX_CONTROL_BODY_BYTES, NAMESPACE};

pub const MAX_SEMANTIC_TEXT_CHARS: usize = 4096;
pub const MAX_SEMANTIC_SPANS: usize = 64;
pub const MAX_DOCUMENT_ID_BYTES: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticRole {
    Heading,
    Label,
    Value,
    Status,
    Code,
    Severity,
    Location,
}

impl SemanticRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heading => "heading",
            Self::Label => "label",
            Self::Value => "value",
            Self::Status => "status",
            Self::Code => "code",
            Self::Severity => "severity",
            Self::Location => "location",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "heading" => Some(Self::Heading),
            "label" => Some(Self::Label),
            "value" => Some(Self::Value),
            "status" => Some(Self::Status),
            "code" => Some(Self::Code),
            "severity" => Some(Self::Severity),
            "location" => Some(Self::Location),
            _ => None,
        }
    }
}

/// Inclusive-start exclusive-end range in Unicode scalar offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticSpan {
    pub start: u32,
    pub end: u32,
    pub role: SemanticRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDocument {
    pub surface_generation: u64,
    pub document_id: String,
    pub rev: u64,
    pub text: String,
    pub spans: Vec<SemanticSpan>,
    /// Current logical selection, if the producer advertised one.
    pub selection: Option<SemanticRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticRange {
    pub rev: u64,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticError {
    Stale,
    Bounds,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCopy {
    pub surface_generation: u64,
    pub document_id: String,
    pub rev: u64,
    pub start: u32,
    pub end: u32,
}

pub fn validate_document_id(id: &str) -> Result<(), DecodeError> {
    if id.is_empty() || id.len() > MAX_DOCUMENT_ID_BYTES {
        return Err(DecodeError::InvalidField("id"));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DecodeError::InvalidField("id"));
    }
    Ok(())
}

/// Install a snapshot. Same revision + same payload is a no-op. Older or
/// conflicting same-rev payloads are stale.
pub fn apply_semantic_snapshot(
    current: Option<&SemanticDocument>,
    incoming: SemanticDocument,
) -> Result<Option<SemanticDocument>, SemanticError> {
    if incoming.surface_generation == 0 || incoming.rev == 0 {
        return Err(SemanticError::Conflict);
    }
    match current {
        Some(existing) if existing.surface_generation != incoming.surface_generation => {
            Err(SemanticError::Stale)
        }
        Some(existing) if incoming.rev < existing.rev => Err(SemanticError::Stale),
        Some(existing) if incoming.rev == existing.rev && existing == &incoming => Ok(None),
        Some(existing) if incoming.rev == existing.rev => Err(SemanticError::Conflict),
        _ => Ok(Some(incoming)),
    }
}

impl SemanticDocument {
    pub fn char_len(&self) -> u32 {
        u32::try_from(self.text.chars().count()).unwrap_or(u32::MAX)
    }

    /// Copy a logical range. Stale revisions and inverted/out-of-range ends fail.
    pub fn project(&self, range: SemanticRange) -> Result<String, SemanticError> {
        if range.rev != self.rev {
            return Err(SemanticError::Stale);
        }
        if range.start > range.end || range.end > self.char_len() {
            return Err(SemanticError::Bounds);
        }
        let start = usize::try_from(range.start).unwrap_or(usize::MAX);
        let end = usize::try_from(range.end).unwrap_or(usize::MAX);
        Ok(self
            .text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect())
    }

    pub fn span_text(&self, span: SemanticSpan) -> Result<String, SemanticError> {
        self.project(SemanticRange {
            rev: self.rev,
            start: span.start,
            end: span.end,
        })
    }

    /// Copy a wire request. Generation, document id, and revision must match.
    pub fn project_copy(&self, copy: &SemanticCopy) -> Result<String, SemanticError> {
        if copy.surface_generation != self.surface_generation
            || copy.document_id != self.document_id
        {
            return Err(SemanticError::Stale);
        }
        self.project(SemanticRange {
            rev: copy.rev,
            start: copy.start,
            end: copy.end,
        })
    }
}

pub fn encode_semantic_snapshot(doc: &SemanticDocument) -> Result<Vec<u8>, DecodeError> {
    if doc.surface_generation == 0 || doc.rev == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    validate_document_id(&doc.document_id)?;
    if doc.text.chars().count() > MAX_SEMANTIC_TEXT_CHARS || doc.spans.len() > MAX_SEMANTIC_SPANS {
        return Err(DecodeError::Oversized);
    }
    for span in &doc.spans {
        if span.start > span.end || span.end > doc.char_len() {
            return Err(DecodeError::InvalidField("spans"));
        }
    }
    if let Some(selection) = doc.selection {
        if selection.rev != doc.rev
            || selection.start > selection.end
            || selection.end > doc.char_len()
        {
            return Err(DecodeError::InvalidField("sel"));
        }
    }
    let text = escape_semantic_text(&doc.text)?;
    let mut spans = String::new();
    for (index, span) in doc.spans.iter().enumerate() {
        if index > 0 {
            spans.push('/');
        }
        spans.push_str(&format!(
            "{},{},{}",
            span.start,
            span.end,
            span.role.as_str()
        ));
    }
    let mut body = format!(
        "{NAMESPACE};semantics;snapshot;generation={};id={};rev={};text={text};spans={spans}",
        doc.surface_generation, doc.document_id, doc.rev
    );
    if let Some(selection) = doc.selection {
        body.push_str(&format!(";sel={},{}", selection.start, selection.end));
    }
    if body.len() > MAX_CONTROL_BODY_BYTES {
        return Err(DecodeError::Oversized);
    }
    super::encode_apc(&body)
}

pub fn decode_semantics<'a>(
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<super::ControlMessage, DecodeError> {
    let kind = parts.next().ok_or(DecodeError::UnknownFamily)?;
    let fields = parse_fields(parts)?;
    match kind {
        "snapshot" => {
            let surface_generation = super::require_u64(&fields, "generation")?;
            let document_id = fields
                .get("id")
                .copied()
                .ok_or(DecodeError::MissingField("id"))?
                .to_string();
            validate_document_id(&document_id)?;
            let rev = super::require_u64(&fields, "rev")?;
            let text = unescape_semantic_text(
                fields
                    .get("text")
                    .copied()
                    .ok_or(DecodeError::MissingField("text"))?,
            )?;
            if text.chars().count() > MAX_SEMANTIC_TEXT_CHARS {
                return Err(DecodeError::Oversized);
            }
            let spans = decode_spans(
                fields.get("spans").copied().unwrap_or(""),
                text.chars().count(),
            )?;
            let char_len = text.chars().count();
            let selection = match fields.get("sel").copied() {
                Some(raw) => Some(decode_selection(raw, rev, char_len)?),
                None => None,
            };
            Ok(super::ControlMessage::SemanticSnapshot(SemanticDocument {
                surface_generation,
                document_id,
                rev,
                text,
                spans,
                selection,
            }))
        }
        "copy" => {
            let document_id = fields
                .get("id")
                .copied()
                .ok_or(DecodeError::MissingField("id"))?
                .to_string();
            validate_document_id(&document_id)?;
            Ok(super::ControlMessage::SemanticCopy(SemanticCopy {
                surface_generation: super::require_u64(&fields, "generation")?,
                document_id,
                rev: super::require_u64(&fields, "rev")?,
                start: require_u32_field(&fields, "start")?,
                end: require_u32_field(&fields, "end")?,
            }))
        }
        _ => Err(DecodeError::UnknownFamily),
    }
}

pub fn encode_semantic_copy(copy: &SemanticCopy) -> Result<Vec<u8>, DecodeError> {
    if copy.surface_generation == 0 || copy.rev == 0 {
        return Err(DecodeError::InvalidField("generation"));
    }
    validate_document_id(&copy.document_id)?;
    let body = format!(
        "{NAMESPACE};semantics;copy;generation={};id={};rev={};start={};end={}",
        copy.surface_generation, copy.document_id, copy.rev, copy.start, copy.end
    );
    super::encode_apc(&body)
}

fn require_u32_field(
    fields: &std::collections::BTreeMap<&str, &str>,
    key: &'static str,
) -> Result<u32, DecodeError> {
    let raw = fields.get(key).ok_or(DecodeError::MissingField(key))?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DecodeError::InvalidField(key));
    }
    raw.parse().map_err(|_| DecodeError::InvalidField(key))
}

fn decode_selection(raw: &str, rev: u64, char_len: usize) -> Result<SemanticRange, DecodeError> {
    let mut bits = raw.split(',');
    let start = bits
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or(DecodeError::InvalidField("sel"))?;
    let end = bits
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or(DecodeError::InvalidField("sel"))?;
    if bits.next().is_some() || start > end || usize::try_from(end).unwrap_or(usize::MAX) > char_len
    {
        return Err(DecodeError::InvalidField("sel"));
    }
    Ok(SemanticRange { rev, start, end })
}

fn decode_spans(raw: &str, char_len: usize) -> Result<Vec<SemanticSpan>, DecodeError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut spans = Vec::new();
    for part in raw.split('/') {
        if spans.len() >= MAX_SEMANTIC_SPANS {
            return Err(DecodeError::Oversized);
        }
        let mut bits = part.split(',');
        let start = bits
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(DecodeError::InvalidField("spans"))?;
        let end = bits
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(DecodeError::InvalidField("spans"))?;
        let role = bits
            .next()
            .and_then(SemanticRole::parse)
            .ok_or(DecodeError::InvalidField("spans"))?;
        if bits.next().is_some()
            || start > end
            || usize::try_from(end).unwrap_or(usize::MAX) > char_len
        {
            return Err(DecodeError::InvalidField("spans"));
        }
        spans.push(SemanticSpan { start, end, role });
    }
    Ok(spans)
}

fn escape_semantic_text(text: &str) -> Result<String, DecodeError> {
    let mut out = String::new();
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

fn unescape_semantic_text(wire: &str) -> Result<String, DecodeError> {
    let bytes = wire.as_bytes();
    if bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte)) {
        return Err(DecodeError::NonPrintableAscii);
    }
    let mut raw = Vec::with_capacity(wire.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b';' | b'=' | b'/' | b'+' | b',' => {
                return Err(DecodeError::InvalidField("text"));
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(DecodeError::InvalidField("text"));
                }
                let hi = from_hex(bytes[index + 1]).ok_or(DecodeError::InvalidField("text"))?;
                let lo = from_hex(bytes[index + 2]).ok_or(DecodeError::InvalidField("text"))?;
                raw.push((hi << 4) | lo);
                index += 3;
            }
            other => {
                raw.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8(raw).map_err(|_| DecodeError::InvalidField("text"))
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_body, ControlMessage};

    fn loc_range(text: &str) -> (u32, u32) {
        let start = text.find("crates/").unwrap();
        let end = start + "crates/prismattyc-core/src/lib.rs:142:5".len();
        (
            text[..start].chars().count() as u32,
            text[..end].chars().count() as u32,
        )
    }

    fn doc() -> SemanticDocument {
        let text = "error crates/prismattyc-core/src/lib.rs:142:5 wide 日本語 wrap".to_string();
        let (loc_start, loc_end) = loc_range(&text);
        SemanticDocument {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 3,
            text,
            spans: vec![
                SemanticSpan {
                    start: 0,
                    end: 5,
                    role: SemanticRole::Severity,
                },
                SemanticSpan {
                    start: loc_start,
                    end: loc_end,
                    role: SemanticRole::Location,
                },
            ],
            selection: None,
        }
    }

    #[test]
    fn projection_is_plain_and_rejects_stale_or_inverted_ranges() {
        let doc = doc();
        let loc = doc.spans[1];
        assert_eq!(
            doc.project(SemanticRange {
                rev: 3,
                start: loc.start,
                end: loc.end
            })
            .unwrap(),
            "crates/prismattyc-core/src/lib.rs:142:5"
        );
        assert_eq!(
            doc.project(SemanticRange {
                rev: 2,
                start: 0,
                end: 5
            }),
            Err(SemanticError::Stale)
        );
        assert_eq!(
            doc.project(SemanticRange {
                rev: 3,
                start: 8,
                end: 2
            }),
            Err(SemanticError::Bounds)
        );
    }

    #[test]
    fn wide_characters_use_scalar_offsets_and_survive_the_wire() {
        let doc = doc();
        let jp = doc.text.find('日').unwrap();
        let start = doc.text[..jp].chars().count() as u32;
        assert_eq!(
            doc.project(SemanticRange {
                rev: 3,
                start,
                end: start + 3
            })
            .unwrap(),
            "日本語"
        );
        let encoded = encode_semantic_snapshot(&doc).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        let ControlMessage::SemanticSnapshot(round) = decode_body(body).unwrap() else {
            panic!("expected snapshot");
        };
        assert_eq!(round.text, doc.text);
        assert_eq!(round.spans, doc.spans);
    }

    #[test]
    fn mixed_styles_and_filtered_rows_keep_logical_order() {
        let mut doc = doc();
        doc.text = "error boom\nfailed later\ncheck: crates/x.rs:1:1".into();
        doc.spans = vec![
            SemanticSpan {
                start: 0,
                end: 5,
                role: SemanticRole::Severity,
            },
            SemanticSpan {
                start: 11,
                end: 17,
                role: SemanticRole::Severity,
            },
            SemanticSpan {
                start: 31,
                end: 46,
                role: SemanticRole::Location,
            },
        ];
        // Cross-run: first severity plus later location.
        assert_eq!(
            doc.project(SemanticRange {
                rev: 3,
                start: 0,
                end: 5
            })
            .unwrap(),
            "error"
        );
        assert_eq!(doc.span_text(doc.spans[2]).unwrap(), "crates/x.rs:1:1");
    }

    #[test]
    fn document_id_bounds_are_identical_on_encode_and_decode() {
        let mut doc = doc();
        doc.document_id = "diag;alternate".into();
        assert!(encode_semantic_snapshot(&doc).is_err());
        doc.document_id = "x".repeat(49);
        assert!(encode_semantic_snapshot(&doc).is_err());
        doc.document_id = "diag".into();
        let encoded = encode_semantic_snapshot(&doc).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        let ControlMessage::SemanticSnapshot(round) = decode_body(body).unwrap() else {
            panic!("expected snapshot");
        };
        assert_eq!(round.document_id, "diag");
    }

    #[test]
    fn apply_rejects_older_and_conflicting_same_revision() {
        let first = doc();
        let mut same = first.clone();
        assert_eq!(
            apply_semantic_snapshot(Some(&first), same.clone()),
            Ok(None)
        );
        same.text.push('!');
        assert_eq!(
            apply_semantic_snapshot(Some(&first), same),
            Err(SemanticError::Conflict)
        );
        let mut older = first.clone();
        older.rev = 2;
        assert_eq!(
            apply_semantic_snapshot(Some(&first), older),
            Err(SemanticError::Stale)
        );
        let mut newer = first.clone();
        newer.rev = 4;
        assert!(apply_semantic_snapshot(Some(&first), newer)
            .unwrap()
            .is_some());
    }

    #[test]
    fn project_copy_rejects_foreign_generation_or_document() {
        let doc = doc();
        let loc = doc.spans[1];
        let ok = SemanticCopy {
            surface_generation: 1,
            document_id: "diag".into(),
            rev: 3,
            start: loc.start,
            end: loc.end,
        };
        assert_eq!(
            doc.project_copy(&ok).unwrap(),
            "crates/prismattyc-core/src/lib.rs:142:5"
        );
        let mut stale_gen = ok.clone();
        stale_gen.surface_generation = 99;
        assert_eq!(doc.project_copy(&stale_gen), Err(SemanticError::Stale));
        let mut other_id = ok;
        other_id.document_id = "other".into();
        assert_eq!(doc.project_copy(&other_id), Err(SemanticError::Stale));
    }

    #[test]
    fn selection_survives_the_wire() {
        let mut doc = doc();
        doc.selection = Some(SemanticRange {
            rev: 3,
            start: doc.spans[1].start,
            end: doc.spans[1].end,
        });
        let encoded = encode_semantic_snapshot(&doc).unwrap();
        let body = std::str::from_utf8(&encoded[2..encoded.len() - 2]).unwrap();
        let ControlMessage::SemanticSnapshot(round) = decode_body(body).unwrap() else {
            panic!("expected snapshot");
        };
        assert_eq!(round.selection, doc.selection);
    }
}
