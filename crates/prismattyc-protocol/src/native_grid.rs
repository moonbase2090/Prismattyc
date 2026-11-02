//! Wire types for the native grid protocol, `native.grid.v1`.
//!
//! The contract is the Lunatui native protocol proposal (ring ABI 1.0). This
//! module holds only constants and pure encoders and decoders for the
//! control-record header, the request and response bodies, and the
//! shared-memory frame header. It does no I/O and is not wired into the host;
//! later slices add the transport, the state machine and the renderer.
//!
//! Every integer is explicit little-endian. Decoders accept only exact,
//! canonical encodings: a length, flag, reserved byte or fixed value that
//! differs from the contract is an error, never a best-effort parse.

use std::error::Error;
use std::fmt;

/// Bytes in every control-record header.
pub const CONTROL_HEADER_BYTES: usize = 32;
/// Largest control record the contract allows.
pub const MAX_CONTROL_RECORD_BYTES: usize = 4096;

/// Ring ABI version carried in HELLO and in the ring header.
pub const RING_ABI_MAJOR: u16 = 1;
pub const RING_ABI_MINOR: u16 = 0;
/// Magic at the start of the shared mapping.
pub const RING_MAGIC: [u8; 8] = *b"LUNARING";
/// Bytes in the shared-mapping header.
pub const RING_HEADER_BYTES: u32 = 4096;
/// Frame slots in the ring.
pub const RING_SLOTS: u32 = 4;
/// Bytes in each slot.
pub const RING_SLOT_STRIDE: u32 = 1_048_576;
/// Total mapping bytes: the header plus every slot.
pub const RING_MAPPING_BYTES: u64 =
    RING_HEADER_BYTES as u64 + RING_SLOTS as u64 * RING_SLOT_STRIDE as u64;
/// Offset of the client-owned producer sequence in the ring header.
pub const RING_PRODUCER_OFFSET: usize = 64;
/// Offset of the host-owned consumer sequence in the ring header.
pub const RING_CONSUMER_OFFSET: usize = 128;

/// Bytes in each slot's frame header.
pub const FRAME_HEADER_BYTES: u16 = 128;
/// Frame flag: the frame is a full snapshot. No other flag bit is defined.
pub const FRAME_FLAG_FULL_SNAPSHOT: u16 = 1;
/// Per-axis ceiling for columns and rows.
pub const MAX_FRAME_AXIS: u32 = 4096;
/// Ceiling for columns × rows.
pub const MAX_FRAME_CELLS: u32 = 32_768;
/// Ceiling for the frame's UTF-8 cluster blob.
pub const MAX_FRAME_BLOB_BYTES: u32 = 262_144;
/// Ceiling for damage records, also bounded by twice the row count.
pub const MAX_FRAME_DAMAGE_RECORDS: u32 = 8192;
/// Cursor flag: the cursor is visible. No other flag bit is defined.
pub const CURSOR_FLAG_VISIBLE: u32 = 1;
/// Bytes per cell, cluster record and damage record.
pub const CELL_BYTES: u32 = 16;
pub const CLUSTER_RECORD_BYTES: u32 = 16;
pub const DAMAGE_RECORD_BYTES: u32 = 16;

/// Control-record opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Hello = 1,
    Setup = 2,
    Activate = 3,
    Active = 4,
    Release = 5,
    Released = 6,
    Ping = 7,
    Pong = 8,
}

impl Opcode {
    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Setup,
            3 => Self::Activate,
            4 => Self::Active,
            5 => Self::Release,
            6 => Self::Released,
            7 => Self::Ping,
            8 => Self::Pong,
            _ => return None,
        })
    }

    /// The exact total record length for this opcode, header included.
    pub fn record_bytes(self) -> usize {
        match self {
            Self::Hello => 56,
            Self::Setup => 48,
            Self::Activate | Self::Active => 40,
            Self::Release | Self::Released | Self::Ping | Self::Pong => 32,
        }
    }

    /// True for the messages a client sends; false for host responses.
    pub fn is_request(self) -> bool {
        matches!(
            self,
            Self::Hello | Self::Activate | Self::Release | Self::Ping
        )
    }
}

/// A decoded control-record body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMessage {
    Hello {
        capability_id: u32,
        abi_major: u16,
        abi_minor: u16,
        offer: [u8; 16],
    },
    Setup {
        mapping_bytes: u64,
        slots: u32,
        stride: u32,
    },
    Activate {
        frame_sequence: u64,
    },
    Active {
        frame_sequence: u64,
    },
    Release,
    Released,
    Ping,
    Pong,
}

impl ControlMessage {
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Hello { .. } => Opcode::Hello,
            Self::Setup { .. } => Opcode::Setup,
            Self::Activate { .. } => Opcode::Activate,
            Self::Active { .. } => Opcode::Active,
            Self::Release => Opcode::Release,
            Self::Released => Opcode::Released,
            Self::Ping => Opcode::Ping,
            Self::Pong => Opcode::Pong,
        }
    }
}

/// One control record: header identity plus its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRecord {
    /// Zero only in HELLO; the host assigns it in SETUP.
    pub session_id: u64,
    /// Starts at 1 and increases without wrap. A response echoes its request.
    pub sequence: u64,
    /// The nonzero PTY authority generation from the capability reply.
    pub generation: u64,
    pub message: ControlMessage,
}

/// Why a control record or frame header was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeGridError {
    /// The buffer is not exactly one record of a valid size.
    Length,
    /// The opcode is not in the contract.
    Opcode,
    /// The total length does not match the opcode's exact size.
    RecordSize,
    /// The header flags are not zero.
    Flags,
    /// The request sequence is zero.
    Sequence,
    /// The generation is zero.
    Generation,
    /// The session ID is zero where it must be assigned, or nonzero in HELLO.
    Session,
    /// A fixed contract value (ABI, mapping size, slots, stride) differs.
    ContractValue,
    /// The capability ID is zero.
    CapabilityId,
    /// A frame-header field is outside its bounds.
    FrameField(FrameField),
}

/// The frame-header field that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameField {
    UsedBytes,
    HeaderBytes,
    Flags,
    Sequence,
    Generation,
    Geometry,
    CellCount,
    Offsets,
    ClusterCount,
    BlobBytes,
    DamageCount,
    Cursor,
    Reserved,
}

impl fmt::Display for NativeGridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length => f.write_str("control record length is invalid"),
            Self::Opcode => f.write_str("unknown control opcode"),
            Self::RecordSize => f.write_str("control record size does not match its opcode"),
            Self::Flags => f.write_str("control header flags must be zero"),
            Self::Sequence => f.write_str("request sequence must be nonzero"),
            Self::Generation => f.write_str("PTY generation must be nonzero"),
            Self::Session => f.write_str("session ID does not match the message"),
            Self::ContractValue => f.write_str("fixed contract value does not match ring ABI 1.0"),
            Self::CapabilityId => f.write_str("capability ID must be nonzero"),
            Self::FrameField(field) => write!(f, "frame header field is invalid: {field:?}"),
        }
    }
}

impl Error for NativeGridError {}

/// Encode one control record. The caller supplies a structurally valid
/// record; this returns an error for anything the decoder would reject, so
/// an encoded record always decodes back to the same value.
pub fn encode_control(record: &ControlRecord) -> Result<Vec<u8>, NativeGridError> {
    validate_control(record)?;
    let opcode = record.message.opcode();
    let total = opcode.record_bytes();
    let mut out = Vec::with_capacity(total);
    put_u32(&mut out, total as u32);
    put_u16(&mut out, opcode as u16);
    put_u16(&mut out, 0);
    put_u64(&mut out, record.session_id);
    put_u64(&mut out, record.sequence);
    put_u64(&mut out, record.generation);
    match record.message {
        ControlMessage::Hello {
            capability_id,
            abi_major,
            abi_minor,
            offer,
        } => {
            put_u32(&mut out, capability_id);
            put_u16(&mut out, abi_major);
            put_u16(&mut out, abi_minor);
            out.extend_from_slice(&offer);
        }
        ControlMessage::Setup {
            mapping_bytes,
            slots,
            stride,
        } => {
            put_u64(&mut out, mapping_bytes);
            put_u32(&mut out, slots);
            put_u32(&mut out, stride);
        }
        ControlMessage::Activate { frame_sequence } | ControlMessage::Active { frame_sequence } => {
            put_u64(&mut out, frame_sequence);
        }
        ControlMessage::Release
        | ControlMessage::Released
        | ControlMessage::Ping
        | ControlMessage::Pong => {}
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// Read the total length from a control header without trusting the rest.
/// Returns the length a reader must collect before calling
/// [`decode_control`], or an error when it is outside 32..=4096.
pub fn control_record_len(header: &[u8; CONTROL_HEADER_BYTES]) -> Result<usize, NativeGridError> {
    let total = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    if !(CONTROL_HEADER_BYTES..=MAX_CONTROL_RECORD_BYTES).contains(&total) {
        return Err(NativeGridError::Length);
    }
    Ok(total)
}

/// Decode exactly one control record. The buffer must hold the whole record
/// and nothing else.
pub fn decode_control(bytes: &[u8]) -> Result<ControlRecord, NativeGridError> {
    if bytes.len() < CONTROL_HEADER_BYTES || bytes.len() > MAX_CONTROL_RECORD_BYTES {
        return Err(NativeGridError::Length);
    }
    let total = u32_at(bytes, 0) as usize;
    if total != bytes.len() {
        return Err(NativeGridError::Length);
    }
    let opcode = Opcode::from_u16(u16_at(bytes, 4)).ok_or(NativeGridError::Opcode)?;
    if total != opcode.record_bytes() {
        return Err(NativeGridError::RecordSize);
    }
    if u16_at(bytes, 6) != 0 {
        return Err(NativeGridError::Flags);
    }
    let body = &bytes[CONTROL_HEADER_BYTES..];
    let message = match opcode {
        Opcode::Hello => ControlMessage::Hello {
            capability_id: u32_at(body, 0),
            abi_major: u16_at(body, 4),
            abi_minor: u16_at(body, 6),
            offer: body[8..24].try_into().unwrap(),
        },
        Opcode::Setup => ControlMessage::Setup {
            mapping_bytes: u64_at(body, 0),
            slots: u32_at(body, 8),
            stride: u32_at(body, 12),
        },
        Opcode::Activate => ControlMessage::Activate {
            frame_sequence: u64_at(body, 0),
        },
        Opcode::Active => ControlMessage::Active {
            frame_sequence: u64_at(body, 0),
        },
        Opcode::Release => ControlMessage::Release,
        Opcode::Released => ControlMessage::Released,
        Opcode::Ping => ControlMessage::Ping,
        Opcode::Pong => ControlMessage::Pong,
    };
    let record = ControlRecord {
        session_id: u64_at(bytes, 8),
        sequence: u64_at(bytes, 16),
        generation: u64_at(bytes, 24),
        message,
    };
    validate_control(&record)?;
    Ok(record)
}

/// Field rules that hold for a single record on its own. Ordering, the one
/// outstanding request and offer binding belong to the later state machine.
fn validate_control(record: &ControlRecord) -> Result<(), NativeGridError> {
    if record.sequence == 0 {
        return Err(NativeGridError::Sequence);
    }
    if record.generation == 0 {
        return Err(NativeGridError::Generation);
    }
    match record.message {
        ControlMessage::Hello {
            capability_id,
            abi_major,
            abi_minor,
            ..
        } => {
            if record.session_id != 0 {
                return Err(NativeGridError::Session);
            }
            if capability_id == 0 {
                return Err(NativeGridError::CapabilityId);
            }
            if abi_major != RING_ABI_MAJOR || abi_minor != RING_ABI_MINOR {
                return Err(NativeGridError::ContractValue);
            }
        }
        ControlMessage::Setup {
            mapping_bytes,
            slots,
            stride,
        } => {
            if record.session_id == 0 {
                return Err(NativeGridError::Session);
            }
            if mapping_bytes != RING_MAPPING_BYTES
                || slots != RING_SLOTS
                || stride != RING_SLOT_STRIDE
            {
                return Err(NativeGridError::ContractValue);
            }
        }
        _ => {
            if record.session_id == 0 {
                return Err(NativeGridError::Session);
            }
        }
    }
    Ok(())
}

/// The fixed 128-byte header at the start of each frame slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub used_bytes: u32,
    pub flags: u16,
    pub frame_sequence: u64,
    pub generation: u64,
    pub columns: u32,
    pub rows: u32,
    pub cell_count: u32,
    pub cells_offset: u32,
    pub cluster_count: u32,
    pub clusters_offset: u32,
    pub blob_bytes: u32,
    pub blob_offset: u32,
    pub damage_count: u32,
    pub damage_offset: u32,
    pub cursor_column: u32,
    pub cursor_row: u32,
    pub cursor_flags: u32,
}

/// The canonical section layout for a frame of the given sizes, or `None`
/// when a section would overflow or the frame would not fit one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub cells_offset: u32,
    pub clusters_offset: u32,
    pub blob_offset: u32,
    pub damage_offset: u32,
    /// End of the damage records: the minimum used-slot bytes.
    pub end: u32,
}

impl FrameLayout {
    /// Cells start at 128; each later section starts at the previous end
    /// rounded up to 8.
    pub fn canonical(
        cell_count: u32,
        cluster_count: u32,
        blob_bytes: u32,
        damage_count: u32,
    ) -> Option<Self> {
        let cells_offset = u32::from(FRAME_HEADER_BYTES);
        let cells_end = cells_offset.checked_add(cell_count.checked_mul(CELL_BYTES)?)?;
        let clusters_offset = round_up_8(cells_end)?;
        let clusters_end =
            clusters_offset.checked_add(cluster_count.checked_mul(CLUSTER_RECORD_BYTES)?)?;
        let blob_offset = round_up_8(clusters_end)?;
        let blob_end = blob_offset.checked_add(blob_bytes)?;
        let damage_offset = round_up_8(blob_end)?;
        let end = damage_offset.checked_add(damage_count.checked_mul(DAMAGE_RECORD_BYTES)?)?;
        (end <= RING_SLOT_STRIDE).then_some(Self {
            cells_offset,
            clusters_offset,
            blob_offset,
            damage_offset,
            end,
        })
    }
}

impl FrameHeader {
    /// A header for a frame of the given geometry and section sizes, with
    /// canonical offsets and `used_bytes` set to the end of the last section.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        full_snapshot: bool,
        frame_sequence: u64,
        generation: u64,
        columns: u32,
        rows: u32,
        cluster_count: u32,
        blob_bytes: u32,
        damage_count: u32,
        cursor: Option<(u32, u32)>,
    ) -> Result<Self, NativeGridError> {
        // Check the geometry before sizing sections, so an oversized grid
        // reports its real cause rather than a layout overflow.
        if !(1..=MAX_FRAME_AXIS).contains(&columns) || !(1..=MAX_FRAME_AXIS).contains(&rows) {
            return Err(NativeGridError::FrameField(FrameField::Geometry));
        }
        let cell_count = columns * rows;
        if cell_count > MAX_FRAME_CELLS {
            return Err(NativeGridError::FrameField(FrameField::CellCount));
        }
        let layout = FrameLayout::canonical(cell_count, cluster_count, blob_bytes, damage_count)
            .ok_or(NativeGridError::FrameField(FrameField::Offsets))?;
        let (cursor_column, cursor_row, cursor_flags) = match cursor {
            Some((column, row)) => (column, row, CURSOR_FLAG_VISIBLE),
            None => (0, 0, 0),
        };
        let header = Self {
            used_bytes: layout.end,
            flags: if full_snapshot {
                FRAME_FLAG_FULL_SNAPSHOT
            } else {
                0
            },
            frame_sequence,
            generation,
            columns,
            rows,
            cell_count,
            cells_offset: layout.cells_offset,
            cluster_count,
            clusters_offset: layout.clusters_offset,
            blob_bytes,
            blob_offset: layout.blob_offset,
            damage_count,
            damage_offset: layout.damage_offset,
            cursor_column,
            cursor_row,
            cursor_flags,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn is_full_snapshot(&self) -> bool {
        self.flags & FRAME_FLAG_FULL_SNAPSHOT != 0
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_flags & CURSOR_FLAG_VISIBLE != 0
    }

    pub fn encode(&self) -> Result<[u8; FRAME_HEADER_BYTES as usize], NativeGridError> {
        self.validate()?;
        let mut out = [0u8; FRAME_HEADER_BYTES as usize];
        out[0..4].copy_from_slice(&self.used_bytes.to_le_bytes());
        out[4..6].copy_from_slice(&FRAME_HEADER_BYTES.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..16].copy_from_slice(&self.frame_sequence.to_le_bytes());
        out[16..24].copy_from_slice(&self.generation.to_le_bytes());
        for (offset, value) in [
            (24, self.columns),
            (28, self.rows),
            (32, self.cell_count),
            (36, self.cells_offset),
            (40, self.cluster_count),
            (44, self.clusters_offset),
            (48, self.blob_bytes),
            (52, self.blob_offset),
            (56, self.damage_count),
            (60, self.damage_offset),
            (64, self.cursor_column),
            (68, self.cursor_row),
            (72, self.cursor_flags),
        ] {
            out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        Ok(out)
    }

    /// Decode and validate the 128-byte header at the start of a slot. This
    /// checks the header alone; cells, clusters and damage are validated by
    /// the renderer slice.
    pub fn decode(bytes: &[u8]) -> Result<Self, NativeGridError> {
        if bytes.len() != usize::from(FRAME_HEADER_BYTES) {
            return Err(NativeGridError::Length);
        }
        if u16_at(bytes, 4) != FRAME_HEADER_BYTES {
            return Err(NativeGridError::FrameField(FrameField::HeaderBytes));
        }
        if bytes[76..128].iter().any(|byte| *byte != 0) {
            return Err(NativeGridError::FrameField(FrameField::Reserved));
        }
        let header = Self {
            used_bytes: u32_at(bytes, 0),
            flags: u16_at(bytes, 6),
            frame_sequence: u64_at(bytes, 8),
            generation: u64_at(bytes, 16),
            columns: u32_at(bytes, 24),
            rows: u32_at(bytes, 28),
            cell_count: u32_at(bytes, 32),
            cells_offset: u32_at(bytes, 36),
            cluster_count: u32_at(bytes, 40),
            clusters_offset: u32_at(bytes, 44),
            blob_bytes: u32_at(bytes, 48),
            blob_offset: u32_at(bytes, 52),
            damage_count: u32_at(bytes, 56),
            damage_offset: u32_at(bytes, 60),
            cursor_column: u32_at(bytes, 64),
            cursor_row: u32_at(bytes, 68),
            cursor_flags: u32_at(bytes, 72),
        };
        header.validate()?;
        Ok(header)
    }

    /// Rules the header must satisfy on its own. The current committed
    /// sequence and control generation are checked by the consumer.
    pub fn validate(&self) -> Result<(), NativeGridError> {
        let bad = |field| Err(NativeGridError::FrameField(field));
        if self.flags & !FRAME_FLAG_FULL_SNAPSHOT != 0 {
            return bad(FrameField::Flags);
        }
        if self.frame_sequence == 0 {
            return bad(FrameField::Sequence);
        }
        if self.generation == 0 {
            return bad(FrameField::Generation);
        }
        if !(1..=MAX_FRAME_AXIS).contains(&self.columns)
            || !(1..=MAX_FRAME_AXIS).contains(&self.rows)
        {
            return bad(FrameField::Geometry);
        }
        if u64::from(self.columns) * u64::from(self.rows) != u64::from(self.cell_count)
            || self.cell_count > MAX_FRAME_CELLS
        {
            return bad(FrameField::CellCount);
        }
        if self.cluster_count > self.cell_count {
            return bad(FrameField::ClusterCount);
        }
        if self.blob_bytes > MAX_FRAME_BLOB_BYTES {
            return bad(FrameField::BlobBytes);
        }
        let damage_limit = (self.rows * 2).min(MAX_FRAME_DAMAGE_RECORDS);
        if self.damage_count > damage_limit || (self.is_full_snapshot() && self.damage_count != 0) {
            return bad(FrameField::DamageCount);
        }
        let Some(layout) = FrameLayout::canonical(
            self.cell_count,
            self.cluster_count,
            self.blob_bytes,
            self.damage_count,
        ) else {
            return bad(FrameField::Offsets);
        };
        if (
            self.cells_offset,
            self.clusters_offset,
            self.blob_offset,
            self.damage_offset,
        ) != (
            layout.cells_offset,
            layout.clusters_offset,
            layout.blob_offset,
            layout.damage_offset,
        ) {
            return bad(FrameField::Offsets);
        }
        if self.used_bytes < layout.end || self.used_bytes > RING_SLOT_STRIDE {
            return bad(FrameField::UsedBytes);
        }
        match self.cursor_flags {
            0 if self.cursor_column == 0 && self.cursor_row == 0 => {}
            CURSOR_FLAG_VISIBLE
                if self.cursor_column < self.columns && self.cursor_row < self.rows => {}
            _ => return bad(FrameField::Cursor),
        }
        Ok(())
    }
}

fn round_up_8(value: u32) -> Option<u32> {
    value.checked_add(7).map(|value| value & !7)
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
#[path = "native_grid_tests.rs"]
mod tests;
