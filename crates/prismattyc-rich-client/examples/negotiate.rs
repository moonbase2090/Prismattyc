//! Public integration example: classify 0.1/0.2 replies without host imports.

use prismattyc_protocol::{encode_capability_reply, CapabilityQuery, ProtocolVersion, RequestId};
use prismattyc_rich_client::{
    encode_default_query, session_from_body, session_from_message, ClassicReason, Session,
};

fn main() {
    let query = encode_default_query().expect("default 0.2 query");
    assert!(query.starts_with(b"\x1b_Prismattyc;cap;q;"));
    println!("query_bytes={}", query.len());

    assert!(matches!(
        session_from_body(None),
        Session::Classic {
            reason: ClassicReason::Timeout
        }
    ));
    assert!(matches!(
        session_from_body(Some("junk")),
        Session::Classic {
            reason: ClassicReason::Malformed
        }
    ));

    let grant = encode_capability_reply(
        &prismattyc_protocol::CapabilityReply::for_v1_query(CapabilityQuery {
            request_id: RequestId::new(1).unwrap(),
            max_version: ProtocolVersion::new(0, 2),
        })
        .unwrap(),
    )
    .unwrap();
    let body = std::str::from_utf8(&grant[2..grant.len() - 2]).unwrap();
    let session = session_from_body(Some(body));
    assert!(session.is_rich());
    println!("grant=rich");
    let _ = session_from_message;
}
