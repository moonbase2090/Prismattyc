// SPDX-License-Identifier: MIT
//! Capability negotiation for experimental native sessions.
//!
//! A child asks what the host supports with an APC capability query, and a
//! native session answers it on the pane's PTY writer. The writer messages,
//! the grant applied after a reply is written, and the collector-ordered
//! feeding of child output live here.

use std::sync::mpsc;

use prismattyc_emulator::Emulator;
#[cfg(any(test, native_transport))]
use prismattyc_protocol::CapabilityQuery;

/// The experimental features a pane was started with. Off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Experimental {
    /// Native terminal sessions and images for direct local PTY children
    /// (`--experimental-native`).
    pub native: bool,
}

impl Experimental {
    /// Whether the pane's emulator collects APC bodies, where native
    /// sessions read capability queries.
    pub(crate) const fn collects_apc(self) -> bool {
        self.native
    }
}

impl From<bool> for Experimental {
    /// A bare flag means native sessions on or off.
    fn from(native: bool) -> Self {
        Self { native }
    }
}

/// Bytes for the child PTY writer thread (keys, paste, DSR, capability replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildWrite {
    pub bytes: Vec<u8>,
    /// Applied to the pane only after a successful PTY write.
    pub capability_grant: Option<CapabilityGrant>,
    /// An `async_paste` paste: its bytes (maybe still being produced) and
    /// where the writer reports the outcome. `bytes` is empty when set.
    pub paste: Option<crate::paste_job::PasteTicket>,
}

/// What a written capability reply grants the pane. Without native sessions
/// nothing is granted, so this has no values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapabilityGrant {
    #[cfg(native_transport)]
    Native(crate::native::authority::OfferReceipt),
}

impl ChildWrite {
    pub(crate) fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            capability_grant: None,
            paste: None,
        }
    }

    pub(crate) fn paste(ticket: crate::paste_job::PasteTicket) -> Self {
        Self {
            bytes: Vec::new(),
            capability_grant: None,
            paste: Some(ticket),
        }
    }

    #[cfg(native_transport)]
    pub(crate) fn capability(bytes: Vec<u8>, grant: CapabilityGrant) -> Self {
        Self {
            bytes,
            capability_grant: Some(grant),
            paste: None,
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

/// Send the emulator's pending terminal replies (DSR, DA and the like) to
/// the child.
pub(crate) fn drain_emulator_replies(
    emulator: &mut Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
) {
    for reply in emulator.take_pending_replies() {
        let _ = to_child.try_send(ChildWrite::bytes(reply));
    }
}

/// How a [`CapabilityHost`] handled a capability query.
#[cfg(any(test, native_transport))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryReply {
    /// Not answered: the query was not for this host, or the reply could
    /// not be made.
    Pass,
    /// The host answered the query.
    Answered,
}

/// Hooks for a host that answers capability queries itself (native
/// sessions). They run in collector order: `before_bytes` before each slice
/// is parsed, `capability_reply` after a query is decoded.
#[cfg(any(test, native_transport))]
pub(crate) trait CapabilityHost {
    /// True when the bytes about to be fed revoked something the pane shows.
    fn before_bytes(&mut self, emulator: &Emulator) -> bool;
    fn capability_reply(
        &mut self,
        query: CapabilityQuery,
        emulator: &mut Emulator,
        to_child: &mpsc::SyncSender<ChildWrite>,
    ) -> QueryReply;
}

/// End of the next slice of `bytes` from `index`. Plain text runs to the next
/// ESC in one slice, so a flood cannot stall the event loop. Control-bearing
/// bytes go one at a time, so a completed APC and a grid scroll never share
/// one `feed` (the collector runs before the VT parser).
#[cfg(any(test, native_transport))]
pub(crate) fn slice_end(emulator: &Emulator, bytes: &[u8], index: usize) -> usize {
    if emulator.apc_pending() || bytes[index] == 0x1b {
        index + 1
    } else {
        bytes[index..]
            .iter()
            .position(|&b| b == 0x1b)
            .map_or(bytes.len(), |rel| index + rel)
    }
}

/// Feed child output for a pane with native sessions. Only capability
/// queries reach `host`; other APC bodies are dropped. Returns true when
/// `before_bytes` revoked something.
#[cfg(any(test, native_transport))]
pub(crate) fn process_native_chunk(
    emulator: &mut Emulator,
    to_child: &mpsc::SyncSender<ChildWrite>,
    bytes: &[u8],
    host: &mut impl CapabilityHost,
) -> bool {
    use prismattyc_protocol::{decode_body, CollectedApc, ControlMessage};
    let mut index = 0;
    let mut revoked = false;
    while index < bytes.len() {
        let end = slice_end(emulator, bytes, index);
        revoked |= host.before_bytes(emulator);
        let events = emulator.feed(&bytes[index..end]);
        drain_emulator_replies(emulator, to_child);
        for event in events {
            if let CollectedApc::Body(body) = event {
                if let Ok(ControlMessage::CapabilityQuery(query)) = decode_body(&body) {
                    host.capability_reply(query, emulator, to_child);
                }
            }
        }
        index = end;
    }
    revoked
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_protocol::{
        encode_attach_cell_rect, encode_capability_query, AttachCellRect, ProtocolVersion,
        RequestId,
    };

    #[derive(Default)]
    struct Recorder {
        queries: Vec<u32>,
        before: usize,
        revoke: bool,
    }

    impl CapabilityHost for Recorder {
        fn before_bytes(&mut self, _emulator: &Emulator) -> bool {
            self.before += 1;
            self.revoke
        }

        fn capability_reply(
            &mut self,
            query: CapabilityQuery,
            _emulator: &mut Emulator,
            to_child: &mpsc::SyncSender<ChildWrite>,
        ) -> QueryReply {
            self.queries.push(query.request_id.get());
            let _ = to_child.try_send(ChildWrite::bytes(b"reply".to_vec()));
            QueryReply::Answered
        }
    }

    fn query(id: u32, minor: u16) -> Vec<u8> {
        encode_capability_query(CapabilityQuery {
            request_id: RequestId::new(id).unwrap(),
            max_version: ProtocolVersion::new(0, minor),
        })
        .unwrap()
    }

    #[test]
    fn only_a_native_pane_collects_apc() {
        assert_eq!(Experimental::from(true), Experimental { native: true });
        assert_eq!(Experimental::from(false), Experimental::default());
        assert!(Experimental::from(true).collects_apc());
        assert!(!Experimental::default().collects_apc());
    }

    #[test]
    fn a_native_pane_hands_queries_to_its_host_and_drops_other_apc_messages() {
        let mut emulator = Emulator::new_experimental(20, 4, 100);
        let (to_child, from_host) = mpsc::sync_channel(8);
        let mut host = Recorder::default();
        let mut bytes = b"ab".to_vec();
        bytes.extend(query(7, 4));
        bytes.extend(
            encode_attach_cell_rect(&AttachCellRect {
                id: 1,
                row: 0,
                col: 0,
                rows: 1,
                cols: 4,
                text: "STAT".into(),
            })
            .unwrap(),
        );
        bytes.extend(query(9, 3));
        bytes.extend(b"cd\x1b[6n");
        let revoked = process_native_chunk(&mut emulator, &to_child, &bytes, &mut host);
        assert!(!revoked);
        assert_eq!(host.queries, [7, 9]);
        let written: Vec<Vec<u8>> = from_host.try_iter().map(|write| write.bytes).collect();
        assert_eq!(written[..2], [b"reply".to_vec(), b"reply".to_vec()]);
        // The cursor report comes after both replies, from the emulator.
        assert_eq!(written.len(), 3);
        assert!(written[2].ends_with(b"R"));
        let row: String = (0..4)
            .map(|col| emulator.screen().view_cell(0, 0, col).character)
            .collect();
        assert_eq!(row, "abcd", "APC bodies must not paint and text must");
    }

    #[test]
    fn before_bytes_runs_for_every_slice_and_reports_a_revocation() {
        let mut emulator = Emulator::new_experimental(20, 4, 100);
        let (to_child, _from_host) = mpsc::sync_channel(8);
        let mut host = Recorder {
            revoke: true,
            ..Recorder::default()
        };
        // "plain" is one slice; each byte of "\x1b[m" is its own slice.
        assert!(process_native_chunk(
            &mut emulator,
            &to_child,
            b"plain\x1b[m",
            &mut host
        ));
        assert_eq!(host.before, 4);
    }
}
