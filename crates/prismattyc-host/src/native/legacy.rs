// SPDX-License-Identifier: MIT
//! Frozen minor-0 records bridge into the same owned scene implementation.

use super::wire12::{
    Header, PtyEpoch, Record, Reply, ReplyIdentity, Request, RequestSequence, SessionId,
    UploadState,
};
use prismattyc_protocol::native_grid::{self as frozen, ControlMessage, ControlRecord};
use std::num::NonZeroU32;

#[derive(Debug)]
pub enum LegacyError {
    Codec(frozen::NativeGridError),
    Direction,
    Identity,
}

pub fn decode_hello(bytes: &[u8]) -> Result<Record, LegacyError> {
    let record = frozen::decode_control(bytes).map_err(LegacyError::Codec)?;
    let ControlMessage::Hello {
        capability_id,
        offer,
        ..
    } = record.message
    else {
        return Err(LegacyError::Direction);
    };
    if record.sequence != 1 {
        return Err(LegacyError::Identity);
    }
    Ok(Record {
        header: Header {
            session: SessionId::new(record.session_id),
            sequence: RequestSequence::new(record.sequence).ok_or(LegacyError::Identity)?,
            epoch: PtyEpoch::new(record.generation).ok_or(LegacyError::Identity)?,
        },
        request: Request::Hello {
            capability_id: NonZeroU32::new(capability_id).ok_or(LegacyError::Identity)?,
            offer,
            attributes: false,
        },
    })
}

pub fn encode(identity: ReplyIdentity, reply: Reply) -> Result<Vec<u8>, LegacyError> {
    let message = match reply {
        Reply::Setup { ring, .. } if ring.legacy_compatible() => ControlMessage::Setup {
            mapping_bytes: frozen::RING_MAPPING_BYTES,
            slots: frozen::RING_SLOTS,
            stride: frozen::RING_SLOT_STRIDE,
        },
        Reply::Active {
            first_frame,
            attributes: false,
        } => ControlMessage::Active {
            frame_sequence: first_frame.get(),
        },
        Reply::Released => ControlMessage::Released,
        Reply::Pong(UploadState::Idle) => ControlMessage::Pong,
        _ => return Err(LegacyError::Direction),
    };
    frozen::encode_control(&ControlRecord {
        session_id: identity.session.get(),
        sequence: identity.sequence.get(),
        generation: identity.epoch.get(),
        message,
    })
    .map_err(LegacyError::Codec)
}

#[cfg(test)]
mod tests {
    use super::super::wire12::FrameSequence;
    use super::super::{
        frame::{self, literals as frames, FrameContext, Glyph},
        wire12::{self, literals},
    };
    use super::*;

    fn identity(sequence: u64) -> ReplyIdentity {
        ReplyIdentity {
            session: SessionId::new(1).unwrap(),
            sequence: RequestSequence::new(sequence).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
        }
    }
    fn context() -> FrameContext {
        FrameContext {
            sequence: FrameSequence::new(1).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: wire12::MetricsEpoch::new(9).unwrap(),
            columns: 1,
            rows: 1,
            activating: true,
        }
    }
    fn slot() -> Box<[u8]> {
        let mut bytes = vec![0; wire12::SLOT_BYTES as usize];
        bytes[..frames::FRAME_INITIAL.len()].copy_from_slice(frames::FRAME_INITIAL);
        bytes[96..104].fill(0);
        bytes.into_boxed_slice()
    }

    #[test]
    fn legacy_hello_keeps_its_frozen_minor_and_rejects_new_or_forged_headers() {
        let mut bytes = literals::HELLO.to_vec();
        bytes[38..40].fill(0);
        let record = decode_hello(&bytes).unwrap();
        assert_eq!(record.header.epoch.get(), 7);
        assert_eq!(record.header.sequence.get(), 1);
        assert_eq!(record.header.session, None);
        assert!(
            matches!(record.request, Request::Hello { capability_id, attributes: false, .. } if capability_id.get()==17)
        );
        for (offset, value) in [(6, 1), (38, 2), (16, 2)] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            assert!(decode_hello(&bad).is_err());
        }
    }

    #[test]
    fn legacy_active_pong_and_release_keep_their_exact_bytes_and_flags() {
        let active = encode(
            identity(2),
            Reply::Active {
                first_frame: FrameSequence::new(1).unwrap(),
                attributes: false,
            },
        )
        .unwrap();
        assert_eq!(
            active,
            [
                40, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0,
                0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        let pong = encode(identity(3), Reply::Pong(UploadState::Idle)).unwrap();
        assert_eq!(
            pong,
            [
                32, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0,
                0, 0, 0, 0, 0
            ]
        );
        let released = encode(identity(4), Reply::Released).unwrap();
        assert_eq!(
            released,
            [
                32, 0, 0, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0,
                0, 0, 0, 0, 0
            ]
        );
        assert!(encode(
            identity(2),
            Reply::Active {
                first_frame: FrameSequence::new(1).unwrap(),
                attributes: true
            }
        )
        .is_err());
        assert!(encode(
            identity(3),
            Reply::Pong(UploadState::Ready(wire12::ResourceHandle::new(1).unwrap()))
        )
        .is_err());
    }

    #[test]
    fn legacy_setup_keeps_its_exact_ring_shape_and_cannot_describe_rounded_pshm() {
        let metrics =
            wire12::Metrics::new(wire12::MetricsEpoch::new(9).unwrap(), 1, 1, 8, 16).unwrap();
        let ring =
            wire12::RingExtent::new(wire12::BackingKind::LinuxSealed, wire12::RING_BYTES).unwrap();
        let bytes = encode(identity(1), Reply::Setup { ring, metrics }).unwrap();
        assert_eq!(
            bytes,
            [
                48, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0,
                0, 0, 0, 0, 0, 0, 16, 64, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 16, 0
            ]
        );
        let ring =
            wire12::RingExtent::new(wire12::BackingKind::DarwinFixedPosix, 4_210_688).unwrap();
        assert!(encode(identity(1), Reply::Setup { ring, metrics }).is_err());
    }

    #[test]
    fn legacy_frame_checks_reserved_bytes_and_keeps_its_original_used_length_rule() {
        let frame = frame::decode_legacy_slot(slot(), context()).unwrap();
        assert_eq!(frame.cells[0].glyph, Glyph::Scalar('?'));
        assert_eq!(frame.cells[0].width(), 1);
        let mut padded = slot();
        padded[..4].copy_from_slice(&145u32.to_le_bytes());
        assert!(frame::decode_legacy_slot(padded, context()).is_ok());
        for offset in [76, 80, 84, 88, 96, 104, 127] {
            let mut bad = slot();
            bad[offset] = 1;
            assert!(frame::decode_legacy_slot(bad, context()).is_err());
        }
        let mut bad = slot();
        bad[200] = 1;
        assert!(frame::decode_legacy_slot(bad, context()).is_err());
    }
}
