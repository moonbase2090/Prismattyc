use std::collections::BTreeMap;

use prismattyc_protocol::{
    encode_apc, encode_capability_query, encode_capability_reply, CapabilityQuery, CapabilityReply,
    ProtocolVersion, RequestId,
};

const COMPAT: &str = include_str!("../../../docs/fixtures/rich-surface-v2/compat-wire.tsv");
const NEXT_CAPABILITY: &str =
    include_str!("../../../docs/fixtures/rich-surface-v2/capability-v0.3.tsv");
const NEXT_STATE: &str = include_str!("../../../docs/fixtures/rich-surface-v2/state-v0.3.tsv");

fn decode_hex(raw: &str) -> Vec<u8> {
    assert_eq!(raw.len() % 2, 0, "hex fixture must contain byte pairs");
    assert!(
        raw.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "fixture must use lowercase hex"
    );
    raw.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("fixture hex is ASCII");
            u8::from_str_radix(text, 16).expect("fixture contains lowercase hex")
        })
        .collect()
}

fn compat_cases() -> BTreeMap<&'static str, Vec<u8>> {
    let id = RequestId::new(7).expect("fixture id is non-zero");
    let query_v0_1 = CapabilityQuery {
        request_id: id,
        max_version: ProtocolVersion::new(0, 1),
    };
    let query_v0_2 = CapabilityQuery {
        request_id: id,
        max_version: ProtocolVersion::new(0, 2),
    };
    BTreeMap::from([
        (
            "query_v0_1",
            encode_capability_query(query_v0_1).expect("encode 0.1 query"),
        ),
        (
            "reply_v0_1",
            encode_capability_reply(&CapabilityReply::for_v1_query(query_v0_1).expect("0.1 reply"))
                .expect("encode 0.1 reply"),
        ),
        (
            "query_v0_2",
            encode_capability_query(query_v0_2).expect("encode 0.2 query"),
        ),
        (
            "reply_v0_2",
            encode_capability_reply(&CapabilityReply::for_v1_query(query_v0_2).expect("0.2 reply"))
                .expect("encode 0.2 reply"),
        ),
    ])
}

#[test]
fn existing_capability_bytes_match_adr0014_fixture() {
    let expected = compat_cases();
    let mut seen = BTreeMap::new();
    for line in COMPAT
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (name, hex) = line.split_once('\t').expect("name and hex columns");
        assert!(seen.insert(name, ()).is_none(), "duplicate case {name}");
        assert_eq!(decode_hex(hex), expected[name], "wire drift in {name}");
    }
    assert_eq!(seen.len(), expected.len());
    assert!(expected.keys().all(|name| seen.contains_key(name)));
}

#[test]
fn next_capability_bodies_are_exact_bounded_apc_inputs() {
    let mut seen = BTreeMap::new();
    for line in NEXT_CAPABILITY
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut columns = line.split('\t');
        let name = columns.next().expect("name");
        let direction = columns.next().expect("direction");
        let body = columns.next().expect("APC body");
        assert!(columns.next().is_none(), "unexpected column in {name}");
        assert!(seen.insert(name, ()).is_none(), "duplicate case {name}");
        assert!(matches!(direction, "app_to_host" | "host_to_app"));
        let wire = encode_apc(body).expect("next capability fixture is bounded ASCII");
        assert!(wire.starts_with(b"\x1b_") && wire.ends_with(b"\x1b\\"));
    }
    assert_eq!(seen.len(), 2);
    assert!(seen.contains_key("query_v0_3"));
    assert!(seen.contains_key("reply_v0_3"));
}

#[test]
fn next_state_fixture_has_required_failure_and_lifetime_cases() {
    let required = [
        "surface_conflict_drop",
        "surface_gap_resnapshot",
        "generation_stale_drop",
        "collection_gap_isolated",
        "append_queue_full",
        "viewer_wrong_connection_drop",
        "viewer_unsolicited_projection_drop",
        "viewer_local_scroll_isolated",
        "stale_action_drop",
        "surface_drop_primary_fallback",
        "too_small_decline",
        "observer_resize_drop",
        "controller_transfer_resize",
        "runbook_crash_reap",
    ];
    let mut seen = BTreeMap::new();
    for line in NEXT_STATE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 6, "state fixture must have six columns");
        assert!(
            seen.insert(columns[0], ()).is_none(),
            "duplicate state case {}",
            columns[0]
        );
        assert!(columns.iter().all(|column| !column.is_empty()));
    }
    for name in required {
        assert!(seen.contains_key(name), "missing state fixture {name}");
    }
}
