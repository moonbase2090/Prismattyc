// SPDX-License-Identifier: MIT
//! Explicit ABI 1.2 control decoding. FD ownership and session authority live above this layer.

use std::num::{NonZeroU32, NonZeroU64};

pub const HEADER_BYTES: usize = 32;
pub const MAX_RECORD_BYTES: usize = 4096;
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const RING_BYTES: u64 = 4_198_400;
pub const RING_SLOTS: u32 = 4;
pub const SLOT_BYTES: u32 = 1_048_576;
pub const SESSION_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_RESOURCES: u32 = 256;
pub const MAX_PLACEMENTS: u32 = 256;

macro_rules! identifier {
    ($($name:ident),+ $(,)?) => { $(
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);
        impl $name {
            pub fn new(value: u64) -> Option<Self> { NonZeroU64::new(value).map(Self) }
            pub fn get(self) -> u64 { self.0.get() }
        }
    )+ };
}
identifier!(
    SessionId,
    RequestSequence,
    PtyEpoch,
    FrameSequence,
    ResourceHandle,
    MetricsEpoch
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub session: Option<SessionId>,
    pub sequence: RequestSequence,
    pub epoch: PtyEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackingKind {
    LinuxSealed,
    DarwinFixedPosix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageUpload {
    pub image_id: NonZeroU32,
    pub source_generation: u64,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub backing: BackingKind,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    Hello {
        capability_id: NonZeroU32,
        offer: [u8; 16],
        attributes: bool,
    },
    Activate(FrameSequence),
    Release,
    Ping,
    PutImage(ImageUpload),
    RetireImage {
        handle: ResourceHandle,
        omitting_frame: FrameSequence,
    },
}

impl Request {
    pub fn descriptor_count(self) -> usize {
        usize::from(matches!(self, Self::PutImage(_)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    pub header: Header,
    pub request: Request,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Length,
    Header,
    Direction,
    Version,
    Body,
    Dimensions,
    Extent,
}

// The caller validates each opcode's exact length before reading fixed offsets.
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    if !(HEADER_BYTES..=MAX_RECORD_BYTES).contains(&bytes.len())
        || u32_at(bytes, 0) as usize != bytes.len()
    {
        return Err(DecodeError::Length);
    }
    let opcode = u16_at(bytes, 4);
    let expected_bytes = match opcode {
        1 => 56,
        3 => 40,
        5 | 7 => 32,
        10 => 112,
        12 => 48,
        _ => return Err(DecodeError::Direction),
    };
    if bytes.len() != expected_bytes {
        return Err(DecodeError::Length);
    }
    let flags = u16_at(bytes, 6);
    let header = Header {
        session: SessionId::new(u64_at(bytes, 8)),
        sequence: RequestSequence::new(u64_at(bytes, 16)).ok_or(DecodeError::Header)?,
        epoch: PtyEpoch::new(u64_at(bytes, 24)).ok_or(DecodeError::Header)?,
    };
    if opcode == 1 {
        if header.session.is_some() || header.sequence.get() != 1 || flags & !1 != 0 {
            return Err(DecodeError::Header);
        }
    } else if header.session.is_none() || flags != 0 {
        return Err(DecodeError::Header);
    }
    let request = match opcode {
        1 => {
            if u16_at(bytes, 36) != 1 || u16_at(bytes, 38) != 2 {
                return Err(DecodeError::Version);
            }
            Request::Hello {
                capability_id: NonZeroU32::new(u32_at(bytes, 32)).ok_or(DecodeError::Body)?,
                offer: bytes[40..56].try_into().unwrap(),
                attributes: flags & 1 != 0,
            }
        }
        3 => Request::Activate(FrameSequence::new(u64_at(bytes, 32)).ok_or(DecodeError::Body)?),
        5 => Request::Release,
        7 => Request::Ping,
        10 => Request::PutImage(image_upload(bytes)?),
        12 => Request::RetireImage {
            handle: ResourceHandle::new(u64_at(bytes, 32)).ok_or(DecodeError::Body)?,
            omitting_frame: FrameSequence::new(u64_at(bytes, 40)).ok_or(DecodeError::Body)?,
        },
        _ => unreachable!(),
    };
    Ok(Record { header, request })
}

fn image_upload(bytes: &[u8]) -> Result<ImageUpload, DecodeError> {
    if u16_at(bytes, 44) != 1 || bytes[104..112] != [0; 8] {
        return Err(DecodeError::Body);
    }
    let backing = match u16_at(bytes, 46) {
        1 => BackingKind::LinuxSealed,
        2 => BackingKind::DarwinFixedPosix,
        _ => return Err(DecodeError::Body),
    };
    let image_id = NonZeroU32::new(u32_at(bytes, 32)).ok_or(DecodeError::Body)?;
    let width = NonZeroU32::new(u32_at(bytes, 36)).ok_or(DecodeError::Dimensions)?;
    let height = NonZeroU32::new(u32_at(bytes, 40)).ok_or(DecodeError::Dimensions)?;
    let logical_bytes = u64_at(bytes, 56);
    let physical_bytes = u64_at(bytes, 64);
    let product = u64::from(width.get())
        .checked_mul(u64::from(height.get()))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(DecodeError::Dimensions)?;
    if width.get() > 16_777_216
        || height.get() > 16_777_216
        || product != logical_bytes
        || product > MAX_IMAGE_BYTES
    {
        return Err(DecodeError::Dimensions);
    }
    let valid_extent = match backing {
        BackingKind::LinuxSealed => physical_bytes == logical_bytes,
        BackingKind::DarwinFixedPosix => {
            physical_bytes >= logical_bytes
                && logical_bytes
                    .checked_add(65_536)
                    .is_some_and(|limit| physical_bytes < limit)
        }
    };
    if !valid_extent {
        return Err(DecodeError::Extent);
    }
    Ok(ImageUpload {
        image_id,
        source_generation: u64_at(bytes, 48),
        width,
        height,
        logical_bytes,
        physical_bytes,
        backing,
        digest: bytes[72..104].try_into().unwrap(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplyIdentity {
    pub session: SessionId,
    pub sequence: RequestSequence,
    pub epoch: PtyEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metrics {
    epoch: MetricsEpoch,
    cell_width: u32,
    cell_height: u32,
    guest_width: u32,
    guest_height: u32,
}

impl Metrics {
    pub fn new(
        epoch: MetricsEpoch,
        columns: u32,
        rows: u32,
        cell_width: u32,
        cell_height: u32,
    ) -> Option<Self> {
        if !(1..=4096).contains(&columns)
            || !(1..=4096).contains(&rows)
            || !(1..=4096).contains(&cell_width)
            || !(1..=4096).contains(&cell_height)
            || columns.checked_mul(rows)? > 32_768
        {
            return None;
        }
        Some(Self {
            epoch,
            cell_width,
            cell_height,
            guest_width: columns.checked_mul(cell_width)?,
            guest_height: rows.checked_mul(cell_height)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingExtent {
    kind: BackingKind,
    physical_bytes: u64,
}

impl RingExtent {
    pub(super) fn legacy_compatible(self) -> bool {
        self.kind == BackingKind::LinuxSealed && self.physical_bytes == RING_BYTES
    }

    pub fn new(kind: BackingKind, physical_bytes: u64) -> Option<Self> {
        let valid = match kind {
            BackingKind::LinuxSealed => physical_bytes == RING_BYTES,
            BackingKind::DarwinFixedPosix => {
                (RING_BYTES..RING_BYTES + 65_536).contains(&physical_bytes)
            }
        };
        valid.then_some(Self {
            kind,
            physical_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Pending,
    ExistingReady,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decline {
    ByteBudget,
    HandleBudget,
    Busy,
    Allocation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadFailure {
    Deadline,
    Allocation,
    Read,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadState {
    Idle,
    Pending(ResourceHandle),
    Ready(ResourceHandle),
    Failed {
        handle: ResourceHandle,
        error: UploadFailure,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retirement {
    Retired,
    Busy,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndReason {
    Resize,
    PtyOutput,
    LeaseExpired,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reply {
    Setup {
        ring: RingExtent,
        metrics: Metrics,
    },
    Active {
        first_frame: FrameSequence,
        attributes: bool,
    },
    Released,
    Admitted {
        handle: ResourceHandle,
        status: Admission,
    },
    Declined(Decline),
    Pong(UploadState),
    Retired {
        handle: ResourceHandle,
        status: Retirement,
    },
    SessionEnded {
        current_epoch: PtyEpoch,
        reason: EndReason,
    },
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn encode(identity: ReplyIdentity, reply: Reply) -> Vec<u8> {
    let (opcode, length, flags): (u16, usize, u16) = match reply {
        Reply::Setup { .. } => (9, 128, 0),
        Reply::Active { attributes, .. } => (4, 40, 2 | u16::from(attributes)),
        Reply::Released => (6, 32, 0),
        Reply::Admitted { .. } | Reply::Declined(_) => (11, 48, 0),
        Reply::Pong(_) => (15, 48, 0),
        Reply::Retired { .. } => (13, 48, 0),
        Reply::SessionEnded { .. } => (14, 48, 0),
    };
    let mut bytes = vec![0; length];
    put_u32(&mut bytes, 0, length as u32);
    bytes[4..6].copy_from_slice(&opcode.to_le_bytes());
    bytes[6..8].copy_from_slice(&flags.to_le_bytes());
    put_u64(&mut bytes, 8, identity.session.get());
    put_u64(&mut bytes, 16, identity.sequence.get());
    put_u64(&mut bytes, 24, identity.epoch.get());
    match reply {
        Reply::Setup { ring, metrics } => {
            put_u64(&mut bytes, 32, RING_BYTES);
            put_u64(&mut bytes, 40, ring.physical_bytes);
            put_u32(&mut bytes, 48, RING_SLOTS);
            put_u32(&mut bytes, 52, SLOT_BYTES);
            let kind: u16 = match ring.kind {
                BackingKind::LinuxSealed => 1,
                BackingKind::DarwinFixedPosix => 2,
            };
            bytes[56..58].copy_from_slice(&kind.to_le_bytes());
            bytes[58..60].copy_from_slice(&1u16.to_le_bytes());
            put_u64(&mut bytes, 64, metrics.epoch.get());
            put_u32(&mut bytes, 72, metrics.cell_width);
            put_u32(&mut bytes, 76, metrics.cell_height);
            put_u32(&mut bytes, 80, metrics.guest_width);
            put_u32(&mut bytes, 84, metrics.guest_height);
            put_u64(&mut bytes, 88, MAX_IMAGE_BYTES);
            put_u64(&mut bytes, 96, SESSION_IMAGE_BYTES);
            put_u32(&mut bytes, 104, MAX_RESOURCES);
            put_u32(&mut bytes, 108, MAX_PLACEMENTS);
        }
        Reply::Active { first_frame, .. } => put_u64(&mut bytes, 32, first_frame.get()),
        Reply::Released => {}
        Reply::Admitted { handle, status } => {
            put_u64(&mut bytes, 32, handle.get());
            put_u32(
                &mut bytes,
                40,
                match status {
                    Admission::Pending => 0,
                    Admission::ExistingReady => 1,
                },
            );
        }
        Reply::Declined(reason) => put_u32(
            &mut bytes,
            40,
            match reason {
                Decline::ByteBudget => 2,
                Decline::HandleBudget => 3,
                Decline::Busy => 4,
                Decline::Allocation => 5,
            },
        ),
        Reply::Pong(state) => {
            let (handle, state, error) = match state {
                UploadState::Idle => (0, 0, 0),
                UploadState::Pending(handle) => (handle.get(), 1, 0),
                UploadState::Ready(handle) => (handle.get(), 2, 0),
                UploadState::Failed { handle, error } => (
                    handle.get(),
                    3,
                    match error {
                        UploadFailure::Deadline => 1,
                        UploadFailure::Allocation => 2,
                        UploadFailure::Read => 3,
                    },
                ),
            };
            put_u64(&mut bytes, 32, handle);
            put_u32(&mut bytes, 40, state);
            put_u32(&mut bytes, 44, error);
        }
        Reply::Retired { handle, status } => {
            put_u64(&mut bytes, 32, handle.get());
            put_u32(
                &mut bytes,
                40,
                match status {
                    Retirement::Retired => 0,
                    Retirement::Busy => 1,
                },
            );
        }
        Reply::SessionEnded {
            current_epoch,
            reason,
        } => {
            put_u64(&mut bytes, 32, current_epoch.get());
            put_u32(
                &mut bytes,
                40,
                match reason {
                    EndReason::Resize => 1,
                    EndReason::PtyOutput => 2,
                    EndReason::LeaseExpired => 3,
                    EndReason::Release => 4,
                },
            );
        }
    }
    bytes
}

#[cfg(test)]
#[path = "wire12_literals.rs"]
pub(crate) mod literals;

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Vec<u8> {
        literals::HELLO.to_vec()
    }
    fn upload() -> Vec<u8> {
        literals::PUT_IMAGE.to_vec()
    }

    #[test]
    fn hello_literal_preserves_offer_and_authority_identity() {
        assert_eq!(
            decode(&hello()),
            Ok(Record {
                header: Header {
                    session: None,
                    sequence: RequestSequence::new(1).unwrap(),
                    epoch: PtyEpoch::new(7).unwrap()
                },
                request: Request::Hello {
                    capability_id: NonZeroU32::new(17).unwrap(),
                    offer: [1, 35, 69, 103, 137, 171, 205, 239, 1, 35, 69, 103, 137, 171, 205, 239],
                    attributes: false
                },
            })
        );
    }

    #[test]
    fn image_literal_preserves_zero_generation_digest_and_logical_extent() {
        let record = decode(&upload()).unwrap();
        assert_eq!(record.header.session.unwrap().get(), 1);
        assert_eq!(record.header.sequence.get(), 4);
        assert_eq!(record.header.epoch.get(), 7);
        assert_eq!(record.request.descriptor_count(), 1);
        assert_eq!(
            record.request,
            Request::PutImage(ImageUpload {
                image_id: NonZeroU32::new(1).unwrap(),
                source_generation: 0,
                width: NonZeroU32::new(1).unwrap(),
                height: NonZeroU32::new(1).unwrap(),
                logical_bytes: 4,
                physical_bytes: 16384,
                backing: BackingKind::DarwinFixedPosix,
                digest: [
                    196, 11, 74, 192, 81, 79, 98, 173, 45, 134, 119, 232, 248, 188, 83, 237, 90,
                    128, 88, 44, 94, 163, 175, 195, 149, 226, 208, 223, 109, 136, 40, 161
                ],
            })
        );
    }

    #[test]
    fn framing_rejects_truncation_extra_bytes_and_forged_lengths() {
        let valid = hello();
        for length in 0..valid.len() {
            assert_eq!(decode(&valid[..length]), Err(DecodeError::Length));
        }
        let mut extra = valid.clone();
        extra.push(0);
        assert_eq!(decode(&extra), Err(DecodeError::Length));
        let mut oversized = vec![0; 4097];
        oversized[..4].copy_from_slice(&4097u32.to_le_bytes());
        assert_eq!(decode(&oversized), Err(DecodeError::Length));
        let mut forged = valid;
        forged[..4].copy_from_slice(&40u32.to_le_bytes());
        assert_eq!(decode(&forged), Err(DecodeError::Length));
    }

    #[test]
    fn hello_requires_supported_dialect_and_fresh_identity_shape() {
        for offset in [6, 8, 16, 24, 32, 36, 38] {
            let mut bytes = hello();
            bytes[offset] = match offset {
                6 => 2,
                8 => 1,
                16 => 2,
                _ => 0,
            };
            assert!(decode(&bytes).is_err(), "offset {offset}");
        }
        let mut attributes = hello();
        attributes[6] = 1;
        assert!(matches!(
            decode(&attributes).unwrap().request,
            Request::Hello {
                attributes: true,
                ..
            }
        ));
        // This decoder does not grant alpha support or validate the one-use offer.
    }

    #[test]
    fn image_rejects_invalid_dimensions_format_identity_and_reserved_bytes() {
        for (offset, value) in [
            (6, 1),
            (8, 0),
            (16, 0),
            (24, 0),
            (32, 0),
            (36, 0),
            (40, 0),
            (44, 2),
            (46, 0),
            (56, 8),
            (104, 1),
        ] {
            let mut bytes = upload();
            bytes[offset] = value;
            assert!(decode(&bytes).is_err(), "offset {offset}");
        }
        let mut overflow = upload();
        overflow[36..44].fill(255);
        assert_eq!(decode(&overflow), Err(DecodeError::Dimensions));
        let mut oversized = upload();
        oversized[36..40].copy_from_slice(&16_777_217u32.to_le_bytes());
        assert_eq!(decode(&oversized), Err(DecodeError::Dimensions));
    }

    #[test]
    fn image_extent_obeys_backing_kind_without_reading_rounded_suffix() {
        for physical in [0, 3, 65_540, u64::MAX] {
            let mut bytes = upload();
            bytes[64..72].copy_from_slice(&physical.to_le_bytes());
            assert_eq!(decode(&bytes), Err(DecodeError::Extent));
        }
        let mut linux = upload();
        linux[46] = 1;
        assert_eq!(decode(&linux), Err(DecodeError::Extent));
        linux[64..72].copy_from_slice(&4u64.to_le_bytes());
        assert!(matches!(
            decode(&linux).unwrap().request,
            Request::PutImage(ImageUpload {
                backing: BackingKind::LinuxSealed,
                ..
            })
        ));
        let mut maximum = upload();
        maximum[36..40].copy_from_slice(&16_777_216u32.to_le_bytes());
        maximum[56..64].copy_from_slice(&67_108_864u64.to_le_bytes());
        maximum[64..72].copy_from_slice(&67_108_864u64.to_le_bytes());
        assert!(decode(&maximum).is_ok());
    }
    fn identity(sequence: u64) -> ReplyIdentity {
        ReplyIdentity {
            session: SessionId::new(1).unwrap(),
            sequence: RequestSequence::new(sequence).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
        }
    }

    #[test]
    fn setup_reply_matches_the_complete_fixed_literal() {
        let metrics = Metrics::new(MetricsEpoch::new(9).unwrap(), 1, 1, 8, 16).unwrap();
        let ring = RingExtent::new(BackingKind::DarwinFixedPosix, 4_210_688).unwrap();
        assert_eq!(
            encode(identity(1), Reply::Setup { ring, metrics }),
            literals::SETUP_EXT
        );
    }

    #[test]
    fn replies_match_the_fixed_lifecycle_and_upload_trace() {
        let handle = ResourceHandle::new(1).unwrap();
        for (sequence, reply, expected) in [
            (
                2,
                Reply::Active {
                    first_frame: FrameSequence::new(1).unwrap(),
                    attributes: false,
                },
                literals::ACTIVE_IMAGES,
            ),
            (3, Reply::Pong(UploadState::Idle), literals::PONG_IDLE),
            (
                4,
                Reply::Admitted {
                    handle,
                    status: Admission::Pending,
                },
                literals::IMAGE_PENDING,
            ),
            (
                5,
                Reply::Pong(UploadState::Pending(handle)),
                literals::PONG_PENDING,
            ),
            (
                6,
                Reply::Pong(UploadState::Ready(handle)),
                literals::PONG_READY,
            ),
            (
                6,
                Reply::Pong(UploadState::Failed {
                    handle,
                    error: UploadFailure::Deadline,
                }),
                literals::PONG_FAILED,
            ),
            (
                7,
                Reply::Retired {
                    handle,
                    status: Retirement::Busy,
                },
                literals::RETIRE_BUSY,
            ),
            (
                9,
                Reply::Retired {
                    handle,
                    status: Retirement::Retired,
                },
                literals::IMAGE_RETIRED,
            ),
            (
                10,
                Reply::SessionEnded {
                    current_epoch: PtyEpoch::new(8).unwrap(),
                    reason: EndReason::Resize,
                },
                literals::SESSION_ENDED,
            ),
        ] {
            assert_eq!(encode(identity(sequence), reply), expected, "{reply:?}");
        }
    }

    #[test]
    fn metrics_and_ring_metadata_reject_invalid_setup_shapes() {
        let epoch = MetricsEpoch::new(9).unwrap();
        for (columns, rows, width, height) in [
            (0, 1, 8, 16),
            (1, 0, 8, 16),
            (4097, 1, 8, 16),
            (1, 4097, 8, 16),
            (4096, 4096, 8, 16),
            (1, 1, 0, 16),
            (1, 1, 8, 0),
            (1, 1, 4097, 16),
            (1, 1, 8, 4097),
        ] {
            assert_eq!(Metrics::new(epoch, columns, rows, width, height), None);
        }
        for size in [0, 4_198_399, 4_263_936, u64::MAX] {
            assert_eq!(RingExtent::new(BackingKind::DarwinFixedPosix, size), None);
        }
        assert_eq!(RingExtent::new(BackingKind::LinuxSealed, 4_210_688), None);
        assert!(RingExtent::new(BackingKind::LinuxSealed, 4_198_400).is_some());
        assert!(Metrics::new(epoch, 4096, 8, 4096, 4096).is_some());
    }

    #[test]
    fn client_lifecycle_literals_preserve_frame_and_retirement_identity() {
        let activate = decode(literals::ACTIVATE).unwrap();
        assert_eq!(
            activate.header,
            Header {
                session: SessionId::new(1),
                sequence: RequestSequence::new(2).unwrap(),
                epoch: PtyEpoch::new(7).unwrap()
            }
        );
        assert_eq!(
            activate.request,
            Request::Activate(FrameSequence::new(1).unwrap())
        );
        assert_eq!(activate.request.descriptor_count(), 0);
        let ping = decode(literals::PING_IDLE).unwrap();
        assert_eq!(ping.header.sequence.get(), 3);
        assert_eq!(ping.request, Request::Ping);
        let retire = decode(literals::RETIRE_IMAGE).unwrap();
        assert_eq!(retire.header.sequence.get(), 7);
        assert_eq!(
            retire.request,
            Request::RetireImage {
                handle: ResourceHandle::new(1).unwrap(),
                omitting_frame: FrameSequence::new(3).unwrap()
            }
        );
        for source in [literals::ACTIVATE, literals::RETIRE_IMAGE] {
            let mut invalid = source.to_vec();
            invalid[32..40].fill(0);
            assert_eq!(decode(&invalid), Err(DecodeError::Body));
        }
    }
}
