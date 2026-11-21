// SPDX-License-Identifier: MIT
//! Consumption acknowledges owned snapshots, separately from installation.

use super::{
    backing::RingBacking,
    frame::{self, FrameContext, FrameError, FrameSnapshot},
    profile::Dialect,
    wire12::{
        FrameSequence, MetricsEpoch, PtyEpoch, SessionId, RING_BYTES, RING_SLOTS, SLOT_BYTES,
    },
};
use std::{
    io,
    os::fd::BorrowedFd,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingContext {
    pub session: SessionId,
    pub epoch: PtyEpoch,
    pub metrics: MetricsEpoch,
    pub columns: u32,
    pub rows: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RingError {
    Header,
    Counters,
    Exhausted,
    Unacknowledged,
    Receipt,
    Allocation,
    Frame(FrameError),
}

pub struct ConsumptionReceipt {
    identity: Arc<()>,
    sequence: FrameSequence,
}

pub struct CopiedFrame {
    pub snapshot: FrameSnapshot,
    pub receipt: ConsumptionReceipt,
}

pub struct SharedRing {
    backing: RingBacking,
    address: NonNull<u8>,
    context: RingContext,
    header: [u8; 4096],
    consumed: u64,
    pending: Option<FrameSequence>,
    identity: Arc<()>,
    dialect: Dialect,
}

impl SharedRing {
    pub fn create(context: RingContext) -> io::Result<Self> {
        Self::create_for(context, Dialect::Images2)
    }

    pub fn create_legacy(context: RingContext) -> io::Result<Self> {
        Self::create_for(context, Dialect::Grid0)
    }

    fn create_for(context: RingContext, dialect: Dialect) -> io::Result<Self> {
        if !dialect.supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native ring dialect",
            ));
        }
        if !cfg!(all(
            target_endian = "little",
            target_pointer_width = "64",
            target_has_atomic = "64"
        )) || !(1..=4096).contains(&context.columns)
            || !(1..=4096).contains(&context.rows)
            || context
                .columns
                .checked_mul(context.rows)
                .is_none_or(|count| count > 32_768)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native ring geometry or atomics",
            ));
        }
        let mut backing = RingBacking::create()?;
        let address = backing.address();
        if address.as_ptr() as usize & 63 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native ring alignment",
            ));
        }
        let mut header = [0u8; 4096];
        header[..8].copy_from_slice(b"LUNARING");
        header[8..10].copy_from_slice(&1u16.to_le_bytes());
        header[10..12].copy_from_slice(&dialect.minor().to_le_bytes());
        header[12..16].copy_from_slice(&4096u32.to_le_bytes());
        header[16..24].copy_from_slice(&RING_BYTES.to_le_bytes());
        header[24..28].copy_from_slice(&RING_SLOTS.to_le_bytes());
        header[28..32].copy_from_slice(&SLOT_BYTES.to_le_bytes());
        header[32..40].copy_from_slice(&context.session.get().to_le_bytes());
        // New zero-filled storage; no other process has its descriptor yet.
        unsafe { std::ptr::copy_nonoverlapping(header.as_ptr(), address.as_ptr(), header.len()) };
        Ok(Self {
            backing,
            address,
            context,
            header,
            consumed: 0,
            pending: None,
            identity: Arc::new(()),
            dialect,
        })
    }

    pub fn descriptor(&self) -> BorrowedFd<'_> {
        self.backing.descriptor()
    }
    pub fn physical_bytes(&self) -> u64 {
        self.backing.physical_bytes()
    }

    fn counter(&self, offset: usize) -> &AtomicU64 {
        // Fixed mapping, initialized header, aligned interprocess counters.
        unsafe { &*self.address.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    /// Scheduling hint only; copy_next still validates the header and counters.
    pub(super) fn has_unconsumed_frame(&self) -> bool {
        self.counter(64).load(Ordering::Acquire) != self.consumed
    }

    pub fn copy_next(&mut self) -> Result<Option<CopiedFrame>, RingError> {
        if self.pending.is_some() {
            return Err(RingError::Unacknowledged);
        }
        for (offset, expected) in self.header.iter().enumerate() {
            if (64..72).contains(&offset) || (128..136).contains(&offset) {
                continue;
            }
            if unsafe { self.address.as_ptr().add(offset).read_volatile() } != *expected {
                return Err(RingError::Header);
            }
        }
        let producer = self.counter(64).load(Ordering::Acquire);
        if self.counter(128).load(Ordering::Acquire) != self.consumed
            || producer < self.consumed
            || producer - self.consumed > u64::from(RING_SLOTS)
        {
            return Err(RingError::Counters);
        }
        let next = self
            .consumed
            .checked_add(1)
            .and_then(FrameSequence::new)
            .ok_or(RingError::Exhausted)?;
        if producer == self.consumed {
            return Ok(None);
        }
        let start = 4096 + (self.consumed % u64::from(RING_SLOTS)) as usize * SLOT_BYTES as usize;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(SLOT_BYTES as usize)
            .map_err(|_| RingError::Allocation)?;
        owned.resize(SLOT_BYTES as usize, 0);
        for (offset, byte) in owned.iter_mut().enumerate() {
            // No Rust reference covers bytes the peer may modify.
            *byte = unsafe { self.address.as_ptr().add(start + offset).read_volatile() };
        }
        let decode = match self.dialect {
            Dialect::Grid0 => frame::decode_legacy_slot,
            Dialect::Images2 => frame::decode_slot,
        };
        let snapshot = decode(
            owned.into_boxed_slice(),
            FrameContext {
                sequence: next,
                epoch: self.context.epoch,
                metrics: self.context.metrics,
                columns: self.context.columns,
                rows: self.context.rows,
                activating: self.consumed == 0,
            },
        )
        .map_err(RingError::Frame)?;
        self.pending = Some(next);
        Ok(Some(CopiedFrame {
            snapshot,
            receipt: ConsumptionReceipt {
                identity: Arc::clone(&self.identity),
                sequence: next,
            },
        }))
    }

    /// The authority resolves READY resources before consuming this receipt.
    /// Installation and its acknowledgment remain separate.
    pub fn acknowledge(&mut self, receipt: ConsumptionReceipt) -> Result<FrameSequence, RingError> {
        if !Arc::ptr_eq(&self.identity, &receipt.identity) || self.pending != Some(receipt.sequence)
        {
            return Err(RingError::Receipt);
        }
        self.counter(128)
            .store(receipt.sequence.get(), Ordering::Release);
        self.consumed = receipt.sequence.get();
        self.pending = None;
        Ok(receipt.sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::literals;
    use super::*;
    fn context() -> RingContext {
        RingContext {
            session: SessionId::new(1).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns: 1,
            rows: 1,
        }
    }
    fn publish(ring: &SharedRing, sequence: u64, literal: &[u8]) {
        let start = 4096 + ((sequence - 1) % 4) as usize * SLOT_BYTES as usize;
        let address = unsafe { ring.address.as_ptr().add(start) };
        unsafe {
            std::ptr::write_bytes(address, 0, SLOT_BYTES as usize);
            std::ptr::copy_nonoverlapping(literal.as_ptr(), address, literal.len());
            std::ptr::copy_nonoverlapping(sequence.to_le_bytes().as_ptr(), address.add(8), 8);
        }
        ring.counter(64).store(sequence, Ordering::Release);
    }

    #[test]
    fn copies_are_owned_and_consumption_requires_an_acknowledgment() {
        let mut ring = SharedRing::create(context()).unwrap();
        assert!(ring.copy_next().unwrap().is_none());
        publish(&ring, 1, literals::FRAME_INITIAL);
        let copied = ring.copy_next().unwrap().unwrap();
        assert_eq!(ring.counter(128).load(Ordering::Acquire), 0);
        assert!(matches!(ring.copy_next(), Err(RingError::Unacknowledged)));
        unsafe { ring.address.as_ptr().add(4096 + 128).write_volatile(b'X') };
        assert_eq!(copied.snapshot.cells[0].glyph, frame::Glyph::Scalar('?'));
        assert_eq!(ring.acknowledge(copied.receipt).unwrap().get(), 1);
        assert_eq!(ring.counter(128).load(Ordering::Acquire), 1);
        assert!(ring.copy_next().unwrap().is_none());
    }

    #[test]
    fn slots_cycle_with_complete_placement_and_omission_snapshots() {
        let mut ring = SharedRing::create(context()).unwrap();
        publish(&ring, 1, literals::FRAME_INITIAL);
        publish(&ring, 2, literals::FRAME_PLACEMENT);
        publish(&ring, 3, literals::FRAME_OMISSION);
        publish(&ring, 4, literals::FRAME_PLACEMENT);
        for (sequence, expected) in [(1, 0), (2, 1), (3, 0), (4, 1)] {
            let copied = ring.copy_next().unwrap().unwrap();
            assert_eq!(copied.snapshot.context.sequence.get(), sequence);
            assert_eq!(copied.snapshot.placements.len(), expected);
            ring.acknowledge(copied.receipt).unwrap();
        }
        publish(&ring, 5, literals::FRAME_OMISSION);
        let copied = ring.copy_next().unwrap().unwrap();
        assert_eq!(copied.snapshot.context.sequence.get(), 5);
        assert_eq!(copied.snapshot.cells[0].glyph, frame::Glyph::Scalar('?'));
        assert_eq!(copied.snapshot.placements, Vec::new());
        ring.acknowledge(copied.receipt).unwrap();
        assert_eq!(ring.counter(128).load(Ordering::Acquire), 5);
    }

    #[test]
    fn malformed_headers_counters_and_slots_are_not_acknowledged() {
        for offset in [0, 10, 40, 72, 136, 4095] {
            let mut ring = SharedRing::create(context()).unwrap();
            publish(&ring, 1, literals::FRAME_INITIAL);
            unsafe { ring.address.as_ptr().add(offset).write_volatile(255) };
            assert!(matches!(ring.copy_next(), Err(RingError::Header)));
            assert_eq!(ring.counter(128).load(Ordering::Acquire), 0);
        }
        for (producer, consumer) in [(0, 1), (5, 0), (3, 1)] {
            let mut ring = SharedRing::create(context()).unwrap();
            ring.counter(64).store(producer, Ordering::Release);
            ring.counter(128).store(consumer, Ordering::Release);
            assert!(matches!(ring.copy_next(), Err(RingError::Counters)));
        }
        let mut ring = SharedRing::create(context()).unwrap();
        publish(&ring, 2, literals::FRAME_PLACEMENT);
        assert!(matches!(ring.copy_next(), Err(RingError::Frame(_))));
        assert_eq!(ring.counter(128).load(Ordering::Acquire), 0);
        let mut ring = SharedRing::create(context()).unwrap();
        ring.consumed = u64::MAX;
        ring.counter(64).store(u64::MAX, Ordering::Release);
        ring.counter(128).store(u64::MAX, Ordering::Release);
        assert!(matches!(ring.copy_next(), Err(RingError::Exhausted)));
    }

    #[test]
    fn receipts_cannot_acknowledge_another_ring_with_the_same_wire_identity() {
        let mut first = SharedRing::create(context()).unwrap();
        let mut second = SharedRing::create(context()).unwrap();
        publish(&first, 1, literals::FRAME_INITIAL);
        publish(&second, 1, literals::FRAME_INITIAL);
        let first_copy = first.copy_next().unwrap().unwrap();
        let second_copy = second.copy_next().unwrap().unwrap();
        assert_eq!(
            second.acknowledge(first_copy.receipt),
            Err(RingError::Receipt)
        );
        assert_eq!(second.counter(128).load(Ordering::Acquire), 0);
        second.acknowledge(second_copy.receipt).unwrap();
        assert_eq!(second.counter(128).load(Ordering::Acquire), 1);
        assert_eq!(first.counter(128).load(Ordering::Acquire), 0);
    }
}
