use super::*;

fn decode_frame(frame: Vec<u8>) -> Result<ControlMessage, DecodeError> {
    assert!(frame.starts_with(APC_INTRODUCER));
    assert!(frame.ends_with(STRING_TERMINATOR));
    decode_body(std::str::from_utf8(&frame[2..frame.len() - 2]).unwrap())
}

#[test]
fn collection_control_replies_preserve_identity_revision_and_reason() {
    let id = "diagnostics";
    assert_eq!(
        decode_frame(encode_collection_ack(7, id, 12).unwrap()).unwrap(),
        ControlMessage::CollectionAck {
            surface_generation: 7,
            collection_id: id.into(),
            rev: 12
        }
    );
    assert_eq!(
        decode_frame(encode_collection_drop(7, id).unwrap()).unwrap(),
        ControlMessage::CollectionDrop {
            surface_generation: 7,
            collection_id: id.into()
        }
    );
    assert_eq!(
        decode_frame(encode_collection_resnapshot(7, id).unwrap()).unwrap(),
        ControlMessage::CollectionResnapshot {
            surface_generation: 7,
            collection_id: id.into()
        }
    );
    for reason in [
        CollectionRejectReason::Gap,
        CollectionRejectReason::Stale,
        CollectionRejectReason::Conflict,
        CollectionRejectReason::Backpressure,
    ] {
        assert_eq!(
            decode_frame(encode_collection_reject(7, id, reason).unwrap()).unwrap(),
            ControlMessage::CollectionReject {
                surface_generation: 7,
                collection_id: id.into(),
                reason
            }
        );
    }
    for (suffix, error) in [
        (
            "reject;generation=7;id=diagnostics",
            DecodeError::MissingField("reason"),
        ),
        (
            "reject;generation=7;id=diagnostics;reason=unknown",
            DecodeError::InvalidField("reason"),
        ),
        (
            "ack;generation=7;id=diagnostics;rev=0",
            DecodeError::InvalidField("revision"),
        ),
        (
            "drop;generation=0;id=diagnostics",
            DecodeError::InvalidField("generation"),
        ),
        ("drop;generation=7", DecodeError::MissingField("id")),
        (
            "unknown;generation=7;id=diagnostics",
            DecodeError::UnknownFamily,
        ),
    ] {
        assert_eq!(
            decode_body(&format!("Prismattyc;collection;{suffix}")),
            Err(error)
        );
    }
}

#[test]
fn collection_revision_exhaustion_is_rejected_without_wrapping() {
    let patch = CollectionPatch {
        surface_generation: 1,
        collection_id: "diagnostics".into(),
        base: u64::MAX,
        next: 0,
        kind: CollectionPatchKind::Append,
        items: vec![CollectionItem {
            id: 1,
            replaceable: false,
            text: "message".into(),
        }],
    };
    assert_eq!(
        encode_collection_patch(&patch),
        Err(DecodeError::InvalidField("revision"))
    );
    assert_eq!(
        apply_collection_patch(None, patch),
        Err(CollectionRejectReason::Conflict)
    );
    let items = encode_collection_items(&[CollectionItem {
        id: 1,
        replaceable: false,
        text: "message".into(),
    }])
    .unwrap();
    let body = format!(
        "Prismattyc;collection;append;generation=1;id=diagnostics;base={};next=0;items={items}",
        u64::MAX
    );
    assert_eq!(
        decode_body(&body),
        Err(DecodeError::InvalidField("revision"))
    );
}

#[test]
fn decode_errors_identify_the_problem_and_field() {
    for (error, expected) in [
        (DecodeError::Empty, "empty control body"),
        (
            DecodeError::NonPrintableAscii,
            "non-printable ASCII in control body",
        ),
        (
            DecodeError::BadNamespace,
            "missing or wrong Prismattyc namespace",
        ),
        (
            DecodeError::UnknownFamily,
            "unknown Prismattyc message family",
        ),
        (
            DecodeError::MissingField("rev"),
            "missing required field rev",
        ),
        (
            DecodeError::DuplicateField("rev"),
            "duplicate required field rev",
        ),
        (DecodeError::InvalidField("rev"), "invalid field rev"),
        (DecodeError::ZeroId, "id must be non-zero"),
    ] {
        assert_eq!(error.to_string(), expected);
    }
    assert_eq!(
        DecodeError::Oversized.to_string(),
        format!("control body exceeds {MAX_CONTROL_BODY_BYTES} bytes")
    );
}
