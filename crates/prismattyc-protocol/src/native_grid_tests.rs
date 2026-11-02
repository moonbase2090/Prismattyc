use super::*;

/// Bytes from a hex string; spaces and newlines are ignored.
fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert_eq!(digits.len() % 2, 0, "odd hex length");
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

const OFFER: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

fn record(session_id: u64, sequence: u64, message: ControlMessage) -> ControlRecord {
    ControlRecord {
        session_id,
        sequence,
        generation: 7,
        message,
    }
}

/// One golden record per row of the contract's message table, written out by
/// hand from the table: total, opcode, flags, session, sequence, generation,
/// then the body.
fn goldens() -> Vec<(ControlRecord, Vec<u8>)> {
    vec![
        (
            record(
                0,
                1,
                ControlMessage::Hello {
                    capability_id: 17,
                    abi_major: 1,
                    abi_minor: 0,
                    offer: OFFER,
                },
            ),
            hex(
                "38000000 0100 0000 0000000000000000 0100000000000000 0700000000000000
                 11000000 0100 0000 0123456789abcdef0123456789abcdef",
            ),
        ),
        (
            record(
                42,
                1,
                ControlMessage::Setup {
                    mapping_bytes: 4_198_400,
                    slots: 4,
                    stride: 1_048_576,
                },
            ),
            hex(
                "30000000 0200 0000 2a00000000000000 0100000000000000 0700000000000000
                 0010400000000000 04000000 00001000",
            ),
        ),
        (
            record(42, 2, ControlMessage::Activate { frame_sequence: 1 }),
            hex(
                "28000000 0300 0000 2a00000000000000 0200000000000000 0700000000000000
                 0100000000000000",
            ),
        ),
        (
            record(42, 2, ControlMessage::Active { frame_sequence: 1 }),
            hex(
                "28000000 0400 0000 2a00000000000000 0200000000000000 0700000000000000
                 0100000000000000",
            ),
        ),
        (
            record(42, 3, ControlMessage::Release),
            hex("20000000 0500 0000 2a00000000000000 0300000000000000 0700000000000000"),
        ),
        (
            record(42, 3, ControlMessage::Released),
            hex("20000000 0600 0000 2a00000000000000 0300000000000000 0700000000000000"),
        ),
        (
            record(42, 4, ControlMessage::Ping),
            hex("20000000 0700 0000 2a00000000000000 0400000000000000 0700000000000000"),
        ),
        (
            record(42, 4, ControlMessage::Pong),
            hex("20000000 0800 0000 2a00000000000000 0400000000000000 0700000000000000"),
        ),
    ]
}

#[test]
fn every_message_in_the_contract_table_matches_its_golden_bytes() {
    let goldens = goldens();
    assert_eq!(goldens.len(), 8, "one golden per opcode");
    for (record, bytes) in goldens {
        let opcode = record.message.opcode();
        assert_eq!(bytes.len(), opcode.record_bytes(), "{opcode:?} size");
        assert_eq!(encode_control(&record).unwrap(), bytes, "{opcode:?} encode");
        assert_eq!(decode_control(&bytes).unwrap(), record, "{opcode:?} decode");
        let header: [u8; CONTROL_HEADER_BYTES] = bytes[..CONTROL_HEADER_BYTES].try_into().unwrap();
        assert_eq!(control_record_len(&header).unwrap(), bytes.len());
    }
}

#[test]
fn the_message_table_sizes_and_directions_match_the_contract() {
    let table = [
        (Opcode::Hello, 56, true),
        (Opcode::Setup, 48, false),
        (Opcode::Activate, 40, true),
        (Opcode::Active, 40, false),
        (Opcode::Release, 32, true),
        (Opcode::Released, 32, false),
        (Opcode::Ping, 32, true),
        (Opcode::Pong, 32, false),
    ];
    for (opcode, bytes, request) in table {
        assert_eq!(opcode.record_bytes(), bytes, "{opcode:?}");
        assert_eq!(opcode.is_request(), request, "{opcode:?}");
        assert_eq!(Opcode::from_u16(opcode as u16), Some(opcode));
    }
    assert_eq!(Opcode::from_u16(0), None);
    assert_eq!(Opcode::from_u16(9), None);
    assert_eq!(RING_MAPPING_BYTES, 4_198_400);
}

#[test]
fn any_length_that_is_not_exact_is_rejected() {
    for (_, bytes) in goldens() {
        for cut in 0..bytes.len() {
            assert!(decode_control(&bytes[..cut]).is_err(), "truncated to {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode_control(&longer), Err(NativeGridError::Length));
        // A header that claims one size while the buffer holds another.
        let mut claimed = bytes.clone();
        claimed[0..4].copy_from_slice(&((bytes.len() as u32) + 8).to_le_bytes());
        assert_eq!(decode_control(&claimed), Err(NativeGridError::Length));
        // A self-consistent record whose size does not match its opcode.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 8]);
        let padded_len = padded.len() as u32;
        padded[0..4].copy_from_slice(&padded_len.to_le_bytes());
        assert_eq!(decode_control(&padded), Err(NativeGridError::RecordSize));
    }
}

#[test]
fn record_length_prefix_is_bounded() {
    let mut header = [0u8; CONTROL_HEADER_BYTES];
    for (total, ok) in [
        (0u32, false),
        (31, false),
        (32, true),
        (4096, true),
        (4097, false),
    ] {
        header[0..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(control_record_len(&header).is_ok(), ok, "{total}");
    }
}

#[test]
fn header_flags_opcode_sequence_and_generation_are_checked() {
    let (_, ping) = goldens().pop().unwrap();
    for flag in [1u16, 0x8000] {
        let mut bytes = ping.clone();
        bytes[6..8].copy_from_slice(&flag.to_le_bytes());
        assert_eq!(decode_control(&bytes), Err(NativeGridError::Flags));
    }
    for opcode in [0u16, 9, 0xffff] {
        let mut bytes = ping.clone();
        bytes[4..6].copy_from_slice(&opcode.to_le_bytes());
        assert_eq!(decode_control(&bytes), Err(NativeGridError::Opcode));
    }
    let mut bytes = ping.clone();
    bytes[16..24].fill(0);
    assert_eq!(decode_control(&bytes), Err(NativeGridError::Sequence));
    let mut bytes = ping.clone();
    bytes[24..32].fill(0);
    assert_eq!(decode_control(&bytes), Err(NativeGridError::Generation));
    let mut bytes = ping;
    bytes[8..16].fill(0);
    assert_eq!(decode_control(&bytes), Err(NativeGridError::Session));
}

#[test]
fn hello_and_setup_carry_the_fixed_contract_values() {
    let hello = |session_id, capability_id, abi_major, abi_minor| {
        record(
            session_id,
            1,
            ControlMessage::Hello {
                capability_id,
                abi_major,
                abi_minor,
                offer: OFFER,
            },
        )
    };
    assert_eq!(
        encode_control(&hello(5, 17, 1, 0)),
        Err(NativeGridError::Session),
        "HELLO has session zero"
    );
    assert_eq!(
        encode_control(&hello(0, 0, 1, 0)),
        Err(NativeGridError::CapabilityId)
    );
    assert_eq!(
        encode_control(&hello(0, 17, 2, 0)),
        Err(NativeGridError::ContractValue)
    );
    assert_eq!(
        encode_control(&hello(0, 17, 1, 1)),
        Err(NativeGridError::ContractValue)
    );
    let setup = |session_id, mapping_bytes, slots, stride| {
        record(
            session_id,
            1,
            ControlMessage::Setup {
                mapping_bytes,
                slots,
                stride,
            },
        )
    };
    assert_eq!(
        encode_control(&setup(0, RING_MAPPING_BYTES, 4, RING_SLOT_STRIDE)),
        Err(NativeGridError::Session),
        "SETUP assigns a nonzero session"
    );
    for (mapping, slots, stride) in [
        (RING_MAPPING_BYTES - 1, 4, RING_SLOT_STRIDE),
        (RING_MAPPING_BYTES, 3, RING_SLOT_STRIDE),
        (RING_MAPPING_BYTES, 4, RING_SLOT_STRIDE / 2),
    ] {
        assert_eq!(
            encode_control(&setup(9, mapping, slots, stride)),
            Err(NativeGridError::ContractValue)
        );
    }
}

/// Deterministic xorshift so property runs are reproducible without a
/// dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn nonzero(&mut self) -> u64 {
        self.next().max(1)
    }
}

fn random_record(rng: &mut Rng) -> ControlRecord {
    let session = rng.nonzero();
    let message = match rng.below(8) {
        0 => {
            let mut offer = [0u8; 16];
            for byte in &mut offer {
                *byte = rng.next() as u8;
            }
            return ControlRecord {
                session_id: 0,
                sequence: rng.nonzero(),
                generation: rng.nonzero(),
                message: ControlMessage::Hello {
                    capability_id: (rng.next() as u32).max(1),
                    abi_major: 1,
                    abi_minor: 0,
                    offer,
                },
            };
        }
        1 => ControlMessage::Setup {
            mapping_bytes: RING_MAPPING_BYTES,
            slots: RING_SLOTS,
            stride: RING_SLOT_STRIDE,
        },
        2 => ControlMessage::Activate {
            frame_sequence: rng.next(),
        },
        3 => ControlMessage::Active {
            frame_sequence: rng.next(),
        },
        4 => ControlMessage::Release,
        5 => ControlMessage::Released,
        6 => ControlMessage::Ping,
        _ => ControlMessage::Pong,
    };
    ControlRecord {
        session_id: session,
        sequence: rng.nonzero(),
        generation: rng.nonzero(),
        message,
    }
}

#[test]
fn property_valid_records_round_trip_and_any_single_change_is_caught_or_preserved() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..5_000 {
        let record = random_record(&mut rng);
        let bytes = encode_control(&record).unwrap();
        assert_eq!(bytes.len(), record.message.opcode().record_bytes());
        assert_eq!(decode_control(&bytes).unwrap(), record);
        // Flip one random bit: the result is either rejected or decodes to a
        // record that re-encodes to exactly those bytes. Nothing is silently
        // normalized.
        let mut flipped = bytes.clone();
        let index = rng.below(flipped.len() as u64) as usize;
        flipped[index] ^= 1 << rng.below(8);
        if let Ok(decoded) = decode_control(&flipped) {
            assert_eq!(encode_control(&decoded).unwrap(), flipped);
        }
    }
}

fn frame(columns: u32, rows: u32) -> FrameHeader {
    FrameHeader::new(true, 1, 7, columns, rows, 0, 0, 0, None).unwrap()
}

/// The golden frame header is assembled from the contract's offset table, not
/// from the encoder.
#[test]
fn frame_header_matches_its_golden_layout() {
    let header = FrameHeader::new(true, 1, 7, 80, 24, 2, 5, 0, Some((3, 1))).unwrap();
    let mut golden = [0u8; 128];
    let mut put =
        |offset: usize, bytes: &[u8]| golden[offset..offset + bytes.len()].copy_from_slice(bytes);
    // 80 × 24 = 1920 cells. Cells end at 128 + 1920 × 16 = 30848. Two cluster
    // records end at 30880, five blob bytes at 30885, and damage starts at
    // the next multiple of eight, 30888.
    put(0, &30_888u32.to_le_bytes());
    put(4, &128u16.to_le_bytes());
    put(6, &1u16.to_le_bytes());
    put(8, &1u64.to_le_bytes());
    put(16, &7u64.to_le_bytes());
    put(24, &80u32.to_le_bytes());
    put(28, &24u32.to_le_bytes());
    put(32, &1920u32.to_le_bytes());
    put(36, &128u32.to_le_bytes());
    put(40, &2u32.to_le_bytes());
    put(44, &30_848u32.to_le_bytes());
    put(48, &5u32.to_le_bytes());
    put(52, &30_880u32.to_le_bytes());
    put(56, &0u32.to_le_bytes());
    put(60, &30_888u32.to_le_bytes());
    put(64, &3u32.to_le_bytes());
    put(68, &1u32.to_le_bytes());
    put(72, &1u32.to_le_bytes());
    assert_eq!(header.encode().unwrap(), golden);
    assert_eq!(FrameHeader::decode(&golden).unwrap(), header);
    assert!(header.is_full_snapshot());
    assert!(header.cursor_visible());
}

#[test]
fn frame_geometry_and_cell_limits_follow_the_contract() {
    assert!(
        FrameHeader::new(true, 1, 7, 256, 128, 0, 0, 0, None).is_ok(),
        "32768 cells"
    );
    for (columns, rows) in [(0, 1), (1, 0), (4097, 1), (1, 4097)] {
        assert!(FrameHeader::new(true, 1, 7, columns, rows, 0, 0, 0, None).is_err());
    }
    // Each axis is within its ceiling, but the product is not.
    assert_eq!(
        FrameHeader::new(true, 1, 7, 4096, 4096, 0, 0, 0, None),
        Err(NativeGridError::FrameField(FrameField::CellCount))
    );
    assert_eq!(
        FrameHeader::new(true, 1, 7, 32_769, 1, 0, 0, 0, None),
        Err(NativeGridError::FrameField(FrameField::Geometry))
    );
    let mut header = frame(10, 10);
    header.cell_count = 99;
    assert_eq!(
        header.encode(),
        Err(NativeGridError::FrameField(FrameField::CellCount))
    );
}

#[test]
fn frame_sections_must_fit_one_slot_with_canonical_offsets() {
    // 32768 cells and 32768 cluster records need 1 MiB of arrays plus the
    // header, so the frame cannot fit a slot.
    assert_eq!(
        FrameHeader::new(true, 1, 7, 256, 128, 32_768, 0, 0, None),
        Err(NativeGridError::FrameField(FrameField::Offsets))
    );
    let mut header = FrameHeader::new(true, 1, 7, 80, 24, 2, 5, 0, None).unwrap();
    for change in [
        |h: &mut FrameHeader| h.cells_offset += 8,
        |h: &mut FrameHeader| h.clusters_offset += 8,
        |h: &mut FrameHeader| h.blob_offset -= 8,
        |h: &mut FrameHeader| h.damage_offset += 1,
    ] {
        let mut changed = header;
        change(&mut changed);
        assert_eq!(
            changed.encode(),
            Err(NativeGridError::FrameField(FrameField::Offsets))
        );
    }
    header.used_bytes -= 1;
    assert_eq!(
        header.encode(),
        Err(NativeGridError::FrameField(FrameField::UsedBytes))
    );
    header.used_bytes = RING_SLOT_STRIDE + 1;
    assert_eq!(
        header.encode(),
        Err(NativeGridError::FrameField(FrameField::UsedBytes))
    );
    header.used_bytes = RING_SLOT_STRIDE;
    assert!(
        header.encode().is_ok(),
        "trailing unused slot bytes are allowed"
    );
}

#[test]
fn frame_counts_flags_and_cursor_are_checked() {
    let cases: [(FrameHeader, FrameField); 8] = [
        (
            FrameHeader {
                flags: 2,
                ..frame(4, 4)
            },
            FrameField::Flags,
        ),
        (
            FrameHeader {
                frame_sequence: 0,
                ..frame(4, 4)
            },
            FrameField::Sequence,
        ),
        (
            FrameHeader {
                generation: 0,
                ..frame(4, 4)
            },
            FrameField::Generation,
        ),
        (
            FrameHeader {
                cluster_count: 17,
                ..frame(4, 4)
            },
            FrameField::ClusterCount,
        ),
        (
            FrameHeader {
                blob_bytes: MAX_FRAME_BLOB_BYTES + 1,
                ..frame(4, 4)
            },
            FrameField::BlobBytes,
        ),
        (
            FrameHeader {
                cursor_column: 1,
                ..frame(4, 4)
            },
            FrameField::Cursor,
        ),
        (
            FrameHeader {
                cursor_flags: CURSOR_FLAG_VISIBLE,
                cursor_column: 4,
                ..frame(4, 4)
            },
            FrameField::Cursor,
        ),
        (
            FrameHeader {
                cursor_flags: 2,
                ..frame(4, 4)
            },
            FrameField::Cursor,
        ),
    ];
    for (header, field) in cases {
        assert_eq!(
            header.validate(),
            Err(NativeGridError::FrameField(field)),
            "{field:?}"
        );
    }
    // A full snapshot carries no damage records. A partial frame may carry
    // up to two per row.
    assert_eq!(
        FrameHeader::new(true, 2, 7, 4, 4, 0, 0, 1, None),
        Err(NativeGridError::FrameField(FrameField::DamageCount))
    );
    assert!(FrameHeader::new(false, 2, 7, 4, 4, 0, 0, 8, None).is_ok());
    assert_eq!(
        FrameHeader::new(false, 2, 7, 4, 4, 0, 0, 9, None),
        Err(NativeGridError::FrameField(FrameField::DamageCount))
    );
    assert!(
        FrameHeader::new(false, 2, 7, 4, 4, 0, 0, 0, None).is_ok(),
        "cursor-only frame"
    );
}

#[test]
fn frame_decoder_rejects_reserved_bytes_and_wrong_sizes() {
    let bytes = frame(10, 3).encode().unwrap();
    for index in 76..128 {
        let mut changed = bytes;
        changed[index] = 1;
        assert_eq!(
            FrameHeader::decode(&changed),
            Err(NativeGridError::FrameField(FrameField::Reserved)),
            "reserved byte {index}"
        );
    }
    let mut changed = bytes;
    changed[4..6].copy_from_slice(&64u16.to_le_bytes());
    assert_eq!(
        FrameHeader::decode(&changed),
        Err(NativeGridError::FrameField(FrameField::HeaderBytes))
    );
    assert_eq!(
        FrameHeader::decode(&bytes[..127]),
        Err(NativeGridError::Length)
    );
    assert_eq!(
        FrameHeader::decode(&[bytes.as_slice(), &[0]].concat()),
        Err(NativeGridError::Length)
    );
}

#[test]
fn property_valid_frame_headers_round_trip() {
    let mut rng = Rng(0xd1b5_4a32_d192_ed03);
    let mut checked = 0;
    while checked < 5_000 {
        let columns = 1 + rng.below(u64::from(MAX_FRAME_AXIS)) as u32;
        let rows = 1 + rng.below(u64::from(MAX_FRAME_CELLS / columns).clamp(1, 4096)) as u32;
        let cells = columns * rows;
        let full = rng.below(2) == 0;
        let clusters = rng.below(u64::from(cells) + 1) as u32;
        let blob = rng.below(u64::from(MAX_FRAME_BLOB_BYTES) + 1) as u32;
        let damage = if full {
            0
        } else {
            rng.below(u64::from((rows * 2).min(MAX_FRAME_DAMAGE_RECORDS)) + 1) as u32
        };
        let cursor = (rng.below(2) == 0).then(|| {
            (
                rng.below(u64::from(columns)) as u32,
                rng.below(u64::from(rows)) as u32,
            )
        });
        let Ok(header) = FrameHeader::new(
            full,
            rng.nonzero(),
            rng.nonzero(),
            columns,
            rows,
            clusters,
            blob,
            damage,
            cursor,
        ) else {
            // Only a frame too large for one slot may be refused here.
            assert!(FrameLayout::canonical(cells, clusters, blob, damage).is_none());
            continue;
        };
        let bytes = header.encode().unwrap();
        assert_eq!(FrameHeader::decode(&bytes).unwrap(), header);
        assert!(header.used_bytes <= RING_SLOT_STRIDE);
        checked += 1;
    }
}
